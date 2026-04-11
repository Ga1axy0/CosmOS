//! TCP socket implementation backed by smoltcp.

use core::any::Any;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use smoltcp::socket::tcp as tcp_socket;
use smoltcp::wire::IpEndpoint;

use crate::fs::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::net::{NEED_POLL, NET_STACK};
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};

pub(crate) struct TcpSocketState {
    pub(crate) handle: smoltcp::iface::SocketHandle,
    pub(crate) read_wait: WaitQueue,
    pub(crate) write_wait: WaitQueue,
    pub(crate) orphaned: AtomicBool,
}

impl TcpSocketState {
    pub(crate) fn new(handle: smoltcp::iface::SocketHandle) -> Self {
        Self {
            handle,
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            orphaned: AtomicBool::new(false),
        }
    }

    pub(crate) fn source_id(&self) -> usize {
        self as *const Self as usize
    }
}

pub(crate) struct TcpSocketFile {
    st: SpinNoIrqLock<Arc<TcpSocketState>>,
    bound_port: SpinNoIrqLock<Option<u16>>,
    listening: AtomicBool,
}

impl TcpSocketFile {
    fn new(st: Arc<TcpSocketState>) -> Self {
        Self {
            st: SpinNoIrqLock::new(st),
            bound_port: SpinNoIrqLock::new(None),
            listening: AtomicBool::new(false),
        }
    }

    fn state(&self) -> Arc<TcpSocketState> {
        Arc::clone(&self.st.lock())
    }

    pub(crate) fn bind(&self, port: u16) -> Result<(), ERRNO> {
        if port == 0 {
            return Err(ERRNO::EINVAL);
        }
        *self.bound_port.lock() = Some(port);
        Ok(())
    }

    pub(crate) fn listen(&self, _backlog: usize) -> Result<(), ERRNO> {
        let port = (*self.bound_port.lock()).ok_or(ERRNO::EINVAL)?;
        let st = self.state();
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            socket.listen(port).map_err(|_| ERRNO::EADDRINUSE)?;
            stack.poll();
        }
        self.listening.store(true, Ordering::Release);
        NEED_POLL.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn accept(&self) -> Result<(Arc<TcpSocketFile>, Option<IpEndpoint>), ERRNO> {
        if !self.listening.load(Ordering::Acquire) {
            return Err(ERRNO::EINVAL);
        }
        let port = (*self.bound_port.lock()).ok_or(ERRNO::EINVAL)?;

        loop {
            let cur_st = self.state();
            {
                let mut guard = NET_STACK.lock();
                let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(cur_st.handle);
                match socket.state() {
                    tcp_socket::State::Established | tcp_socket::State::CloseWait => {
                        let peer = socket.remote_endpoint();

                        let (_h2, new_listen_st) = stack.create_tcp_socket();
                        {
                            let s2 = stack
                                .sockets
                                .get_mut::<tcp_socket::Socket>(new_listen_st.handle);
                            s2.listen(port).map_err(|_| ERRNO::EIO)?;
                        }

                        *self.st.lock() = Arc::clone(&new_listen_st);

                        let accepted = Arc::new(TcpSocketFile {
                            st: SpinNoIrqLock::new(Arc::clone(&cur_st)),
                            bound_port: SpinNoIrqLock::new(Some(port)),
                            listening: AtomicBool::new(false),
                        });

                        stack.poll();
                        NEED_POLL.store(true, Ordering::Release);
                        return Ok((accepted, peer));
                    }
                    tcp_socket::State::Closed => return Err(ERRNO::ECONNABORTED),
                    _ => {}
                }
            }

            cur_st
                .read_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || {
                    self.accept_ready(cur_st.handle)
                });
        }
    }

    pub(crate) fn connect(&self, ep: IpEndpoint) -> Result<(), ERRNO> {
        {
            let mut guard = NET_STACK.lock();
            let stack = guard.as_mut().ok_or(ERRNO::ENETDOWN)?;
            self.listening.store(false, Ordering::Release);
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
            socket
                .connect(stack.iface.context(), ep, local_port)
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
            st.read_wait
                .wait_with_reason_or_skip(WaitReason::SocketReadable, || self.connect_done());
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
                            let n = socket
                                .send_slice(&slice[off..])
                                .map_err(|_| ERRNO::EIO)?;
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

    fn accept_ready(&self, handle: smoltcp::iface::SocketHandle) -> bool {
        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return true;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(handle);
        matches!(
            socket.state(),
            tcp_socket::State::Established | tcp_socket::State::CloseWait | tcp_socket::State::Closed
        )
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
        let st = self.state();

        let mut guard = NET_STACK.lock();
        let Some(stack) = guard.as_mut() else {
            return POLLHUP;
        };
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);

        if (events & POLLIN) != 0 {
            if self.listening.load(Ordering::Acquire) {
                if matches!(
                    socket.state(),
                    tcp_socket::State::Established | tcp_socket::State::CloseWait
                ) {
                    ready |= POLLIN;
                }
                if matches!(socket.state(), tcp_socket::State::Closed) {
                    ready |= POLLHUP;
                }
            } else {
                if socket.can_recv() || !socket.may_recv() {
                    ready |= POLLIN;
                }
                if !socket.may_recv() {
                    ready |= POLLHUP;
                }
            }
        }

        if (events & POLLOUT) != 0 && (socket.can_send() || !socket.may_send()) {
            ready |= POLLOUT;
        }

        ready
    }

    fn poll_source_id(&self) -> usize {
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
        let st = self.state();
        let source_id = st.source_id();
        {
            let mut guard = NET_STACK.lock();
            if let Some(stack) = guard.as_mut() {
                let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.is_open() {
                    if self.listening.load(Ordering::Acquire) {
                        socket.abort();
                    } else {
                        socket.close();
                    }
                }
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
