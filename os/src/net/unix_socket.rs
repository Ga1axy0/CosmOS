use core::any::Any;

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use crate::{
    fs::{File, FileDescription, Pipe, Stat, StatMode},
    mm::UserBuffer,
    sync::SpinNoIrqLock,
    syscall::errno::ERRNO,
};

const POLLIN: u16 = 0x001;
const POLLOUT: u16 = 0x004;
const POLLHUP: u16 = 0x010;

/// socket level for ancillary data.
pub const SOL_SOCKET: i32 = 1;
/// pass file descriptors through UNIX domain sockets.
pub const SCM_RIGHTS: i32 = 1;
/// pass peer credentials through UNIX domain sockets.
pub const SCM_CREDENTIALS: i32 = 2;

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
    read_shutdown: bool,
    write_shutdown: bool,
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
    rx_seq_lock: SpinNoIrqLock<()>,
    /// 串行化 write/sendmsg，保证与 tx_meta 的入队顺序一致。
    tx_seq_lock: SpinNoIrqLock<()>,
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
                read_shutdown: false,
                write_shutdown: false,
            }),
            rx_meta,
            tx_meta,
            rx_seq_lock: SpinNoIrqLock::new(()),
            tx_seq_lock: SpinNoIrqLock::new(()),
        }
    }

    /// 使用两条方向 pipe 创建一对互联 UNIX socket 端点。
    pub(crate) fn new_pair(
        end0_rx: Arc<Pipe>,
        end0_tx: Arc<Pipe>,
        end1_rx: Arc<Pipe>,
        end1_tx: Arc<Pipe>,
    ) -> (Self, Self) {
        let ab_meta = Arc::new(SpinNoIrqLock::new(VecDeque::new()));
        let ba_meta = Arc::new(SpinNoIrqLock::new(VecDeque::new()));

        let end0 = Self::new_internal(end0_rx, end0_tx, ba_meta.clone(), ab_meta.clone());
        let end1 = Self::new_internal(end1_rx, end1_tx, ab_meta, ba_meta);
        (end0, end1)
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

        let _seq = self.tx_seq_lock.lock();

        let tx = {
            let state = self.state.lock();
            if state.write_shutdown || state.tx.is_none() {
                if strict_shutdown {
                    return Err(ERRNO::ESHUTDOWN);
                }
                return Ok(0);
            }
            state.tx.as_ref().cloned().unwrap()
        };

        let written = tx.write_at(0, buf);
        if written > 0 {
            self.tx_meta.lock().push_back(UnixStreamFrameMeta {
                remaining: written,
                rights: ancillary.rights,
                credentials: ancillary.credentials,
            });
        }

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

        let _seq = self.rx_seq_lock.lock();

        {
            let state = self.state.lock();
            if state.read_shutdown {
                return Ok((0, UnixSocketAncillaryData::default()));
            }
        }

        let read_len = self.rx.read_at(0, buf);
        let ancillary = self.consume_rx_meta(read_len, true);
        Ok((read_len, ancillary))
    }

    /// `shutdown(2)` half-close 支持。
    pub fn shutdown(&self, how: i32) -> Result<(), ERRNO> {
        let mut state = self.state.lock();
        match how {
            0 => {  // SHUT_RD
                state.read_shutdown = true;
            }
            1 => {  // SHUT_WR
                state.write_shutdown = true;
                state.tx.take();
            }
            2 => {  // SHUT_RDWR
                state.read_shutdown = true;
                state.write_shutdown = true;
                state.tx.take();
            }
            _ => return Err(ERRNO::EINVAL),
        }
        Ok(())
    }
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

        let _seq = self.rx_seq_lock.lock();

        {
            let state = self.state.lock();
            if state.read_shutdown {
                return 0;
            }
        }

        let read_len = self.rx.read_at(offset, buf);
        self.consume_rx_meta(read_len, false);
        read_len
    }

    fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        let _ = offset;
        self.write_with_ancillary(buf, UnixSocketAncillaryData::default(), false)
            .unwrap_or(0)
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