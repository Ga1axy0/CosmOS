//! TCP socket implementation backed by smoltcp.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use smoltcp::socket::tcp as tcp_socket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{
    cleanup_socket_wait, register_socket_wait, socket_wait_mark_ready, socket_wait_should_skip,
    socket_wait_state, timeout_ns_to_deadline_ns, SocketWakeState, NEED_POLL, NET_STACK,
};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{current_task, WaitQueue, WaitReason};
use crate::timer::{add_timer_with_socket_tag, get_time_ns};

const SOMAXCONN: usize = 128;

#[inline]
fn unspecified_addr_for_family(family: i32) -> IpAddress {
    if family == super::AF_INET6 as i32 {
        IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
    } else {
        IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0))
    }
}

#[inline]
fn stack_listen_addr_for_family(family: i32, addr: Option<IpAddress>) -> Option<IpAddress> {
    match (family, addr) {
        (x, None) if x == super::AF_INET6 as i32 => Some(IpAddress::Ipv6(Ipv6Address::LOCALHOST)),
        (_, addr) => addr,
    }
}

#[inline]
fn ipv4_loopback_listen_endpoint(port: u16) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))),
        port,
    }
}

#[inline]
fn normalize_backlog(backlog: usize) -> usize {
    backlog.clamp(1, SOMAXCONN)
}

#[inline]
fn listen_endpoint_from_bind(ep: IpEndpoint) -> IpListenEndpoint {
    let addr = if ep.addr.is_unspecified() {
        None
    } else {
        Some(ep.addr)
    };
    IpListenEndpoint {
        addr,
        port: ep.port,
    }
}

pub(crate) struct TcpListenerShared {
    family: i32,
    addr: SpinNoIrqLock<Option<IpAddress>>,
    port: AtomicUsize,
    backlog: AtomicUsize,
    dual_stack_v4: AtomicBool,
    pending: SpinNoIrqLock<VecDeque<Arc<TcpSocketState>>>,
    passive: SpinNoIrqLock<Vec<Arc<TcpSocketState>>>,
    accept_wait: WaitQueue,
}

impl TcpListenerShared {
    fn new(family: i32, endpoint: IpListenEndpoint, backlog: usize) -> Self {
        Self {
            family,
            addr: SpinNoIrqLock::new(endpoint.addr),
            port: AtomicUsize::new(endpoint.port as usize),
            backlog: AtomicUsize::new(backlog),
            dual_stack_v4: AtomicBool::new(false),
            pending: SpinNoIrqLock::new(VecDeque::new()),
            passive: SpinNoIrqLock::new(Vec::new()),
            accept_wait: WaitQueue::new(),
        }
    }

    pub(crate) fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn port(&self) -> u16 {
        self.port.load(Ordering::Acquire) as u16
    }

    fn addr(&self) -> Option<IpAddress> {
        *self.addr.lock()
    }

    fn set_endpoint(&self, endpoint: IpListenEndpoint) {
        *self.addr.lock() = endpoint.addr;
        self.port.store(endpoint.port as usize, Ordering::Release);
    }

    fn endpoint(&self) -> IpListenEndpoint {
        IpListenEndpoint {
            addr: self.addr(),
            port: self.port(),
        }
    }

    fn stack_endpoint(&self) -> IpListenEndpoint {
        let endpoint = self.endpoint();
        IpListenEndpoint {
            addr: stack_listen_addr_for_family(self.family, endpoint.addr),
            port: endpoint.port,
        }
    }

    fn set_dual_stack_v4(&self, enabled: bool) {
        self.dual_stack_v4.store(enabled, Ordering::Release);
    }

    fn stack_endpoints(&self) -> Vec<IpListenEndpoint> {
        let mut endpoints = Vec::new();
        let primary = self.stack_endpoint();
        endpoints.push(primary);
        if self.dual_stack_v4.load(Ordering::Acquire) && self.family == super::AF_INET6 as i32 {
            let base = self.endpoint();
            if base.addr.is_none() {
                endpoints.push(ipv4_loopback_listen_endpoint(base.port));
            }
        }
        endpoints
    }

    fn backlog(&self) -> usize {
        self.backlog.load(Ordering::Acquire)
    }

    fn set_backlog(&self, backlog: usize) {
        self.backlog.store(backlog, Ordering::Release);
    }

    fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    fn pop_pending(&self) -> Option<Arc<TcpSocketState>> {
        self.pending.lock().pop_front()
    }

    fn push_pending(&self, st: Arc<TcpSocketState>) {
        self.pending.lock().push_back(st);
    }

    fn passive_len(&self) -> usize {
        self.passive.lock().len()
    }

    fn contains_passive(&self, handle: smoltcp::iface::SocketHandle) -> bool {
        self.passive.lock().iter().any(|st| st.handle == handle)
    }

    fn push_passive(&self, st: Arc<TcpSocketState>) {
        if !self.contains_passive(st.handle) {
            self.passive.lock().push(st);
        }
    }

    fn remove_passive(&self, handle: smoltcp::iface::SocketHandle) {
        self.passive.lock().retain(|st| st.handle != handle);
    }

    fn pop_one_passive(&self) -> Option<Arc<TcpSocketState>> {
        self.passive.lock().pop()
    }

    fn slot_count(&self) -> usize {
        self.pending_len() + self.passive_len()
    }

    fn take_all_states(&self) -> Vec<Arc<TcpSocketState>> {
        let mut out = Vec::new();
        {
            let mut pending = self.pending.lock();
            while let Some(st) = pending.pop_front() {
                out.push(st);
            }
        }
        {
            let mut passive = self.passive.lock();
            while let Some(st) = passive.pop() {
                out.push(st);
            }
        }
        out
    }

    fn wake_accept_one(&self) {
        self.accept_wait.wake_one();
    }

    fn wake_accept_all(&self) {
        self.accept_wait.wake_all();
    }
}

pub(crate) struct TcpSocketState {
    pub(crate) handle: smoltcp::iface::SocketHandle,
    pub(crate) read_wait: WaitQueue,
    pub(crate) write_wait: WaitQueue,
    pub(crate) orphaned: AtomicBool,
    listener: SpinNoIrqLock<Option<Weak<TcpListenerShared>>>,
    listener_endpoint: SpinNoIrqLock<Option<IpListenEndpoint>>,
    queued_for_accept: AtomicBool,
    last_state: AtomicUsize,
}

impl TcpSocketState {
    pub(crate) fn new(handle: smoltcp::iface::SocketHandle) -> Self {
        Self {
            handle,
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            orphaned: AtomicBool::new(false),
            listener: SpinNoIrqLock::new(None),
            listener_endpoint: SpinNoIrqLock::new(None),
            queued_for_accept: AtomicBool::new(false),
            last_state: AtomicUsize::new(usize::MAX),
        }
    }

    pub(crate) fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    pub(crate) fn listener_shared(&self) -> Option<Arc<TcpListenerShared>> {
        self.listener.lock().as_ref().and_then(Weak::upgrade)
    }

    fn set_listener(&self, listener: Option<Weak<TcpListenerShared>>) {
        *self.listener.lock() = listener;
        self.queued_for_accept.store(false, Ordering::Release);
    }

    fn clear_listener(&self) {
        *self.listener.lock() = None;
        *self.listener_endpoint.lock() = None;
    }

    fn listener_endpoint(&self) -> Option<IpListenEndpoint> {
        *self.listener_endpoint.lock()
    }

    fn set_listener_endpoint(&self, endpoint: IpListenEndpoint) {
        *self.listener_endpoint.lock() = Some(endpoint);
    }

    pub(crate) fn is_listener_owned(&self) -> bool {
        self.listener_shared().is_some()
    }

    fn try_mark_queued_for_accept(&self) -> bool {
        self.queued_for_accept
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn clear_queued_for_accept(&self) {
        self.queued_for_accept.store(false, Ordering::Release);
    }

    pub(crate) fn observe_state_change(&self, state: tcp_socket::State) -> Option<usize> {
        let next = state as usize;
        let prev = self.last_state.swap(next, Ordering::AcqRel);
        if prev == next {
            None
        } else {
            Some(prev)
        }
    }
}

pub(crate) fn tcp_state_name(state: tcp_socket::State) -> &'static str {
    match state {
        tcp_socket::State::Closed => "CLOSED",
        tcp_socket::State::Listen => "LISTEN",
        tcp_socket::State::SynSent => "SYN-SENT",
        tcp_socket::State::SynReceived => "SYN-RECEIVED",
        tcp_socket::State::Established => "ESTABLISHED",
        tcp_socket::State::FinWait1 => "FIN-WAIT-1",
        tcp_socket::State::FinWait2 => "FIN-WAIT-2",
        tcp_socket::State::CloseWait => "CLOSE-WAIT",
        tcp_socket::State::Closing => "CLOSING",
        tcp_socket::State::LastAck => "LAST-ACK",
        tcp_socket::State::TimeWait => "TIME-WAIT",
    }
}

pub(crate) fn tcp_state_name_repr(state: usize) -> &'static str {
    if state == usize::MAX {
        return "<unobserved>";
    }
    match state {
        x if x == tcp_socket::State::Closed as usize => "CLOSED",
        x if x == tcp_socket::State::Listen as usize => "LISTEN",
        x if x == tcp_socket::State::SynSent as usize => "SYN-SENT",
        x if x == tcp_socket::State::SynReceived as usize => "SYN-RECEIVED",
        x if x == tcp_socket::State::Established as usize => "ESTABLISHED",
        x if x == tcp_socket::State::FinWait1 as usize => "FIN-WAIT-1",
        x if x == tcp_socket::State::FinWait2 as usize => "FIN-WAIT-2",
        x if x == tcp_socket::State::CloseWait as usize => "CLOSE-WAIT",
        x if x == tcp_socket::State::Closing as usize => "CLOSING",
        x if x == tcp_socket::State::LastAck as usize => "LAST-ACK",
        x if x == tcp_socket::State::TimeWait as usize => "TIME-WAIT",
        _ => "<unknown>",
    }
}

/// Called by net poll path to move one passive listener socket into the listener pending queue
/// once a TCP handshake is established.
pub(crate) fn queue_listener_connection_if_ready(
    st: &Arc<TcpSocketState>,
    state: tcp_socket::State,
) -> Option<usize> {
    if !matches!(
        state,
        tcp_socket::State::Established | tcp_socket::State::CloseWait
    ) {
        return None;
    }
    let listener = st.listener_shared()?;
    if !st.try_mark_queued_for_accept() {
        return None;
    }

    debug!(
        "Tcp listener queued established connection: handle={:?} state={}",
        st.handle,
        tcp_state_name(state)
    );
    listener.remove_passive(st.handle);
    listener.push_pending(Arc::clone(st));
    st.clear_listener();
    listener.wake_accept_one();
    Some(listener.source_id())
}

pub(crate) struct TcpSocketFile {
    family: i32,
    st: SpinNoIrqLock<Arc<TcpSocketState>>,
    bound_endpoint: SpinNoIrqLock<Option<IpListenEndpoint>>,
    listening: AtomicBool,
    listener: SpinNoIrqLock<Option<Arc<TcpListenerShared>>>,
    ipv6_only: AtomicBool,
    close_on_drop: AtomicBool,
    recv_timeout_ns: AtomicU64,
    send_timeout_ns: AtomicU64,
    /// IPv4 multicast groups this socket has joined (per-socket membership).
    /// Tracked here, not in the shared `TcpSocketState`, so that an accepted
    /// socket starts with an empty list rather than inheriting the listener's
    /// memberships (CVE-2017-8890).
    mcast_groups: SpinNoIrqLock<Vec<Ipv4Address>>,
}

impl TcpSocketFile {
    fn new(st: Arc<TcpSocketState>, family: i32) -> Self {
        Self {
            family,
            st: SpinNoIrqLock::new(st),
            bound_endpoint: SpinNoIrqLock::new(None),
            listening: AtomicBool::new(false),
            listener: SpinNoIrqLock::new(None),
            ipv6_only: AtomicBool::new(false),
            close_on_drop: AtomicBool::new(true),
            recv_timeout_ns: AtomicU64::new(0),
            send_timeout_ns: AtomicU64::new(0),
            mcast_groups: SpinNoIrqLock::new(Vec::new()),
        }
    }

    pub(crate) fn set_ipv6_only(&self, enabled: bool) {
        self.ipv6_only.store(enabled, Ordering::Release);
    }

    pub(crate) fn ipv6_only(&self) -> bool {
        self.ipv6_only.load(Ordering::Acquire)
    }

    /// Join an IPv4 multicast group. Returns `false` if the socket was already
    /// a member of `addr` (caller maps this to `EADDRINUSE`).
    pub(crate) fn join_mcast_group(&self, addr: Ipv4Address) -> bool {
        let mut groups = self.mcast_groups.lock();
        if groups.contains(&addr) {
            return false;
        }
        groups.push(addr);
        true
    }

    /// Leave an IPv4 multicast group. Returns `false` if the socket was not a
    /// member of `addr` (caller maps this to `EADDRNOTAVAIL`).
    pub(crate) fn leave_mcast_group(&self, addr: Ipv4Address) -> bool {
        let mut groups = self.mcast_groups.lock();
        if let Some(pos) = groups.iter().position(|g| *g == addr) {
            groups.remove(pos);
            true
        } else {
            false
        }
    }

    pub(crate) fn recv_buffer_size(&self) -> usize {
        super::TCP_RX_BUF
    }

    pub(crate) fn send_buffer_size(&self) -> usize {
        super::TCP_TX_BUF
    }

    pub(crate) fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    pub(crate) fn is_connected(&self) -> Result<bool, ERRNO> {
        let st = self.state();
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        Ok(matches!(
            socket.state(),
            tcp_socket::State::Established | tcp_socket::State::CloseWait
        ))
    }

    pub(crate) fn disconnect(&self) -> Result<(), ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EISCONN);
        }

        let st = self.state();
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            socket.abort();
            stack.poll();
        }
        *self.bound_endpoint.lock() = None;
        st.read_wait.wake_all();
        st.write_wait.wake_all();
        notify_poll_source(st.source_id(), POLLIN | POLLOUT | POLLHUP);
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn ipv6_addrform_to_ipv4(&self) -> Result<Arc<TcpSocketFile>, ERRNO> {
        if self.family != super::AF_INET6 as i32 || self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::ENOPROTOOPT);
        }

        let local = self.local_endpoint().ok_or(ERRNO::ENOTCONN)?;
        let remote = self.remote_endpoint().ok_or(ERRNO::ENOTCONN)?;
        if !matches!(local.addr, IpAddress::Ipv4(_)) || !matches!(remote.addr, IpAddress::Ipv4(_)) {
            return Err(ERRNO::EADDRNOTAVAIL);
        }

        self.close_on_drop.store(false, Ordering::Release);
        Ok(Arc::new(TcpSocketFile {
            family: super::AF_INET as i32,
            st: SpinNoIrqLock::new(self.state()),
            bound_endpoint: SpinNoIrqLock::new(Some(listen_endpoint_from_bind(local))),
            listening: AtomicBool::new(false),
            listener: SpinNoIrqLock::new(None),
            ipv6_only: AtomicBool::new(false),
            close_on_drop: AtomicBool::new(true),
            recv_timeout_ns: AtomicU64::new(self.recv_timeout_ns()),
            send_timeout_ns: AtomicU64::new(self.send_timeout_ns()),
            mcast_groups: SpinNoIrqLock::new(Vec::new()),
        }))
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

    /// Return the local endpoint of this TCP socket, or None if unavailable.
    pub(crate) fn local_endpoint(&self) -> Option<IpEndpoint> {
        let st = self.state();
        let mut guard = crate::net::NET_STACK.lock();
        let stack = guard.as_mut()?;
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        if let Some(ep) = socket.local_endpoint() {
            return Some(ep);
        }
        let bound = *self.bound_endpoint.lock();
        bound.map(|bound| {
            let addr = bound
                .addr
                .unwrap_or(unspecified_addr_for_family(self.family));
            IpEndpoint::new(addr, bound.port)
        })
    }

    /// Return the remote endpoint of this TCP socket, or None if not connected.
    pub(crate) fn remote_endpoint(&self) -> Option<IpEndpoint> {
        let st = self.state();
        let mut guard = crate::net::NET_STACK.lock();
        let stack = guard.as_mut()?;
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        socket.remote_endpoint()
    }

    fn state(&self) -> Arc<TcpSocketState> {
        Arc::clone(&self.st.lock())
    }

    fn listener_shared(&self) -> Option<Arc<TcpListenerShared>> {
        self.listener.lock().as_ref().map(Arc::clone)
    }

    fn should_dual_stack_with_ipv4(&self, endpoint: IpListenEndpoint) -> bool {
        self.family == super::AF_INET6 as i32 && !self.ipv6_only() && endpoint.addr.is_none()
    }

    fn choose_refill_endpoint(&self, listener: &Arc<TcpListenerShared>) -> IpListenEndpoint {
        let endpoints = listener.stack_endpoints();
        let passive = listener.passive.lock();
        let mut best = endpoints[0];
        let mut best_count = usize::MAX;
        for endpoint in endpoints {
            let count = passive
                .iter()
                .filter(|st| st.listener_endpoint() == Some(endpoint))
                .count();
            if count < best_count {
                best = endpoint;
                best_count = count;
            }
        }
        best
    }

    fn trim_listener_slots(&self, listener: &Arc<TcpListenerShared>) -> Result<(), ERRNO> {
        let target = listener.backlog();
        let mut to_close = Vec::new();
        while listener.slot_count() > target {
            if let Some(st) = listener.pop_one_passive() {
                to_close.push(st);
            } else {
                break;
            }
        }
        if to_close.is_empty() {
            return Ok(());
        }

        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
        for st in to_close.iter() {
            st.clear_listener();
            st.clear_queued_for_accept();
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            if socket.is_open() {
                socket.abort();
            }
            st.orphaned.store(true, Ordering::Release);
            st.read_wait.wake_all();
            st.write_wait.wake_all();
        }
        stack.poll();
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    fn refill_listener_slots(&self, listener: &Arc<TcpListenerShared>) -> Result<(), ERRNO> {
        let target = listener.backlog().min(super::MAX_PASSIVE_LISTEN_SOCKETS);
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;

        while listener.slot_count() < target {
            let (_h, st) = stack.create_tcp_socket();
            let listen_endpoint = self.choose_refill_endpoint(listener);
            {
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                socket.listen(listen_endpoint).map_err(|_| ERRNO::EIO)?;
            }
            debug!(
                "Tcp listener refill: passive_handle={:?} endpoint={:?} slots={}/{}",
                st.handle,
                listen_endpoint,
                listener.slot_count() + 1,
                target
            );
            st.set_listener(Some(Arc::downgrade(listener)));
            st.set_listener_endpoint(listen_endpoint);
            listener.push_passive(Arc::clone(&st));
        }

        stack.poll();
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn bind(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        let port = if ep.port == 0 {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            stack.alloc_ephemeral_port()
        } else {
            ep.port
        };
        *self.bound_endpoint.lock() =
            Some(listen_endpoint_from_bind(IpEndpoint::new(ep.addr, port)));
        debug!(
            "Tcp bind: requested={} effective={:?}",
            ep,
            *self.bound_endpoint.lock()
        );
        Ok(())
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), ERRNO> {
        let endpoint = (*self.bound_endpoint.lock()).ok_or(ERRNO::EINVAL)?;

        info!(
            "Tcp listen: endpoint={:?} stack_endpoint={:?} backlog={}",
            endpoint,
            IpListenEndpoint {
                addr: stack_listen_addr_for_family(self.family, endpoint.addr),
                port: endpoint.port,
            },
            backlog
        );

        let backlog = normalize_backlog(backlog);
        let was_listening = self.listening.load(Ordering::Acquire);

        let listener = {
            let mut guard = self.listener.lock();
            match guard.as_ref() {
                Some(ls) => Arc::clone(ls),
                None => {
                    let ls = Arc::new(TcpListenerShared::new(self.family, endpoint, backlog));
                    *guard = Some(Arc::clone(&ls));
                    ls
                }
            }
        };

        listener.set_dual_stack_v4(self.should_dual_stack_with_ipv4(endpoint));
        listener.set_endpoint(endpoint);
        listener.set_backlog(backlog);

        let st = self.state();
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            if !listener.contains_passive(st.handle) && !st.is_listener_owned() {
                let listen_endpoint = listener.stack_endpoints()[0];
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.listen(listen_endpoint).is_ok() {
                    debug!(
                        "Tcp listen armed primary passive socket: handle={:?} endpoint={:?}",
                        st.handle, listen_endpoint
                    );
                    st.set_listener(Some(Arc::downgrade(&listener)));
                    st.set_listener_endpoint(listen_endpoint);
                    listener.push_passive(Arc::clone(&st));
                } else if !was_listening {
                    return Err(ERRNO::EADDRINUSE);
                }
            }
            stack.poll();
        }

        self.listening.store(true, Ordering::Release);
        self.trim_listener_slots(&listener)?;
        self.refill_listener_slots(&listener)?;
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn accept(&self) -> Result<(Arc<TcpSocketFile>, Option<IpEndpoint>), ERRNO> {
        if !self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }

        let listener = self.listener_shared().ok_or(ERRNO::EINVAL)?;

        loop {
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
            if let Some(st) = listener.pop_pending() {
                st.clear_queued_for_accept();
                debug!("Tcp accept: popped pending handle={:?}", st.handle);

                let mut was_closed = false;
                let (peer, local) = {
                    let mut guard = NET_STACK.lock();
                    let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                    let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                    if matches!(
                        socket.state(),
                        tcp_socket::State::Closed | tcp_socket::State::TimeWait
                    ) {
                        was_closed = true;
                        (None, None)
                    } else {
                        (socket.remote_endpoint(), socket.local_endpoint())
                    }
                };
                if was_closed {
                    debug!(
                        "Tcp accept: pending handle={:?} was already closed",
                        st.handle
                    );
                    st.orphaned.store(true, Ordering::Release);
                    continue;
                }

                self.refill_listener_slots(&listener)?;

                let accepted = Arc::new(TcpSocketFile {
                    family: self.family,
                    st: SpinNoIrqLock::new(Arc::clone(&st)),
                    bound_endpoint: SpinNoIrqLock::new(local.map(listen_endpoint_from_bind)),
                    listening: AtomicBool::new(false),
                    listener: SpinNoIrqLock::new(None),
                    ipv6_only: AtomicBool::new(self.ipv6_only()),
                    close_on_drop: AtomicBool::new(true),
                    recv_timeout_ns: AtomicU64::new(self.recv_timeout_ns()),
                    send_timeout_ns: AtomicU64::new(self.send_timeout_ns()),
                    // A freshly accepted socket must NOT inherit the listening
                    // socket's multicast memberships (CVE-2017-8890).
                    mcast_groups: SpinNoIrqLock::new(Vec::new()),
                });

                debug!(
                    "Tcp accept: accepted handle={:?} peer={:?}",
                    st.handle, peer
                );
                NEED_POLL.store(true, Ordering::Release);
                return Ok((accepted, peer));
            }

            listener
                .accept_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                    listener.has_pending() || crate::signal::has_unmasked_pending_signal()
                });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }

    pub(crate) fn connect(&self, mut ep: IpEndpoint) -> Result<(), ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            info!("Tcp connect failed: socket is listening");
            return Err(ERRNO::EINVAL);
        }
        if self.is_connected()? {
            return Err(ERRNO::EISCONN);
        }
        if ep.addr.is_unspecified() {
            ep.addr = if self.family == super::AF_INET6 as i32 {
                IpAddress::Ipv6(Ipv6Address::LOCALHOST)
            } else {
                IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))
            };
        }

        info!("Tcp connect: {}", ep);

        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let local_endpoint = {
                let mut bp = self.bound_endpoint.lock();
                match *bp {
                    Some(ep) if ep.port != 0 => ep,
                    Some(mut ep) => {
                        ep.port = stack.alloc_ephemeral_port();
                        *bp = Some(ep);
                        ep
                    }
                    None => {
                        let ep = IpListenEndpoint {
                            addr: None,
                            port: stack.alloc_ephemeral_port(),
                        };
                        *bp = Some(ep);
                        ep
                    }
                }
            };

            let st = self.state();
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            debug!(
                "Tcp connect attempt: handle={:?} local={:?} remote={} iface_addrs={:?} chosen_src={:?}",
                st.handle,
                local_endpoint,
                ep,
                stack.iface.ip_addrs(),
                stack.iface.get_source_address(&ep.addr)
            );
            socket
                .connect(stack.iface.context(), ep, local_endpoint)
                .map_err(|_| ERRNO::EADDRINUSE)?;
            debug!(
                "Tcp connect submitted: handle={:?} state={}",
                st.handle,
                tcp_state_name(socket.state())
            );

            stack.poll();
        }

        NEED_POLL.store(true, Ordering::Release);

        loop {
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                match socket.state() {
                    tcp_socket::State::Established | tcp_socket::State::CloseWait => {
                        debug!(
                            "Tcp connect complete: handle={:?} local={:?} remote={:?} state={}",
                            st.handle,
                            socket.local_endpoint(),
                            socket.remote_endpoint(),
                            tcp_state_name(socket.state())
                        );
                        return Ok(());
                    }
                    tcp_socket::State::Closed | tcp_socket::State::TimeWait => {
                        warn!(
                            "Tcp connect refused: handle={:?} local={:?} remote={:?} state={}",
                            st.handle,
                            socket.local_endpoint(),
                            socket.remote_endpoint(),
                            tcp_state_name(socket.state())
                        );
                        return Err(ERRNO::ECONNREFUSED);
                    }
                    _ => {}
                }
            }
            st.write_wait
                .wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                    self.connect_done() || crate::signal::has_unmasked_pending_signal()
                });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }

    pub(crate) fn shutdown(&self, how: i32) -> Result<(), ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::ENOTCONN);
        }

        let st = self.state();
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            if socket.remote_endpoint().is_none() {
                return Err(ERRNO::ENOTCONN);
            }
            match how {
                0 => {}
                1 | 2 => socket.close(),
                _ => return Err(ERRNO::EINVAL),
            }
            stack.poll();
        }

        st.read_wait.wake_all();
        st.write_wait.wake_all();
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn recv_into_user_buffer(&self, buf: &mut UserBuffer) -> Result<usize, ERRNO> {
        // debug!("tcp recv_into_user_buffer: total_len={}: {:?}", buf.len(), buf.buffers);
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
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
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    let mut total = 0usize;
                    for slice in buf.buffers.iter_mut() {
                        if slice.is_empty() {
                            continue;
                        }
                        if !socket.can_recv() {
                            break;
                        }
                        let n = socket.recv_slice(slice).map_err(|_| ERRNO::EIO)?;
                        total += n;
                        if n < slice.len() {
                            break;
                        }
                    }
                    if let Some(handle) = timeout_handle.take() {
                        socket_wait_mark_ready(handle);
                        cleanup_socket_wait(handle);
                    }
                    crate::net::perf_tcp_user_recv(total);
                    stack.poll_socket_recv_work();
                    NEED_POLL.store(true, Ordering::Release);
                    return Ok(total);
                }
                if !socket.may_recv() {
                    debug!(
                        "tcp recv eof: source_id={} buf_len={}",
                        st.source_id(),
                        buf.len()
                    );
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Ok(0);
                }
                debug!(
                    "tcp recv wait: source_id={} buf_len={} can_recv={} may_recv={}",
                    st.source_id(),
                    buf.len(),
                    socket.can_recv(),
                    socket.may_recv()
                );
            }
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(
                        deadline,
                        Arc::clone(&task),
                        Some(handle.timer_tag()),
                    );
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
                st.read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                        self.recv_ready()
                            || socket_wait_should_skip(handle)
                            || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.recv_ready() {
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
                st.read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                        self.recv_ready() || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
        }
    }

    pub(crate) fn send_from_user_buffer(&self, buf: &UserBuffer) -> Result<usize, ERRNO> {
        // debug!("tcp send_from_user_buffer: total_len={}: {:?}", buf.len(), buf.buffers);

        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
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
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.can_send() {
                    let mut total = 0usize;
                    for slice in buf.buffers.iter() {
                        let mut off = 0usize;
                        while off < slice.len() {
                            if !socket.can_send() {
                                break;
                            }
                            let n = socket.send_slice(&slice[off..]).map_err(|_| ERRNO::EIO)?;
                            if n == 0 {
                                if let Some(handle) = timeout_handle.take() {
                                    cleanup_socket_wait(handle);
                                }
                                return Err(ERRNO::EIO);
                            }
                            total += n;
                            off += n;
                        }
                        if !socket.can_send() {
                            break;
                        }
                    }
                    if total > 0 {
                        crate::net::perf_tcp_user_send(total);
                        stack.poll_socket_work_for(st.handle);
                        NEED_POLL.store(true, Ordering::Release);
                        if let Some(handle) = timeout_handle.take() {
                            socket_wait_mark_ready(handle);
                            cleanup_socket_wait(handle);
                        }
                        return Ok(total);
                    }
                }
                if !socket.may_send() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Ok(0);
                }
            }
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(
                        deadline,
                        Arc::clone(&task),
                        Some(handle.timer_tag()),
                    );
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
                st.write_wait
                    .wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                        self.send_ready()
                            || socket_wait_should_skip(handle)
                            || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.send_ready() {
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
                st.write_wait
                    .wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                        self.send_ready() || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
        }
    }

    fn connect_done(&self) -> bool {
        let st = self.state();
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        matches!(
            socket.state(),
            tcp_socket::State::Established
                | tcp_socket::State::CloseWait
                | tcp_socket::State::Closed
                | tcp_socket::State::TimeWait
        )
    }

    fn recv_ready(&self) -> bool {
        let st = self.state();
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        socket.can_recv() || !socket.may_recv()
    }

    fn send_ready(&self) -> bool {
        let st = self.state();
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        socket.can_send() || !socket.may_send()
    }
}

impl File for TcpSocketFile {
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
        self.read_at_result(_offset, buf).unwrap_or(0)
    }

    fn read_at_result(&self, _offset: usize, mut buf: UserBuffer) -> Result<usize, ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Ok(0);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
        self.recv_into_user_buffer(&mut buf)
    }

    fn read_bytes_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
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
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    let n = socket.recv_slice(buf).map_err(|_| ERRNO::EIO)?;
                    if let Some(handle) = timeout_handle.take() {
                        socket_wait_mark_ready(handle);
                        cleanup_socket_wait(handle);
                    }
                    crate::net::perf_tcp_user_recv(n);
                    stack.poll_socket_recv_work();
                    NEED_POLL.store(true, Ordering::Release);
                    return Ok(n);
                }
                if !socket.may_recv() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Ok(0);
                }
            }
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(
                        deadline,
                        Arc::clone(&task),
                        Some(handle.timer_tag()),
                    );
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
                st.read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                        self.recv_ready()
                            || socket_wait_should_skip(handle)
                            || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.recv_ready() {
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
                st.read_wait
                    .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                        self.recv_ready() || crate::signal::has_unmasked_pending_signal()
                    });
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
        if self.listening.load(Ordering::Acquire) {
            return Ok(0);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
        self.send_from_user_buffer(&buf)
    }

    fn write_bytes_at(&self, _offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
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
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.can_send() {
                    let n = socket.send_slice(buf).map_err(|_| ERRNO::EIO)?;
                    if n == 0 {
                        if let Some(handle) = timeout_handle.take() {
                            cleanup_socket_wait(handle);
                        }
                        return Err(ERRNO::EIO);
                    }
                    crate::net::perf_tcp_user_send(n);
                    stack.poll_socket_work_for(st.handle);
                    NEED_POLL.store(true, Ordering::Release);
                    if let Some(handle) = timeout_handle.take() {
                        socket_wait_mark_ready(handle);
                        cleanup_socket_wait(handle);
                    }
                    return Ok(n);
                }
            }
            if let Some(timeout_ns) = timeout_ns {
                if timeout_handle.is_none() {
                    let task = current_task().unwrap();
                    let handle = register_socket_wait(&task).ok_or(ERRNO::EAGAIN)?;
                    let now_ns = get_time_ns();
                    let deadline = now_ns.checked_add(timeout_ns).ok_or(ERRNO::EINVAL)?;
                    add_timer_with_socket_tag(
                        deadline,
                        Arc::clone(&task),
                        Some(handle.timer_tag()),
                    );
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
                st.write_wait
                    .wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                        self.send_ready()
                            || socket_wait_should_skip(handle)
                            || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    if let Some(handle) = timeout_handle.take() {
                        cleanup_socket_wait(handle);
                    }
                    return Err(ERRNO::EINTR);
                }
                if let Some(handle) = timeout_handle {
                    if matches!(socket_wait_state(handle), SocketWakeState::TimedOut) {
                        if self.send_ready() {
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
                st.write_wait
                    .wait_with_reason_or_skip(WaitReason::SocketWritable, || {
                        self.send_ready() || crate::signal::has_unmasked_pending_signal()
                    });
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            }
        }
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;

        if self.listening.load(Ordering::Acquire) {
            let Some(listener) = self.listener_shared() else {
                return POLLHUP;
            };
            if (events & POLLIN) != 0 && listener.has_pending() {
                ready |= POLLIN;
            }
            if ready != 0 {
                debug!(
                    "tcp poll(listening): source_id={} events={:#x} ready={:#x} pending={}",
                    listener.source_id(),
                    events,
                    ready,
                    listener.has_pending()
                );
            }
            return ready;
        }

        let st = self.state();
        let source_id = st.source_id();
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return POLLHUP;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);

        if (events & POLLIN) != 0 {
            if socket.can_recv() || !socket.may_recv() {
                ready |= POLLIN;
            }
            if !socket.may_recv() {
                ready |= POLLHUP;
            }
        }

        if (events & POLLOUT) != 0 && (socket.can_send() || !socket.may_send()) {
            ready |= POLLOUT;
        }

        if ready != 0 {
            debug!(
                "tcp poll: source_id={} events={:#x} ready={:#x} can_recv={} may_recv={} can_send={} may_send={}",
                source_id,
                events,
                ready,
                socket.can_recv(),
                socket.may_recv(),
                socket.can_send(),
                socket.may_send()
            );
        }

        ready
    }

    fn poll_source_id(&self) -> usize {
        if self.listening.load(Ordering::Acquire) {
            if let Some(listener) = self.listener_shared() {
                return listener.source_id();
            }
        }
        self.state().source_id()
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

impl Drop for TcpSocketFile {
    fn drop(&mut self) {
        if !self.close_on_drop.load(Ordering::Acquire) {
            return;
        }
        if self.listening.load(Ordering::Acquire) {
            self.listening.store(false, Ordering::Release);
            if let Some(listener) = self.listener.lock().take() {
                let source_id = listener.source_id();
                let states = listener.take_all_states();

                {
                    let mut guard = NET_STACK.lock();
                    if let Some(stack) = guard.as_mut() {
                        for st in states.iter() {
                            st.clear_listener();
                            st.clear_queued_for_accept();
                            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                            if socket.is_open() {
                                socket.abort();
                            }
                            st.orphaned.store(true, Ordering::Release);
                        }
                        stack.poll();
                    } else {
                        for st in states.iter() {
                            st.clear_listener();
                            st.clear_queued_for_accept();
                            st.orphaned.store(true, Ordering::Release);
                        }
                    }
                }

                for st in states.iter() {
                    st.read_wait.wake_all();
                    st.write_wait.wake_all();
                }
                listener.wake_accept_all();
                notify_poll_source(source_id, POLLIN | POLLOUT | POLLHUP);
                NEED_POLL.store(true, Ordering::Release);
                return;
            }
        }

        let st = self.state();
        let source_id = st.source_id();
        {
            let mut guard = NET_STACK.lock();
            if let Some(stack) = guard.as_mut() {
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.is_open() {
                    socket.close();
                }
                st.clear_listener();
                st.clear_queued_for_accept();
                st.orphaned.store(true, Ordering::Release);
                stack.poll();
            }
        }
        st.read_wait.wake_all();
        st.write_wait.wake_all();
        notify_poll_source(source_id, POLLIN | POLLOUT | POLLHUP);
        NEED_POLL.store(true, Ordering::Release);
    }
}

pub(crate) fn create_tcp_socket_file(family: i32) -> Option<Arc<TcpSocketFile>> {
    let mut guard = NET_STACK.lock();
    let stack = guard.as_mut()?;
    let (_handle, st) = stack.create_tcp_socket();
    Some(Arc::new(TcpSocketFile::new(st, family)))
}
