//! UDP socket implementation backed by smoltcp.

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;
use core::cmp::min;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use smoltcp::socket::udp as udp_socket;
use smoltcp::socket::udp::SendError;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{
    cleanup_socket_wait, compat_ifreq_ioctl, register_socket_wait, socket_wait_mark_ready, socket_wait_should_skip,
    socket_wait_state, timeout_ns_to_deadline_ns, SocketWakeState, NEED_POLL, NET_STACK,
};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{current_task, WaitQueue, WaitReason};
use crate::timer::{add_timer_with_socket_tag, get_time_ns};

const AF_INET_FAMILY: i32 = 2;

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
fn unspecified_addr_for_family(family: i32) -> IpAddress {
    if family == super::AF_INET6 as i32 {
        IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
    } else {
        IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0))
    }
}

#[inline]
fn loopback_source_addr_for_peer(peer: IpEndpoint) -> Option<IpAddress> {
    match peer.addr {
        IpAddress::Ipv4(v4) if v4.is_loopback() => {
            Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)))
        }
        IpAddress::Ipv6(v6) if v6.is_loopback() => Some(IpAddress::Ipv6(Ipv6Address::LOCALHOST)),
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
    family: i32,
    st: Arc<UdpSocketState>,
    bound_endpoint: SpinNoIrqLock<Option<IpListenEndpoint>>,
    connected: SpinNoIrqLock<Option<IpEndpoint>>,
    ipv6_only: AtomicBool,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
}

impl UdpSocketFile {
    fn new(st: Arc<UdpSocketState>, family: i32) -> Self {
        Self {
            family,
            st,
            bound_endpoint: SpinNoIrqLock::new(None),
            connected: SpinNoIrqLock::new(None),
            ipv6_only: AtomicBool::new(false),
            recv_timeout_ns: AtomicU64::new(0),
            send_timeout_ns: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_ipv6_only(&self, enabled: bool) {
        self.ipv6_only.store(enabled, Ordering::Release);
    }

    pub(crate) fn ipv6_only(&self) -> bool {
        self.ipv6_only.load(Ordering::Acquire)
    }

    pub(crate) fn recv_buffer_size(&self) -> usize {
        super::UDP_RX_BUF
    }

    pub(crate) fn send_buffer_size(&self) -> usize {
        super::UDP_TX_BUF
    }

    pub(crate) fn set_recv_timeout_ns(&self, timeout_ns: u64) {
        self.recv_timeout_ns.store(timeout_ns, Ordering::Release);
    }

    pub(crate) fn recv_timeout_ns(&self) -> u64 {
        self.recv_timeout_ns.load(Ordering::Acquire)
    }

    pub(crate) fn set_send_timeout_ns(&self, timeout_ns: u64) {
        self.send_timeout_ns.store(timeout_ns, Ordering::Release);
    }

    pub(crate) fn send_timeout_ns(&self) -> u64 {
        self.send_timeout_ns.load(Ordering::Acquire)
    }

    /// Return the local (bound) endpoint of this UDP socket. If not bound, returns None.
    pub(crate) fn local_endpoint(&self) -> Option<IpEndpoint> {
        if self.connected.lock().is_some() {
            let mut guard = crate::net::NET_STACK.lock();
            let stack = guard.as_mut()?;
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            let listen = socket.endpoint();
            let addr = listen.addr.unwrap_or(unspecified_addr_for_family(self.family));
            return Some(IpEndpoint::new(addr, listen.port));
        }

        if let Some(bound) = *self.bound_endpoint.lock() {
            let addr = bound.addr.unwrap_or(unspecified_addr_for_family(self.family));
            return Some(IpEndpoint::new(addr, bound.port));
        }

        let mut guard = crate::net::NET_STACK.lock();
        let stack = guard.as_mut()?;
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        let listen = socket.endpoint();
        if listen.port == 0 {
            return None;
        }
        let addr = listen.addr.unwrap_or(unspecified_addr_for_family(self.family));
        Some(IpEndpoint::new(addr, listen.port))
    }

    /// Return the connected peer endpoint for this UDP socket, if any.
    pub(crate) fn peer_endpoint(&self) -> Option<IpEndpoint> {
        *self.connected.lock()
    }

    pub(crate) fn bind(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
        let port = if ep.port == 0 {
            stack.alloc_ephemeral_port()
        } else {
            ep.port
        };
        let bind_ep = listen_endpoint_from_bind(IpEndpoint::new(ep.addr, port));
        let stack_ep = if bind_ep.addr.is_some() {
            bind_ep
        } else if self.family == AF_INET_FAMILY {
            IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))),
                port: bind_ep.port,
            }
        } else if self.family == super::AF_INET6 as i32 && self.ipv6_only() {
            IpListenEndpoint {
                addr: Some(IpAddress::Ipv6(Ipv6Address::LOCALHOST)),
                port: bind_ep.port,
            }
        } else {
            // AF_INET6 wildcard with IPV6_V6ONLY=0 keeps addr=None so one UDP
            // socket can accept both IPv6 and IPv4 loopback datagrams.
            bind_ep
        };
        debug!(
            "UDP socket {:?} binding to {:?} stack_ep={:?}",
            self.st.handle,
            bind_ep,
            stack_ep
        );
        let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
        match socket.bind(stack_ep) {
            Ok(()) => {
                *self.bound_endpoint.lock() = Some(bind_ep);
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
        let timeout_ns = timeout_ns_to_deadline_ns(self.send_timeout_ns())?;
        let mut timeout_handle = None;
        let mut deadline_ns = None;
        loop {
            if crate::signal::has_unmasked_pending_signal() {
                if let Some(handle) = timeout_handle.take() {
                    cleanup_socket_wait(handle);
                }
                return Err(ERRNO::EINTR);
            }
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            self.ensure_bound_for_send_locked(stack, ep)?;
            if ep.port != 0 && stack.deliver_udp_loopback(self.st.handle, ep, data) {
                if let Some(handle) = timeout_handle.take() {
                    socket_wait_mark_ready(handle);
                    cleanup_socket_wait(handle);
                }
                crate::net::perf_udp_user_send(data.len());
                return Ok(data.len());
            }

            // Check if socket can send
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
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
                        if let Some(handle) = timeout_handle.take() {
                            socket_wait_mark_ready(handle);
                            cleanup_socket_wait(handle);
                        }
                        crate::net::perf_udp_user_send(data.len());
                        return Ok(data.len());
                    }
                    Err(e) => {
                        error!("udp send_slice failed for socket {:?}: {:?}, ep = {}", self.st.handle, e, ep);
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return match e {
                            SendError::Unaddressable => Err(ERRNO::EHOSTUNREACH),
                            SendError::BufferFull => Err(ERRNO::ENOBUFS),
                        };
                    }
                }
            }
            debug!("udp socket {:?} cannot send, waiting", self.st.handle);
            drop(guard);
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(deadline, Arc::clone(&task), Some(handle.timer_tag()));
                    timeout_handle = Some(handle);
                    deadline_ns = Some(deadline);
                }
                if let Some(deadline) = deadline_ns {
                    if get_time_ns() >= deadline {
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
                let handle = timeout_handle.expect("socket wait handle must exist");
                self.st.write_wait.wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                    self.can_send_now() || socket_wait_should_skip(handle)
                });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.can_send_now() {
                            if let Some(handle) = timeout_handle.take() {
                                cleanup_socket_wait(handle);
                            }
                            deadline_ns = None;
                            continue;
                        }
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
            } else {
                self.st
                    .write_wait
                    .wait_with_reason_or_skip(WaitReason::SocketWritable, || self.can_send_now());
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
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
        let timeout_ns = timeout_ns_to_deadline_ns(self.recv_timeout_ns())?;
        let mut timeout_handle = None;
        let mut deadline_ns = None;
        loop {
            if crate::signal::has_unmasked_pending_signal() {
                if let Some(handle) = timeout_handle.take() {
                    cleanup_socket_wait(handle);
                }
                return Err(ERRNO::EINTR);
            }
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
                    if let Some(handle) = timeout_handle.take() {
                        socket_wait_mark_ready(handle);
                        cleanup_socket_wait(handle);
                    }
                    crate::net::perf_udp_user_recv(off);
                    return Ok((off, meta.endpoint));
                }
            }
            drop(guard);
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(deadline, Arc::clone(&task), Some(handle.timer_tag()));
                    timeout_handle = Some(handle);
                    deadline_ns = Some(deadline);
                }
                if let Some(deadline) = deadline_ns {
                    if get_time_ns() >= deadline {
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
                let handle = timeout_handle.expect("socket wait handle must exist");
                self.st.read_wait.wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                    self.can_recv_now() || socket_wait_should_skip(handle)
                });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.can_recv_now() {
                            if let Some(handle) = timeout_handle.take() {
                                cleanup_socket_wait(handle);
                            }
                            deadline_ns = None;
                            continue;
                        }
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
            } else {
                self.st
                    .read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || self.can_recv_now());
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
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

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        compat_ifreq_ioctl(req, arg)
    }

    fn read_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        self.read_at_result(_offset, buf).unwrap_or(0)
    }

    fn read_at_result(&self, _offset: usize, mut buf: UserBuffer) -> Result<usize, ERRNO> {
        if buf.len() == 0 {
            return Ok(0);
        }
        self.recv_from_user_buffer(&mut buf).map(|(n, _)| n)
    }

    fn read_bytes_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        let timeout_ns = timeout_ns_to_deadline_ns(self.recv_timeout_ns())?;
        let mut timeout_handle = None;
        let mut deadline_ns = None;
        loop {
            if crate::signal::has_unmasked_pending_signal() {
                if let Some(handle) = timeout_handle.take() {
                    cleanup_socket_wait(handle);
                }
                return Err(ERRNO::EINTR);
            }
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<udp_socket::Socket>(self.st.handle);
            if socket.can_recv() {
                if let Ok((data, _meta)) = socket.recv() {
                    let n = min(data.len(), buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    if let Some(handle) = timeout_handle.take() {
                        socket_wait_mark_ready(handle);
                        cleanup_socket_wait(handle);
                    }
                    crate::net::perf_udp_user_recv(n);
                    return Ok(n);
                }
            }
            drop(guard);
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(deadline, Arc::clone(&task), Some(handle.timer_tag()));
                    timeout_handle = Some(handle);
                    deadline_ns = Some(deadline);
                }
                if let Some(deadline) = deadline_ns {
                    if get_time_ns() >= deadline {
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
                let handle = timeout_handle.expect("socket wait handle must exist");
                self.st.read_wait.wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                    self.can_recv_now() || socket_wait_should_skip(handle)
                });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.can_recv_now() {
                            if let Some(handle) = timeout_handle.take() {
                                cleanup_socket_wait(handle);
                            }
                            deadline_ns = None;
                            continue;
                        }
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EAGAIN);
                    }
                }
            } else {
                self.st
                    .read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || self.can_recv_now());
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
        }
    }

    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        self.write_at_result(_offset, buf).unwrap_or(0)
    }

    fn write_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        if buf.len() == 0 {
            return Ok(0);
        }
        self.send_user_buffer(&buf)
    }

    fn write_bytes_at(&self, _offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        let ep = *self.connected.lock();
        let ep = ep.ok_or(ERRNO::EDESTADDRREQ)?;
        self.send_to(buf, ep)
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

pub(crate) fn create_udp_socket_file(family: i32) -> Option<Arc<UdpSocketFile>> {
    let mut guard = NET_STACK.lock();
    let stack = guard.as_mut()?;
    let (_handle, st) = stack.create_udp_socket();
    Some(Arc::new(UdpSocketFile::new(st, family)))
}
