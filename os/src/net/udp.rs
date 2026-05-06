//! UDP socket implementation backed by smoltcp.

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;
use core::cmp::min;
use core::sync::atomic::Ordering;

use smoltcp::socket::udp as udp_socket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{NEED_POLL, NET_STACK};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};

#[inline]
fn listen_endpoint_from_bind(ep: IpEndpoint) -> IpListenEndpoint {
    let addr = if ep.addr.is_unspecified() {
        None
    } else {
        Some(ep.addr)
    };
    IpListenEndpoint { addr, port: ep.port }
}

#[inline]
fn loopback_source_addr_for_peer(peer: IpEndpoint) -> Option<IpAddress> {
    match peer.addr {
        IpAddress::Ipv4(v4) if v4.is_loopback() => {
            Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)))
        }
        _ => None,
    }
}

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

    pub(crate) fn bind(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        if ep.port == 0 {
            return Err(ERRNO::EINVAL);
        }
        let bind_ep = listen_endpoint_from_bind(ep);
        debug!("UDP socket {:?} binding to {:?}", self.st.handle, bind_ep);
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        match socket.bind(bind_ep) {
            Ok(()) => {
                debug!("UDP socket {:?} bind succeeded", self.st.handle);
                Ok(())
            }
            Err(e) => {
                warn!("UDP socket {:?} bind failed: {:?}", self.st.handle, e);
                Err(ERRNO::EADDRINUSE)
            }
        }
    }

    pub(crate) fn connect(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        debug!("UDP socket {:?} connecting to {:?}", self.st.handle, ep);
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            self.ensure_bound_for_send_locked(stack, ep)?;

            // Call smoltcp's connect to enable source filtering
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            socket.connect(ep).map_err(|_| ERRNO::EINVAL)?;
        }
        *self.connected.lock() = Some(ep);
        debug!("UDP socket {:?} connect succeeded", self.st.handle);
        Ok(())
    }

    fn ensure_bound_for_send_locked(
        &self,
        stack: &mut crate::net::NetStack,
        peer: IpEndpoint,
    ) -> Result<(), ERRNO> {
        let already_bound = {
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            socket.endpoint().port != 0
        };
        if already_bound {
            return Ok(());
        }

        let local = IpListenEndpoint {
            addr: loopback_source_addr_for_peer(peer),
            port: stack.alloc_ephemeral_port(),
        };
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        socket.bind(local).map_err(|_| ERRNO::EADDRINUSE)
    }

    pub(crate) fn send_to(&self, data: &[u8], ep: IpEndpoint) -> Result<usize, ERRNO> {
        trace!("udp send_to: data_len={} ep={} handle={:?}", data.len(), ep, self.st.handle);
        loop {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            self.ensure_bound_for_send_locked(stack, ep)?;
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);

            // Check if socket can send
            let can_send = socket.can_send();

            if can_send {
                match socket.send_slice(data, ep) {
                    Ok(()) => {
                        trace!("udp send_slice succeeded for socket {:?}, calling poll", self.st.handle);

                        // Check socket state after send
                        let has_data = socket.can_send(); // This checks if there's room, not if there's data to send
                        trace!("udp socket {:?} state after send: can_send={}", self.st.handle, has_data);

                        stack.poll();
                        NEED_POLL.store(true, Ordering::Release);
                        return Ok(data.len());
                    }
                    Err(e) => {
                        warn!("udp send_slice failed for socket {:?}: {:?}", self.st.handle, e);
                        return Err(ERRNO::ENOBUFS);
                    }
                }
            }
            debug!("udp socket {:?} cannot send, waiting", self.st.handle);
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
