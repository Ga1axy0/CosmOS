//! TCP socket implementation backed by smoltcp.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use smoltcp::socket::tcp as tcp_socket;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{NEED_POLL, NET_STACK};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};

const SOMAXCONN: usize = 128;

#[inline]
fn normalize_backlog(backlog: usize) -> usize {
    backlog.clamp(1, SOMAXCONN)
}

#[inline]
fn loopback_connect_local_endpoint(remote: IpEndpoint, port: u16) -> IpListenEndpoint {
    let addr = match remote.addr {
        IpAddress::Ipv4(v4) if v4.as_bytes()[0] == 127 => {
            Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)))
        }
        _ => None,
    };
    IpListenEndpoint { addr, port }
}

pub(crate) struct TcpListenerShared {
    port: AtomicUsize,
    backlog: AtomicUsize,
    pending: SpinNoIrqLock<VecDeque<Arc<TcpSocketState>>>,
    passive: SpinNoIrqLock<Vec<Arc<TcpSocketState>>>,
    accept_wait: WaitQueue,
}

impl TcpListenerShared {
    fn new(port: u16, backlog: usize) -> Self {
        Self {
            port: AtomicUsize::new(port as usize),
            backlog: AtomicUsize::new(backlog),
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

    fn set_port(&self, port: u16) {
        self.port.store(port as usize, Ordering::Release);
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
    queued_for_accept: AtomicBool,
}

impl TcpSocketState {
    pub(crate) fn new(handle: smoltcp::iface::SocketHandle) -> Self {
        Self {
            handle,
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            orphaned: AtomicBool::new(false),
            listener: SpinNoIrqLock::new(None),
            queued_for_accept: AtomicBool::new(false),
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
}

/// Called by net poll path to move one passive listener socket into the listener pending queue
/// once a TCP handshake is established.
pub(crate) fn queue_listener_connection_if_ready(
    st: &Arc<TcpSocketState>,
    state: tcp_socket::State,
) -> Option<usize> {
    if !matches!(state, tcp_socket::State::Established | tcp_socket::State::CloseWait) {
        return None;
    }
    let listener = st.listener_shared()?;
    if !st.try_mark_queued_for_accept() {
        return None;
    }

    listener.remove_passive(st.handle);
    listener.push_pending(Arc::clone(st));
    st.clear_listener();
    listener.wake_accept_one();
    Some(listener.source_id())
}

pub(crate) struct TcpSocketFile {
    st: SpinNoIrqLock<Arc<TcpSocketState>>,
    bound_port: SpinNoIrqLock<Option<u16>>,
    listening: AtomicBool,
    listener: SpinNoIrqLock<Option<Arc<TcpListenerShared>>>,
}

impl TcpSocketFile {
    fn new(st: Arc<TcpSocketState>) -> Self {
        Self {
            st: SpinNoIrqLock::new(st),
            bound_port: SpinNoIrqLock::new(None),
            listening: AtomicBool::new(false),
            listener: SpinNoIrqLock::new(None),
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

    /// Return the local endpoint of this TCP socket, or None if unavailable.
    pub(crate) fn local_endpoint(&self) -> Option<IpEndpoint> {
        let st = self.state();
        let mut guard = crate::net::NET_STACK.lock();
        let stack = guard.as_mut()?;
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        socket.local_endpoint()
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
        let target = listener.backlog();
        let mut guard = NET_STACK.lock();
        let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;

        while listener.slot_count() < target {
            let (_h, st) = stack.create_tcp_socket();
            {
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                socket.listen(listener.port()).map_err(|_| ERRNO::EIO)?;
            }
            st.set_listener(Some(Arc::downgrade(listener)));
            listener.push_passive(Arc::clone(&st));
        }

        stack.poll();
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn bind(&self, port: u16) -> Result<(), ERRNO> {
        if port == 0 {
            return Err(ERRNO::EINVAL);
        }
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        *self.bound_port.lock() = Some(port);
        Ok(())
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), ERRNO> {
        let port = (*self.bound_port.lock()).ok_or(ERRNO::EINVAL)?;
        let backlog = normalize_backlog(backlog);
        let was_listening = self.listening.load(Ordering::Acquire);

        let listener = {
            let mut guard = self.listener.lock();
            match guard.as_ref() {
                Some(ls) => Arc::clone(ls),
                None => {
                    let ls = Arc::new(TcpListenerShared::new(port, backlog));
                    *guard = Some(Arc::clone(&ls));
                    ls
                }
            }
        };

        listener.set_port(port);
        listener.set_backlog(backlog);

        let st = self.state();
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            if !listener.contains_passive(st.handle) && !st.is_listener_owned() {
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.listen(port).is_ok() {
                    st.set_listener(Some(Arc::downgrade(&listener)));
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
        let port = listener.port();

        loop {
            if let Some(st) = listener.pop_pending() {
                st.clear_queued_for_accept();

                let mut was_closed = false;
                let peer = {
                    let mut guard = NET_STACK.lock();
                    let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                    let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                    if matches!(socket.state(), tcp_socket::State::Closed | tcp_socket::State::TimeWait)
                    {
                        was_closed = true;
                        None
                    } else {
                        socket.remote_endpoint()
                    }
                };
                if was_closed {
                    st.orphaned.store(true, Ordering::Release);
                    continue;
                }

                self.refill_listener_slots(&listener)?;

                let accepted = Arc::new(TcpSocketFile {
                    st: SpinNoIrqLock::new(Arc::clone(&st)),
                    bound_port: SpinNoIrqLock::new(Some(port)),
                    listening: AtomicBool::new(false),
                    listener: SpinNoIrqLock::new(None),
                });

                NEED_POLL.store(true, Ordering::Release);
                return Ok((accepted, peer));
            }

            listener
                .accept_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || listener.has_pending());
        }
    }

    pub(crate) fn connect(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }

        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let local_port = {
                let mut bp = self.bound_port.lock();
                match *bp {
                    Some(p) => p,
                    None => {
                        let p = stack.alloc_ephemeral_port();
                        *bp = Some(p);
                        p
                    }
                }
            };

            let st = self.state();
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            let local_ep = loopback_connect_local_endpoint(ep, local_port);
            socket
                .connect(stack.iface.context(), ep, local_ep)
                .map_err(|_| ERRNO::EADDRINUSE)?;

            stack.poll();
        }

        NEED_POLL.store(true, Ordering::Release);

        loop {
            let st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                match socket.state() {
                    tcp_socket::State::Established | tcp_socket::State::CloseWait => return Ok(()),
                    tcp_socket::State::Closed | tcp_socket::State::TimeWait => {
                        return Err(ERRNO::ECONNREFUSED)
                    }
                    _ => {}
                }
            }
            st.write_wait
                .wait_with_reason_or_skip(WaitReason::SocketWritable, || self.connect_done());
        }
    }

    fn recv_into_user_buffer(&self, buf: &mut UserBuffer) -> Result<usize, ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
        loop {
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
                    return Ok(total);
                }
                if !socket.may_recv() {
                    return Ok(0);
                }
            }
            st.read_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || self.recv_ready());
        }
    }

    fn send_from_user_buffer(&self, buf: &UserBuffer) -> Result<usize, ERRNO> {
        if self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        if buf.len() == 0 {
            return Ok(0);
        }
        loop {
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
                        stack.poll();
                        NEED_POLL.store(true, Ordering::Release);
                        return Ok(total);
                    }
                }
                if !socket.may_send() {
                    return Ok(0);
                }
            }
            st.write_wait
                .wait_with_reason_or_skip(WaitReason::SocketWritable, || self.send_ready());
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

    fn read_at(&self, _offset: usize, mut buf: UserBuffer) -> usize {
        if self.listening.load(Ordering::Acquire) {
            return 0;
        }
        if buf.len() == 0 {
            return 0;
        }
        self.recv_into_user_buffer(&mut buf).unwrap_or(0)
    }

    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        if self.listening.load(Ordering::Acquire) {
            return 0;
        }
        if buf.len() == 0 {
            return 0;
        }
        self.send_from_user_buffer(&buf).unwrap_or(0)
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
            return ready;
        }

        let st = self.state();
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

pub(crate) fn create_tcp_socket_file() -> Option<Arc<TcpSocketFile>> {
    let mut guard = NET_STACK.lock();
    let stack = guard.as_mut()?;
    let (_handle, st) = stack.create_tcp_socket();
    Some(Arc::new(TcpSocketFile::new(st)))
}
