use core::any::Any;

use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::lazy_static;
use strum_macros::FromRepr;

use crate::{
    fs::{open_file_at, unlinkat, File, FileDescription, OpenFlags, Pipe, Stat, StatMode},
    mm::UserBuffer,
    poll::notify_poll_source,
    sync::{Mutex, MutexBlocking, SpinNoIrqLock},
    syscall::errno::ERRNO,
    task::{current_process, WaitQueue, WaitReason},
};

const POLLIN: u16 = 0x001;
const POLLOUT: u16 = 0x004;
const POLLHUP: u16 = 0x010;

/// pass file descriptors through UNIX domain sockets.
pub const SCM_RIGHTS: i32 = 1;
/// pass peer credentials through UNIX domain sockets.
pub const SCM_CREDENTIALS: i32 = 2;

#[repr(i32)]
#[derive(FromRepr)]
#[allow(missing_docs)]
pub enum SocketLevel {
    IpProtoIp = 0,
    SolSocket = 1,
    IpProtoTcp = 6,
    IpProtoIpv6 = 41,
}

/// Userspace-compatible credential payload for `SCM_CREDENTIALS`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UnixUcred {
    /// sender process id.
    pub pid: i32,
    /// sender real/effective user id (MVP uses process uid).
    pub uid: u32,
    /// sender real/effective group id (MVP uses process gid).
    pub gid: u32,
}

/// In-kernel ancillary payload associated with one logical write message.
#[derive(Default)]
pub struct UnixSocketAncillaryData {
    /// file descriptions transferred by `SCM_RIGHTS`.
    pub rights: Vec<Arc<FileDescription>>,
    /// sender credentials transferred by `SCM_CREDENTIALS`.
    pub credentials: Option<UnixUcred>,
}

impl UnixSocketAncillaryData {
    /// whether this ancillary payload is empty.
    pub fn is_empty(&self) -> bool {
        self.rights.is_empty() && self.credentials.is_none()
    }
}

struct UnixStreamFrameMeta {
    remaining: usize,
    rights: Vec<Arc<FileDescription>>,
    credentials: Option<UnixUcred>,
}

struct UnixSocketPairLocalState {
    tx: Option<Arc<Pipe>>,
    peer: Option<Weak<UnixSocketPairEnd>>,
    read_shutdown: bool,
    write_shutdown: bool,
    passcred: bool,
    attached_bpf_prog_fd: Option<u32>,
    bound_addr: Option<Vec<u8>>,
    listening: bool,
    pending: VecDeque<Arc<UnixSocketPairEnd>>,
}

/// 使用两条单向 pipe 交叉组合为一个全双工端点。
pub struct UnixSocketPairEnd {
    rx: Arc<Pipe>,
    state: SpinNoIrqLock<UnixSocketPairLocalState>,
    /// 入方向（peer->self）消息边界与控制消息元数据。
    rx_meta: Arc<SpinNoIrqLock<VecDeque<UnixStreamFrameMeta>>>,
    /// 出方向（self->peer）消息边界与控制消息元数据。
    tx_meta: Arc<SpinNoIrqLock<VecDeque<UnixStreamFrameMeta>>>,
    /// 串行化 read/recvmsg，保证与 rx_meta 的消费顺序一致。
    rx_seq_lock: MutexBlocking,
    /// 串行化 write/sendmsg，保证与 tx_meta 的入队顺序一致。
    tx_seq_lock: MutexBlocking,
}

impl UnixSocketPairEnd {
    fn new_internal(
        rx: Arc<Pipe>,
        tx: Arc<Pipe>,
        rx_meta: Arc<SpinNoIrqLock<VecDeque<UnixStreamFrameMeta>>>,
        tx_meta: Arc<SpinNoIrqLock<VecDeque<UnixStreamFrameMeta>>>,
    ) -> Self {
        Self {
            rx,
            state: SpinNoIrqLock::new(UnixSocketPairLocalState {
                tx: Some(tx),
                peer: None,
                read_shutdown: false,
                write_shutdown: false,
                passcred: false,
                attached_bpf_prog_fd: None,
                bound_addr: None,
                listening: false,
                pending: VecDeque::new(),
            }),
            rx_meta,
            tx_meta,
            rx_seq_lock: MutexBlocking::new(),
            tx_seq_lock: MutexBlocking::new(),
        }
    }

    /// 使用两条方向 pipe 创建一对互联 UNIX socket 端点。
    pub(crate) fn new_pair(
        end0_rx: Arc<Pipe>,
        end0_tx: Arc<Pipe>,
        end1_rx: Arc<Pipe>,
        end1_tx: Arc<Pipe>,
    ) -> (Arc<Self>, Arc<Self>) {
        let ab_meta = Arc::new(SpinNoIrqLock::new(VecDeque::new()));
        let ba_meta = Arc::new(SpinNoIrqLock::new(VecDeque::new()));

        let end0 = Arc::new(Self::new_internal(
            end0_rx,
            end0_tx,
            ba_meta.clone(),
            ab_meta.clone(),
        ));
        let end1 = Arc::new(Self::new_internal(end1_rx, end1_tx, ab_meta, ba_meta));
        end0.set_peer(&end1);
        end1.set_peer(&end0);
        (end0, end1)
    }

    /// Bind this socket to a UNIX-domain address.
    pub(crate) fn bind_addr(&self, addr: Vec<u8>, create_path: bool) -> Result<(), ERRNO> {
        {
            let state = self.state.lock();
            if state.bound_addr.is_some() {
                return Err(ERRNO::EINVAL);
            }
        }

        if create_path {
            let path = unix_path_from_addr(&addr)?;
            let cwd = current_process().inner_exclusive_access().cwd.clone();
            open_file_at(
                cwd.as_str(),
                path.as_str(),
                OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::RDWR,
            )
            .map_err(|err| match err {
                ERRNO::EIO => ERRNO::ENOTDIR,
                ERRNO::EEXIST => ERRNO::EADDRINUSE,
                other => other,
            })?;
        }

        let mut registry = UNIX_REGISTRY.lock();
        if registry.stream.contains_key(&addr) || registry.datagram.contains_key(&addr) {
            if create_path {
                if let Ok(path) = unix_path_from_addr(&addr) {
                    let cwd = current_process().inner_exclusive_access().cwd.clone();
                    let _ = unlinkat(cwd.as_str(), path.as_str(), 0);
                }
            }
            return Err(ERRNO::EADDRINUSE);
        }
        registry
            .stream
            .insert(addr.clone(), self as *const Self as usize);
        self.state.lock().bound_addr = Some(addr);
        Ok(())
    }

    /// Mark a bound stream socket as a listener.
    pub(crate) fn listen(&self) -> Result<(), ERRNO> {
        let mut state = self.state.lock();
        if state.bound_addr.is_none() {
            return Err(ERRNO::EINVAL);
        }
        state.listening = true;
        Ok(())
    }

    pub(crate) fn bound_addr(&self) -> Option<Vec<u8>> {
        self.state.lock().bound_addr.clone()
    }

    /// Queue an accepted stream socket for `accept(2)`.
    pub(crate) fn push_pending(&self, socket: Arc<UnixSocketPairEnd>) -> Result<(), ERRNO> {
        let mut state = self.state.lock();
        if !state.listening {
            return Err(ERRNO::ECONNREFUSED);
        }
        state.pending.push_back(socket);
        Ok(())
    }

    /// Pop one accepted stream socket if available.
    pub(crate) fn pop_pending(&self) -> Option<Arc<UnixSocketPairEnd>> {
        self.state.lock().pending.pop_front()
    }

    fn set_peer(&self, peer: &Arc<Self>) {
        self.state.lock().peer = Some(Arc::downgrade(peer));
    }

    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn notify_self(&self, ready_mask: u16) {
        notify_poll_source(self.source_id(), ready_mask);
    }

    fn notify_peer(&self, ready_mask: u16) {
        let peer = self.state.lock().peer.clone();
        if let Some(peer) = peer.and_then(|peer| peer.upgrade()) {
            peer.notify_self(ready_mask);
        }
    }

    fn consume_rx_meta(
        &self,
        mut consumed: usize,
        collect_ancillary: bool,
    ) -> UnixSocketAncillaryData {
        let mut ancillary = UnixSocketAncillaryData::default();
        let mut queue = self.rx_meta.lock();

        while consumed > 0 {
            let Some(front) = queue.front_mut() else {
                break;
            };

            if front.remaining == 0 {
                queue.pop_front();
                continue;
            }

            let take = front.remaining.min(consumed);
            if take == 0 {
                break;
            }

            // 流语义下，控制消息附着在该 frame 的“首个被消费字节”上。
            if collect_ancillary {
                if !front.rights.is_empty() {
                    ancillary.rights.append(&mut front.rights);
                }
                if ancillary.credentials.is_none() {
                    ancillary.credentials = front.credentials.take();
                }
            } else {
                front.rights.clear();
                front.credentials = None;
            }

            front.remaining -= take;
            consumed -= take;

            if front.remaining == 0 {
                queue.pop_front();
            }
        }

        ancillary
    }

    fn write_with_ancillary(
        &self,
        buf: UserBuffer,
        ancillary: UnixSocketAncillaryData,
        strict_shutdown: bool,
    ) -> Result<usize, ERRNO> {
        let data_len = buf.len();
        if data_len == 0 {
            if strict_shutdown && !ancillary.is_empty() {
                return Err(ERRNO::EINVAL);
            }
            return Ok(0);
        }

        self.tx_seq_lock.lock();

        let tx = {
            let state = self.state.lock();
            if state.write_shutdown || state.tx.is_none() {
                if strict_shutdown {
                    self.tx_seq_lock.unlock();
                    return Err(ERRNO::ESHUTDOWN);
                }
                self.tx_seq_lock.unlock();
                return Ok(0);
            }
            state.tx.as_ref().cloned().unwrap()
        };

        let written = tx.write_at(0, buf);
        if written == 0 && tx.write_peer_closed() {
            self.tx_seq_lock.unlock();
            return Err(ERRNO::EPIPE);
        }
        if written > 0 {
            if let Err(err) = self.run_peer_bpf_filter() {
                self.tx_seq_lock.unlock();
                return Err(err);
            }
            self.tx_meta.lock().push_back(UnixStreamFrameMeta {
                remaining: written,
                rights: ancillary.rights,
                credentials: ancillary.credentials,
            });
            self.notify_peer(POLLIN);
        }

        self.tx_seq_lock.unlock();
        Ok(written)
    }

    /// `sendmsg` 路径：支持附带 `SCM_RIGHTS/SCM_CREDENTIALS` 的流式发送。
    pub fn sendmsg(
        &self,
        buf: UserBuffer,
        ancillary: UnixSocketAncillaryData,
    ) -> Result<usize, ERRNO> {
        if buf.len() == 0 && !ancillary.is_empty() {
            // MVP：避免“无负载仅控制消息”语义歧义。
            return Err(ERRNO::EINVAL);
        }
        self.write_with_ancillary(buf, ancillary, true)
    }

    /// `recvmsg` 路径：读取流数据并回收/交付对应控制消息。
    pub fn recvmsg(&self, buf: UserBuffer) -> Result<(usize, UnixSocketAncillaryData), ERRNO> {
        {
            let state = self.state.lock();
            if state.read_shutdown {
                return Ok((0, UnixSocketAncillaryData::default()));
            }
        }

        self.rx_seq_lock.lock();

        {
            let state = self.state.lock();
            if state.read_shutdown {
                self.rx_seq_lock.unlock();
                return Ok((0, UnixSocketAncillaryData::default()));
            }
        }

        let read_len = self.rx.read_at(0, buf);
        let ancillary = self.consume_rx_meta(read_len, true);
        if read_len > 0 {
            self.notify_peer(POLLOUT);
        }
        self.rx_seq_lock.unlock();
        Ok((read_len, ancillary))
    }

    /// `shutdown(2)` half-close 支持。
    pub fn shutdown(&self, how: i32) -> Result<(), ERRNO> {
        let mut state = self.state.lock();
        match how {
            0 => {
                // SHUT_RD
                state.read_shutdown = true;
                drop(state);
                self.notify_self(POLLHUP);
            }
            1 => {
                // SHUT_WR
                state.write_shutdown = true;
                state.tx.take();
                drop(state);
                self.notify_peer(POLLHUP);
            }
            2 => {
                // SHUT_RDWR
                state.read_shutdown = true;
                state.write_shutdown = true;
                state.tx.take();
                drop(state);
                self.notify_self(POLLHUP);
                self.notify_peer(POLLHUP);
            }
            _ => return Err(ERRNO::EINVAL),
        }
        Ok(())
    }

    /// Enable/disable receiving `SCM_CREDENTIALS` for this endpoint.
    pub fn set_passcred(&self, enabled: bool) {
        self.state.lock().passcred = enabled;
    }

    /// Attach a minimal BPF socket filter target to this receiving endpoint.
    pub fn attach_bpf_prog_fd(&self, prog_fd: u32) {
        self.state.lock().attached_bpf_prog_fd = Some(prog_fd);
    }

    fn run_peer_bpf_filter(&self) -> Result<(), ERRNO> {
        let peer = self.state.lock().peer.clone();
        let Some(peer) = peer.and_then(|peer| peer.upgrade()) else {
            return Ok(());
        };
        let Some(prog_fd) = peer.state.lock().attached_bpf_prog_fd else {
            return Ok(());
        };
        crate::syscall::bpf_run_socket_filter_prog(prog_fd)
    }

    /// Whether receiving `SCM_CREDENTIALS` is enabled on this endpoint.
    pub fn passcred_enabled(&self) -> bool {
        self.state.lock().passcred
    }

    /// Whether writing would fail because the peer read side is gone or this
    /// endpoint has been shut down for writing.
    pub fn write_peer_closed(&self) -> bool {
        let state = self.state.lock();
        if state.write_shutdown {
            return true;
        }
        state
            .tx
            .as_ref()
            .map(|tx| tx.write_peer_closed())
            .unwrap_or(true)
    }
}

/// 创建一个未连接的 UNIX stream socket 端点。
///
/// 当前仅需要一个可被 socket syscall 返回、并能被识别为 `AF_UNIX`
/// socket 的文件对象；其对端立即丢弃，后续对该端点的收发会表现为
/// 未连接/对端关闭状态。
pub(crate) fn create_unix_stream_socket_file() -> Arc<UnixSocketPairEnd> {
    let (ab_read, ab_write) = crate::fs::make_pipe();
    let (ba_read, ba_write) = crate::fs::make_pipe();
    let (socket, _peer) = UnixSocketPairEnd::new_pair(ba_read, ab_write, ab_read, ba_write);
    socket
}

impl Drop for UnixSocketPairEnd {
    fn drop(&mut self) {
        if let Some(addr) = self.state.lock().bound_addr.clone() {
            UNIX_REGISTRY.lock().stream.remove(&addr);
        }
        self.notify_self(POLLHUP | POLLIN | POLLOUT);
        self.notify_peer(POLLHUP | POLLIN | POLLOUT);
    }
}

struct UnixDatagramMessage {
    from: Option<Vec<u8>>,
    data: Vec<u8>,
}

struct UnixDatagramState {
    bound_addr: Option<Vec<u8>>,
    peer_addr: Option<Vec<u8>>,
    queue: VecDeque<UnixDatagramMessage>,
}

/// Minimal AF_UNIX datagram socket used by local pathname/abstract tests.
pub struct UnixDatagramSocketFile {
    state: SpinNoIrqLock<UnixDatagramState>,
    wait_queue: Arc<WaitQueue>,
}

impl UnixDatagramSocketFile {
    fn new() -> Self {
        Self {
            state: SpinNoIrqLock::new(UnixDatagramState {
                bound_addr: None,
                peer_addr: None,
                queue: VecDeque::new(),
            }),
            wait_queue: Arc::new(WaitQueue::new()),
        }
    }

    pub(crate) fn bind_addr(&self, addr: Vec<u8>, create_path: bool) -> Result<(), ERRNO> {
        {
            let state = self.state.lock();
            if state.bound_addr.is_some() {
                return Err(ERRNO::EINVAL);
            }
        }

        if create_path {
            let path = unix_path_from_addr(&addr)?;
            let cwd = current_process().inner_exclusive_access().cwd.clone();
            open_file_at(
                cwd.as_str(),
                path.as_str(),
                OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::RDWR,
            )
            .map_err(|err| match err {
                ERRNO::EIO => ERRNO::ENOTDIR,
                ERRNO::EEXIST => ERRNO::EADDRINUSE,
                other => other,
            })?;
        }

        let mut registry = UNIX_REGISTRY.lock();
        if registry.stream.contains_key(&addr) || registry.datagram.contains_key(&addr) {
            if create_path {
                if let Ok(path) = unix_path_from_addr(&addr) {
                    let cwd = current_process().inner_exclusive_access().cwd.clone();
                    let _ = unlinkat(cwd.as_str(), path.as_str(), 0);
                }
            }
            return Err(ERRNO::EADDRINUSE);
        }
        registry
            .datagram
            .insert(addr.clone(), self as *const Self as usize);
        self.state.lock().bound_addr = Some(addr);
        Ok(())
    }

    pub(crate) fn connect_addr(&self, addr: Vec<u8>) -> Result<(), ERRNO> {
        if !UNIX_REGISTRY.lock().datagram.contains_key(&addr) {
            return Err(ERRNO::ENOENT);
        }
        self.state.lock().peer_addr = Some(addr);
        Ok(())
    }

    pub(crate) fn bound_addr(&self) -> Option<Vec<u8>> {
        self.state.lock().bound_addr.clone()
    }

    pub(crate) fn send_to(&self, data: &[u8], addr: Option<Vec<u8>>) -> Result<usize, ERRNO> {
        let (dst, src) = {
            let state = self.state.lock();
            let dst = addr
                .or_else(|| state.peer_addr.clone())
                .ok_or(ERRNO::ENOTCONN)?;
            (dst, state.bound_addr.clone())
        };
        let peer_ptr = UNIX_REGISTRY
            .lock()
            .datagram
            .get(&dst)
            .copied()
            .ok_or(ERRNO::ENOENT)?;
        let peer = unsafe { &*(peer_ptr as *const UnixDatagramSocketFile) };
        peer.state.lock().queue.push_back(UnixDatagramMessage {
            from: src,
            data: Vec::from(data),
        });
        peer.wait_queue.wake_one();
        notify_poll_source(peer.source_id(), POLLIN);
        Ok(data.len())
    }

    pub(crate) fn recv_from(&self, buf: UserBuffer) -> Result<(usize, Option<Vec<u8>>), ERRNO> {
        loop {
            if let Some(msg) = self.state.lock().queue.pop_front() {
                let mut written = 0usize;
                for byte_ref in buf.into_iter() {
                    if written == msg.data.len() {
                        break;
                    }
                    unsafe {
                        *byte_ref = msg.data[written];
                    }
                    written += 1;
                }
                return Ok((written, msg.from));
            }
            let wait_queue = Arc::clone(&self.wait_queue);
            wait_queue.wait_with_reason_or_skip(WaitReason::PipeReadable, || {
                !self.state.lock().queue.is_empty() || crate::signal::has_unmasked_pending_signal()
            });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }

    fn source_id(&self) -> usize {
        self as *const Self as usize
    }
}

impl File for UnixDatagramSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        self.recv_from(buf).map(|(n, _)| n).unwrap_or(0)
    }

    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        let mut data = Vec::new();
        for byte_ref in buf.into_iter() {
            data.push(unsafe { *byte_ref });
        }
        self.send_to(&data, None).unwrap_or(0)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0;
        if (events & POLLIN) != 0 && !self.state.lock().queue.is_empty() {
            ready |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::SOCK,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl Drop for UnixDatagramSocketFile {
    fn drop(&mut self) {
        if let Some(addr) = self.state.lock().bound_addr.clone() {
            UNIX_REGISTRY.lock().datagram.remove(&addr);
        }
    }
}

pub(crate) fn create_unix_datagram_socket_file() -> Arc<UnixDatagramSocketFile> {
    Arc::new(UnixDatagramSocketFile::new())
}

pub(crate) fn unix_stream_listener(addr: &[u8]) -> Option<&'static UnixSocketPairEnd> {
    UNIX_REGISTRY
        .lock()
        .stream
        .get(addr)
        .copied()
        .map(|ptr| unsafe { &*(ptr as *const UnixSocketPairEnd) })
}

fn unix_path_from_addr(addr: &[u8]) -> Result<String, ERRNO> {
    if addr.first().copied() == Some(0) {
        return Err(ERRNO::EINVAL);
    }
    core::str::from_utf8(addr)
        .map(String::from)
        .map_err(|_| ERRNO::EINVAL)
}

struct UnixRegistry {
    stream: BTreeMap<Vec<u8>, usize>,
    datagram: BTreeMap<Vec<u8>, usize>,
}

impl UnixRegistry {
    fn new() -> Self {
        Self {
            stream: BTreeMap::new(),
            datagram: BTreeMap::new(),
        }
    }
}

lazy_static! {
    static ref UNIX_REGISTRY: SpinNoIrqLock<UnixRegistry> = SpinNoIrqLock::new(UnixRegistry::new());
}

impl File for UnixSocketPairEnd {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at(&self, offset: usize, buf: UserBuffer) -> usize {
        {
            let state = self.state.lock();
            if state.read_shutdown {
                return 0;
            }
        }

        self.rx_seq_lock.lock();

        {
            let state = self.state.lock();
            if state.read_shutdown {
                self.rx_seq_lock.unlock();
                return 0;
            }
        }

        let read_len = self.rx.read_at(offset, buf);
        self.consume_rx_meta(read_len, false);
        if read_len > 0 {
            self.notify_peer(POLLOUT);
        }
        self.rx_seq_lock.unlock();
        read_len
    }

    fn read_bytes_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        {
            let state = self.state.lock();
            if state.read_shutdown {
                return Ok(0);
            }
        }

        let _seq = self.rx_seq_lock.lock();

        {
            let state = self.state.lock();
            if state.read_shutdown {
                return Ok(0);
            }
        }

        let read_len = self.rx.read_bytes_at(offset, buf)?;
        self.consume_rx_meta(read_len, false);
        Ok(read_len)
    }

    fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        let _ = offset;
        self.write_with_ancillary(buf, UnixSocketAncillaryData::default(), false)
            .unwrap_or(0)
    }

    fn write_bytes_at(&self, offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        let _ = offset;
        let _seq = self.tx_seq_lock.lock();

        let tx = {
            let state = self.state.lock();
            if state.write_shutdown || state.tx.is_none() {
                return Ok(0);
            }
            state.tx.as_ref().cloned().unwrap()
        };

        let written = tx.write_bytes_at(0, buf)?;
        if written > 0 {
            self.run_peer_bpf_filter()?;
            self.tx_meta.lock().push_back(UnixStreamFrameMeta {
                remaining: written,
                rights: Vec::new(),
                credentials: None,
            });
        }
        Ok(written)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        let state = self.state.lock();

        if (events & (POLLIN | POLLHUP)) != 0 {
            if state.read_shutdown {
                ready |= POLLHUP;
            } else {
                ready |= self.rx.poll(events & (POLLIN | POLLHUP));
            }
        }
        if (events & POLLOUT) != 0 && !state.write_shutdown {
            if let Some(tx) = state.tx.as_ref() {
                ready |= tx.poll(events & POLLOUT);
            }
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::SOCK,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}
