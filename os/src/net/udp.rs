//! UDP socket implementation backed by smoltcp.

use alloc::{sync::Arc, vec::Vec};
use core::cmp::min;
use core::sync::atomic::Ordering;
use core::any::Any;

use smoltcp::socket::udp as udp_socket;
use smoltcp::wire::{IpEndpoint, IpAddress, Ipv4Address};

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{NEED_POLL, NET_STACK};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};

pub(crate) struct UdpSocketState {
    pub(crate) handle: smoltcp::iface::SocketHandle,
    pub(crate) read_wait: WaitQueue,
    pub(crate) write_wait: WaitQueue,
}

impl UdpSocketState {
    pub(crate) fn new(handle: smoltcp::iface::SocketHandle) -> Self {
        Self {
            handle,
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
        }
    }

    pub(crate) fn source_id(&self) -> usize {
        self as *const Self as usize
    }
}

pub(crate) struct UdpSocketFile {
    st: Arc<UdpSocketState>,
    connected: SpinNoIrqLock<Option<IpEndpoint>>,
}

impl UdpSocketFile {
    fn new(st: Arc<UdpSocketState>) -> Self {
        Self {
            st,
            connected: SpinNoIrqLock::new(None),
        }
    }

    pub(crate) fn recv_buffer_size(&self) -> usize {
        super::UDP_BUF
    }

    pub(crate) fn send_buffer_size(&self) -> usize {
        super::UDP_BUF
    }

    /// Return the local (bound) endpoint of this UDP socket. If not bound, returns None.
    pub(crate) fn local_endpoint(&self) -> Option<IpEndpoint> {
        let mut guard = crate::net::NET_STACK.lock();
        let stack = guard.as_mut()?;
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        let listen = socket.endpoint();
        let addr = listen
            .addr
            .unwrap_or(IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)));
        Some(IpEndpoint::new(addr, listen.port))
    }

    /// Return the connected peer endpoint for this UDP socket, if any.
    pub(crate) fn peer_endpoint(&self) -> Option<IpEndpoint> {
        *self.connected.lock()
    }

    pub(crate) fn bind(&self, port: u16) -> Result<(), ERRNO> {
        if port == 0 {
            return Err(ERRNO::EINVAL);
        }
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        socket.bind(port).map_err(|_| ERRNO::EADDRINUSE)
    }

    pub(crate) fn connect(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        *self.connected.lock() = Some(ep);
        Ok(())
    }

    pub(crate) fn send_to(&self, data: &[u8], ep: IpEndpoint) -> Result<usize, ERRNO> {
        loop {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            if socket.can_send() {
                socket.send_slice(data, ep).map_err(|_| ERRNO::ENOBUFS)?;
                NEED_POLL.store(true, Ordering::Release);
                return Ok(data.len());
            }
            drop(guard);
            self.st
                .write_wait
                .wait_with_reason_or_skip(WaitReason::SocketWritable, || self.can_send_now());
        }
    }

    pub(crate) fn send_user_buffer_to(&self, buf: &UserBuffer, ep: IpEndpoint) -> Result<usize, ERRNO> {
        let total = buf.len();
        if total == 0 {
            return Ok(0);
        }
        if buf.buffers.len() == 1 {
            return self.send_to(buf.buffers[0], ep);
        }

        let mut data = Vec::with_capacity(total);
        for slice in buf.buffers.iter() {
            data.extend_from_slice(slice);
        }
        self.send_to(data.as_slice(), ep)
    }

    pub(crate) fn send_user_buffer(&self, buf: &UserBuffer) -> Result<usize, ERRNO> {
        let ep = *self.connected.lock();
        let ep = ep.ok_or(ERRNO::EDESTADDRREQ)?;
        self.send_user_buffer_to(buf, ep)
    }

    pub(crate) fn recv_from_user_buffer(&self, out: &mut UserBuffer) -> Result<(usize, IpEndpoint), ERRNO> {
        loop {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            if socket.can_recv() {
                if let Ok((data, meta)) = socket.recv() {
                    let mut off = 0usize;
                    for slice in out.buffers.iter_mut() {
                        if off >= data.len() {
                            break;
                        }
                        let end = min(off + slice.len(), data.len());
                        slice[..(end - off)].copy_from_slice(&data[off..end]);
                        off = end;
                    }
                    return Ok((off, meta.endpoint));
                }
            }
            drop(guard);
            self.st
                .read_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || self.can_recv_now());
        }
    }

    fn can_recv_now(&self) -> bool {
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        socket.can_recv()
    }

    fn can_send_now(&self) -> bool {
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        socket.can_send()
    }
}

impl File for UdpSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: usize, mut buf: UserBuffer) -> usize {
        if buf.len() == 0 {
            return 0;
        }
        let Ok((n, _)) = self.recv_from_user_buffer(&mut buf) else {
            return 0;
        };
        n
    }

    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        if buf.len() == 0 {
            return 0;
        }
        self.send_user_buffer(&buf).unwrap_or(0)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if (events & POLLIN) != 0 && self.can_recv_now() {
            ready |= POLLIN;
        }
        if (events & POLLOUT) != 0 && self.can_send_now() {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.st.source_id()
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

impl Drop for UdpSocketFile {
    fn drop(&mut self) {
        let source_id = self.st.source_id();
        let mut guard = NET_STACK.lock();
        if let Some(stack) = guard.as_mut() {
            stack.remove_udp_socket(self.st.handle);
        }
        drop(guard);
        self.st.read_wait.wake_all();
        self.st.write_wait.wake_all();
        notify_poll_source(source_id, POLLIN | POLLOUT | POLLHUP);
    }
}

pub(crate) fn create_udp_socket_file() -> Option<Arc<UdpSocketFile>> {
    let mut guard = NET_STACK.lock();
    let stack = guard.as_mut()?;
    let (_handle, st) = stack.create_udp_socket();
    Some(Arc::new(UdpSocketFile::new(st)))
}
