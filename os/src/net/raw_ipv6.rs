use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    cmp::min,
};

use lazy_static::lazy_static;

use crate::{
    fs::{File, Stat, StatMode},
    mm::UserBuffer,
    poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT},
    sync::SpinNoIrqLock,
    syscall::errno::ERRNO,
    task::{WaitQueue, WaitReason},
};

pub(crate) const AF_INET6: u16 = 10;
pub(crate) const IPPROTO_ICMPV6: i32 = 58;
pub(crate) const SOL_IPV6: i32 = 41;
pub(crate) const ICMP6_FILTER: i32 = 1;
pub(crate) const IPV6_2292PKTINFO: i32 = 2;
pub(crate) const IPV6_2292HOPOPTS: i32 = 3;
pub(crate) const IPV6_2292DSTOPTS: i32 = 4;
pub(crate) const IPV6_2292RTHDR: i32 = 5;
pub(crate) const IPV6_CHECKSUM: i32 = 7;
pub(crate) const IPV6_2292HOPLIMIT: i32 = 8;
pub(crate) const IPV6_RECVPKTINFO: i32 = 49;
pub(crate) const IPV6_PKTINFO: i32 = 50;
pub(crate) const IPV6_RECVHOPLIMIT: i32 = 51;
pub(crate) const IPV6_HOPLIMIT: i32 = 52;
pub(crate) const IPV6_RECVHOPOPTS: i32 = 53;
pub(crate) const IPV6_RECVRTHDR: i32 = 56;
pub(crate) const IPV6_RECVDSTOPTS: i32 = 58;
pub(crate) const IPV6_RECVTCLASS: i32 = 66;
pub(crate) const IPV6_TCLASS: i32 = 67;

const LOOPBACK_V6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

lazy_static! {
    static ref RAW_IPV6_SOCKETS: SpinNoIrqLock<Vec<Weak<RawIpv6SocketFile>>> =
        SpinNoIrqLock::new(Vec::new());
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SockAddrIn6 {
    pub(crate) sin6_family: u16,
    pub(crate) sin6_port: u16,
    pub(crate) sin6_flowinfo: u32,
    pub(crate) sin6_addr: [u8; 16],
    pub(crate) sin6_scope_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct In6PktInfo {
    pub(crate) ipi6_addr: [u8; 16],
    pub(crate) ipi6_ifindex: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RawIpv6SendMeta {
    pub(crate) hoplimit: Option<i32>,
    pub(crate) tclass: Option<i32>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawIpv6ControlMessage {
    pub(crate) level: i32,
    pub(crate) cmsg_type: i32,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawIpv6RecvPacket {
    pub(crate) data: Vec<u8>,
    pub(crate) control: Vec<RawIpv6ControlMessage>,
}

#[derive(Clone, Copy, Debug)]
struct RawIpv6RecvOptions {
    recv_pktinfo: bool,
    recv_hoplimit: bool,
    recv_rthdr: bool,
    recv_hopopts: bool,
    recv_dstopts: bool,
    recv_tclass: bool,
    recv_2292pktinfo: bool,
    recv_2292hoplimit: bool,
    recv_2292rthdr: bool,
    recv_2292hopopts: bool,
    recv_2292dstopts: bool,
}

impl RawIpv6RecvOptions {
    fn get_bool(self, optname: i32) -> Option<bool> {
        Some(match optname {
            IPV6_RECVPKTINFO => self.recv_pktinfo,
            IPV6_RECVHOPLIMIT => self.recv_hoplimit,
            IPV6_RECVRTHDR => self.recv_rthdr,
            IPV6_RECVHOPOPTS => self.recv_hopopts,
            IPV6_RECVDSTOPTS => self.recv_dstopts,
            IPV6_RECVTCLASS => self.recv_tclass,
            IPV6_2292PKTINFO => self.recv_2292pktinfo,
            IPV6_2292HOPLIMIT => self.recv_2292hoplimit,
            IPV6_2292RTHDR => self.recv_2292rthdr,
            IPV6_2292HOPOPTS => self.recv_2292hopopts,
            IPV6_2292DSTOPTS => self.recv_2292dstopts,
            _ => return None,
        })
    }

    fn set_bool(&mut self, optname: i32, enabled: bool) -> bool {
        match optname {
            IPV6_RECVPKTINFO => self.recv_pktinfo = enabled,
            IPV6_RECVHOPLIMIT => self.recv_hoplimit = enabled,
            IPV6_RECVRTHDR => self.recv_rthdr = enabled,
            IPV6_RECVHOPOPTS => self.recv_hopopts = enabled,
            IPV6_RECVDSTOPTS => self.recv_dstopts = enabled,
            IPV6_RECVTCLASS => self.recv_tclass = enabled,
            IPV6_2292PKTINFO => self.recv_2292pktinfo = enabled,
            IPV6_2292HOPLIMIT => self.recv_2292hoplimit = enabled,
            IPV6_2292RTHDR => self.recv_2292rthdr = enabled,
            IPV6_2292HOPOPTS => self.recv_2292hopopts = enabled,
            IPV6_2292DSTOPTS => self.recv_2292dstopts = enabled,
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Copy, Debug)]
struct Icmp6Filter {
    data: [u32; 8],
}

impl Default for Icmp6Filter {
    fn default() -> Self {
        Self { data: [0; 8] }
    }
}

impl Icmp6Filter {
    fn from_bytes(bytes: &[u8]) -> Result<Self, ERRNO> {
        if bytes.len() < 8 * core::mem::size_of::<u32>() {
            return Err(ERRNO::EINVAL);
        }
        let mut data = [0u32; 8];
        for (idx, slot) in data.iter_mut().enumerate() {
            let base = idx * 4;
            *slot = u32::from_ne_bytes([
                bytes[base],
                bytes[base + 1],
                bytes[base + 2],
                bytes[base + 3],
            ]);
        }
        Ok(Self { data })
    }

    fn as_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (idx, word) in self.data.iter().enumerate() {
            let base = idx * 4;
            out[base..base + 4].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }

    fn allows_type(&self, icmp_type: u8) -> bool {
        let idx = (icmp_type / 32) as usize;
        let bit = icmp_type % 32;
        idx < self.data.len() && (self.data[idx] & (1u32 << bit)) == 0
    }
}

#[derive(Clone, Copy, Debug)]
struct RawIpv6SocketOptions {
    checksum_offset: Option<i32>,
    recv: RawIpv6RecvOptions,
    icmp6_filter: Icmp6Filter,
}

impl Default for RawIpv6SocketOptions {
    fn default() -> Self {
        Self {
            checksum_offset: None,
            recv: RawIpv6RecvOptions {
                recv_pktinfo: false,
                recv_hoplimit: false,
                recv_rthdr: false,
                recv_hopopts: false,
                recv_dstopts: false,
                recv_tclass: false,
                recv_2292pktinfo: false,
                recv_2292hoplimit: false,
                recv_2292rthdr: false,
                recv_2292hopopts: false,
                recv_2292dstopts: false,
            },
            icmp6_filter: Icmp6Filter::default(),
        }
    }
}

pub(crate) struct RawIpv6SocketFile {
    protocol: i32,
    bound_addr: SpinNoIrqLock<Option<[u8; 16]>>,
    options: SpinNoIrqLock<RawIpv6SocketOptions>,
    recv_queue: SpinNoIrqLock<VecDeque<RawIpv6RecvPacket>>,
    read_wait: WaitQueue,
}

impl RawIpv6SocketFile {
    fn new(protocol: i32) -> Self {
        Self {
            protocol,
            bound_addr: SpinNoIrqLock::new(None),
            options: SpinNoIrqLock::new(RawIpv6SocketOptions::default()),
            recv_queue: SpinNoIrqLock::new(VecDeque::new()),
            read_wait: WaitQueue::new(),
        }
    }

    pub(crate) fn bind(&self, addr: &SockAddrIn6) -> Result<(), ERRNO> {
        if addr.sin6_family != AF_INET6 {
            return Err(ERRNO::EAFNOSUPPORT);
        }
        if addr.sin6_scope_id != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }
        if !is_supported_addr(&addr.sin6_addr) {
            return Err(ERRNO::EADDRNOTAVAIL);
        }
        *self.bound_addr.lock() = Some(addr.sin6_addr);
        Ok(())
    }

    pub(crate) fn local_addr(&self) -> SockAddrIn6 {
        SockAddrIn6 {
            sin6_family: AF_INET6,
            sin6_addr: self.bound_addr.lock().unwrap_or([0; 16]),
            ..Default::default()
        }
    }

    pub(crate) fn set_bool_option(&self, optname: i32, enabled: bool) -> Result<(), ERRNO> {
        let mut options = self.options.lock();
        if options.recv.set_bool(optname, enabled) {
            Ok(())
        } else {
            Err(ERRNO::ENOPROTOOPT)
        }
    }

    pub(crate) fn get_bool_option(&self, optname: i32) -> Result<i32, ERRNO> {
        let options = self.options.lock();
        options
            .recv
            .get_bool(optname)
            .map(|enabled| if enabled { 1 } else { 0 })
            .ok_or(ERRNO::ENOPROTOOPT)
    }

    pub(crate) fn set_checksum_offset(&self, offset: i32) -> Result<(), ERRNO> {
        if offset < -1 || (offset >= 0 && (offset & 1) != 0) {
            return Err(ERRNO::EINVAL);
        }
        self.options.lock().checksum_offset = if offset < 0 { None } else { Some(offset) };
        Ok(())
    }

    pub(crate) fn checksum_offset(&self) -> i32 {
        self.options.lock().checksum_offset.unwrap_or(-1)
    }

    pub(crate) fn set_icmp6_filter(&self, filter_bytes: &[u8]) -> Result<(), ERRNO> {
        let filter = Icmp6Filter::from_bytes(filter_bytes)?;
        self.options.lock().icmp6_filter = filter;
        Ok(())
    }

    pub(crate) fn icmp6_filter_bytes(&self) -> [u8; 32] {
        self.options.lock().icmp6_filter.as_bytes()
    }

    fn matches_destination(&self, dst: &[u8; 16]) -> bool {
        match *self.bound_addr.lock() {
            None => is_supported_addr(dst),
            Some(bound) => {
                if bound == [0; 16] {
                    is_supported_addr(dst)
                } else {
                    bound == *dst
                }
            }
        }
    }

    fn should_accept(&self, dst: &[u8; 16], protocol: i32, data: &[u8]) -> bool {
        if self.protocol != protocol || !self.matches_destination(dst) {
            return false;
        }
        if self.protocol == IPPROTO_ICMPV6 {
            if let Some(icmp_type) = data.first().copied() {
                return self.options.lock().icmp6_filter.allows_type(icmp_type);
            }
        }
        true
    }

    fn build_pktinfo_cmsg(cmsg_type: i32) -> RawIpv6ControlMessage {
        let pktinfo = In6PktInfo {
            ipi6_addr: LOOPBACK_V6,
            ipi6_ifindex: 1,
        };
        let data = unsafe {
            core::slice::from_raw_parts(
                (&pktinfo as *const In6PktInfo) as *const u8,
                core::mem::size_of::<In6PktInfo>(),
            )
        };
        RawIpv6ControlMessage {
            level: SOL_IPV6,
            cmsg_type,
            data: data.to_vec(),
        }
    }

    fn build_i32_cmsg(cmsg_type: i32, value: i32) -> RawIpv6ControlMessage {
        RawIpv6ControlMessage {
            level: SOL_IPV6,
            cmsg_type,
            data: value.to_ne_bytes().to_vec(),
        }
    }

    fn build_received_packet(&self, data: &[u8], meta: &RawIpv6SendMeta) -> RawIpv6RecvPacket {
        let options = self.options.lock();
        let mut control = Vec::new();
        if options.recv.recv_pktinfo {
            control.push(Self::build_pktinfo_cmsg(IPV6_PKTINFO));
        }
        if options.recv.recv_hoplimit {
            control.push(Self::build_i32_cmsg(IPV6_HOPLIMIT, meta.hoplimit.unwrap_or(64)));
        }
        if options.recv.recv_tclass {
            control.push(Self::build_i32_cmsg(IPV6_TCLASS, meta.tclass.unwrap_or(0)));
        }
        if options.recv.recv_2292pktinfo {
            control.push(Self::build_pktinfo_cmsg(IPV6_2292PKTINFO));
        }
        if options.recv.recv_2292hoplimit {
            control.push(Self::build_i32_cmsg(
                IPV6_2292HOPLIMIT,
                meta.hoplimit.unwrap_or(64),
            ));
        }
        RawIpv6RecvPacket {
            data: data.to_vec(),
            control,
        }
    }

    fn enqueue(&self, packet: RawIpv6RecvPacket) {
        self.recv_queue.lock().push_back(packet);
        self.read_wait.wake_all();
        notify_poll_source(self.poll_source_id(), POLLIN);
    }

    fn recv_ready(&self) -> bool {
        !self.recv_queue.lock().is_empty()
    }

    pub(crate) fn recv_into_user_buffer(
        &self,
        buf: &mut UserBuffer,
    ) -> Result<RawIpv6RecvPacket, ERRNO> {
        loop {
            if let Some(packet) = self.recv_queue.lock().pop_front() {
                let copied = write_user_buffer(buf, packet.data.as_slice());
                let mut packet = packet;
                packet.data.truncate(copied);
                return Ok(packet);
            }
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
            self.read_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                    self.recv_ready() || crate::signal::has_unmasked_pending_signal()
                });
        }
    }

    pub(crate) fn send_user_buffer_to(
        &self,
        buf: &UserBuffer,
        dst: &SockAddrIn6,
        meta: &RawIpv6SendMeta,
    ) -> Result<usize, ERRNO> {
        if dst.sin6_family != AF_INET6 {
            return Err(ERRNO::EAFNOSUPPORT);
        }
        if dst.sin6_scope_id != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }
        if !is_supported_addr(&dst.sin6_addr) {
            return Err(ERRNO::EADDRNOTAVAIL);
        }

        let mut data = Vec::with_capacity(buf.len());
        for chunk in buf.buffers.iter() {
            data.extend_from_slice(chunk);
        }

        if let Some(offset) = self.options.lock().checksum_offset {
            let offset = offset as usize;
            if offset + 1 >= data.len() {
                return Err(ERRNO::EINVAL);
            }
            data[offset] = 0;
            data[offset + 1] = 0;
            let checksum = compute_transport_checksum(&dst.sin6_addr, self.protocol as u8, &data);
            let bytes = checksum.to_be_bytes();
            data[offset] = bytes[0];
            data[offset + 1] = bytes[1];
        }

        let sockets = collect_raw_ipv6_sockets();
        for socket in sockets {
            if socket.should_accept(&dst.sin6_addr, self.protocol, data.as_slice()) {
                let packet = socket.build_received_packet(data.as_slice(), meta);
                socket.enqueue(packet);
            }
        }
        Ok(data.len())
    }
}

impl File for RawIpv6SocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at_result(&self, _offset: usize, mut buf: UserBuffer) -> Result<usize, ERRNO> {
        if buf.len() == 0 {
            return Ok(0);
        }
        let packet = self.recv_into_user_buffer(&mut buf)?;
        Ok(packet.data.len())
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if (events & POLLIN) != 0 && self.recv_ready() {
            ready |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self.poll_source_id() as u64,
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

impl Drop for RawIpv6SocketFile {
    fn drop(&mut self) {
        self.read_wait.wake_all();
        notify_poll_source(self.poll_source_id(), POLLIN | POLLOUT | POLLHUP);
    }
}

pub(crate) fn create_raw_ipv6_socket_file(protocol: i32) -> Arc<RawIpv6SocketFile> {
    let socket = Arc::new(RawIpv6SocketFile::new(protocol));
    RAW_IPV6_SOCKETS.lock().push(Arc::downgrade(&socket));
    socket
}

fn collect_raw_ipv6_sockets() -> Vec<Arc<RawIpv6SocketFile>> {
    let mut registry = RAW_IPV6_SOCKETS.lock();
    let mut sockets = Vec::new();
    registry.retain(|weak| {
        if let Some(socket) = weak.upgrade() {
            sockets.push(socket);
            true
        } else {
            false
        }
    });
    sockets
}

fn is_supported_addr(addr: &[u8; 16]) -> bool {
    *addr == [0; 16] || *addr == LOOPBACK_V6
}

fn write_user_buffer(buf: &mut UserBuffer, data: &[u8]) -> usize {
    let mut copied = 0usize;
    for chunk in buf.buffers.iter_mut() {
        if copied >= data.len() {
            break;
        }
        let take = min(chunk.len(), data.len() - copied);
        chunk[..take].copy_from_slice(&data[copied..copied + take]);
        copied += take;
    }
    copied
}

fn checksum_words(sum: u32, bytes: &[u8]) -> u32 {
    let mut sum = sum;
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        sum = sum.wrapping_add(u16::from_be_bytes([chunk[0], chunk[1]]) as u32);
    }
    if let Some(&byte) = chunks.remainder().first() {
        sum = sum.wrapping_add(u16::from_be_bytes([byte, 0]) as u32);
    }
    sum
}

fn finish_checksum(sum: u32) -> u16 {
    let mut sum = sum;
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
    }
    let value = !(sum as u16);
    if value == 0 { 0xffff } else { value }
}

fn compute_transport_checksum(dst: &[u8; 16], next_header: u8, payload: &[u8]) -> u16 {
    let mut sum = 0u32;
    sum = checksum_words(sum, &LOOPBACK_V6);
    sum = checksum_words(sum, dst);
    sum = checksum_words(sum, &(payload.len() as u32).to_be_bytes());
    sum = checksum_words(sum, &[0, 0, 0, next_header]);
    sum = checksum_words(sum, payload);
    finish_checksum(sum)
}
