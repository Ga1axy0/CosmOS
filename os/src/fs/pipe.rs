use super::File;
use crate::mm::UserBuffer;
use crate::poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use alloc::sync::{Arc, Weak};
use crate::fs::{Stat,StatMode}; 
use crate::task::{WaitQueue, WaitReason};
use core::any::Any;

/// IPC pipe
pub struct Pipe {
    readable: bool,
    writable: bool,
    buffer: Arc<SpinNoIrqLock<PipeRingBuffer>>,
}

impl Pipe {
    /// create readable pipe
    pub fn read_end_with_buffer(buffer: Arc<SpinNoIrqLock<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
        }
    }
    /// create writable pipe
    pub fn write_end_with_buffer(buffer: Arc<SpinNoIrqLock<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
        }
    }

    /// 返回共享 ring buffer 对应的 poll 事件源标识。
    fn source_id(&self) -> usize {
        Arc::as_ptr(&self.buffer) as usize
    }
}

const RING_BUFFER_SIZE: usize = 1024;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: [u8; RING_BUFFER_SIZE],
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    write_end: Option<Weak<Pipe>>,
    read_wait_queue: Arc<WaitQueue>,
    write_wait_queue: Arc<WaitQueue>,
}

impl PipeRingBuffer {
    pub fn new() -> Self {
        Self {
            arr: [0; RING_BUFFER_SIZE],
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            write_end: None,
            read_wait_queue: Arc::new(WaitQueue::new()),
            write_wait_queue: Arc::new(WaitQueue::new()),
        }
    }
    pub fn set_write_end(&mut self, write_end: &Arc<Pipe>) {
        self.write_end = Some(Arc::downgrade(write_end));
    }
    pub fn write_byte(&mut self, byte: u8) {
        self.status = RingBufferStatus::Normal;
        self.arr[self.tail] = byte;
        self.tail = (self.tail + 1) % RING_BUFFER_SIZE;
        if self.tail == self.head {
            self.status = RingBufferStatus::Full;
        }
    }
    pub fn read_byte(&mut self) -> u8 {
        self.status = RingBufferStatus::Normal;
        let c = self.arr[self.head];
        self.head = (self.head + 1) % RING_BUFFER_SIZE;
        if self.head == self.tail {
            self.status = RingBufferStatus::Empty;
        }
        c
    }
    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + RING_BUFFER_SIZE - self.head
        }
    }
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            RING_BUFFER_SIZE - self.available_read()
        }
    }
    pub fn all_write_ends_closed(&self) -> bool {
        self.write_end.as_ref().unwrap().upgrade().is_none()
    }
}

/// Return (read_end, write_end)
pub fn make_pipe() -> (Arc<Pipe>, Arc<Pipe>) {
    trace!("kernel: make_pipe");
    let buffer = Arc::new(unsafe { SpinNoIrqLock::new(PipeRingBuffer::new()) });
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
    buffer.lock().set_write_end(&write_end);
    (read_end, write_end)
}

impl File for Pipe {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn read_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        trace!("kernel: Pipe::read");
        assert!(self.readable());
        let want_to_read = buf.len();
        let mut buf_iter = buf.into_iter();
        let mut already_read = 0usize;
        loop {
            let mut ring_buffer = self.buffer.lock();
            let loop_read = ring_buffer.available_read();
            debug!("Pipe::read: want_to_read {}, already_read {}, loop_read {}", want_to_read, already_read, loop_read);
            if loop_read == 0 {
                // 只要本次调用已经读到数据，就立即短读返回，避免阻塞等待凑满用户缓冲区。
                if already_read > 0 {
                    return already_read;
                }
                if ring_buffer.all_write_ends_closed() {
                    return already_read;
                }
                let read_wait_queue = Arc::clone(&ring_buffer.read_wait_queue);
                drop(ring_buffer);
                read_wait_queue.wait_with_reason_or_skip(WaitReason::PipeReadable, || {
                    let ring_buffer = self.buffer.lock();
                    ring_buffer.available_read() > 0 || ring_buffer.all_write_ends_closed()
                });
                continue;
            }
            for _ in 0..loop_read {
                if let Some(byte_ref) = buf_iter.next() {
                    unsafe {
                        *byte_ref = ring_buffer.read_byte();
                    }
                    already_read += 1;
                    if already_read == want_to_read {
                        ring_buffer.write_wait_queue.wake_one();
                        notify_poll_source(self.source_id(), POLLOUT);
                        return want_to_read;
                    }
                } else {
                    ring_buffer.write_wait_queue.wake_one();
                    notify_poll_source(self.source_id(), POLLOUT);
                    return already_read;
                }
            }
            ring_buffer.write_wait_queue.wake_one();
            notify_poll_source(self.source_id(), POLLOUT);
        }
    }
    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        trace!("kernel: Pipe::write");
        assert!(self.writable());
        let want_to_write = buf.len();
        let mut buf_iter = buf.into_iter();
        let mut already_write = 0usize;
        loop {
            let mut ring_buffer = self.buffer.lock();
            let loop_write = ring_buffer.available_write();
            debug!("Pipe::write: want_to_write {}, already_write {}, loop_write {}", want_to_write, already_write, loop_write);
            if loop_write == 0 {
                let write_wait_queue = Arc::clone(&ring_buffer.write_wait_queue);
                drop(ring_buffer);
                write_wait_queue.wait_with_reason_or_skip(WaitReason::PipeWritable, || {
                    self.buffer.lock().available_write() > 0
                });
                continue;
            }
            // write at most loop_write bytes
            for _ in 0..loop_write {
                if let Some(byte_ref) = buf_iter.next() {
                    ring_buffer.write_byte(unsafe { *byte_ref });
                    already_write += 1;
                    if already_write == want_to_write {
                        ring_buffer.read_wait_queue.wake_one();
                        notify_poll_source(self.source_id(), POLLIN);
                        return want_to_write;
                    }
                } else {
                    ring_buffer.read_wait_queue.wake_one();
                    notify_poll_source(self.source_id(), POLLIN);
                    return already_write;
                }
            }
            ring_buffer.read_wait_queue.wake_one();
            notify_poll_source(self.source_id(), POLLIN);
        }
    }
    fn poll(&self, events: u16) -> u16 {
        const POLLIN: u16 = 0x001;
        const POLLOUT: u16 = 0x004;
        const POLLHUP: u16 = 0x010;

        let mut ready = 0u16;
        let ring_buffer = self.buffer.lock();
        if self.readable && (events & POLLIN) != 0 {
            if ring_buffer.available_read() > 0 {
                ready |= POLLIN;
            }
            if ring_buffer.all_write_ends_closed() {
                ready |= POLLHUP;
            }
        }
        if self.writable && (events & POLLOUT) != 0 && ring_buffer.available_write() > 0 {
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
            ino: 0,
            mode: StatMode::FIFO,
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

impl Drop for Pipe {
    fn drop(&mut self) {
        let source_id = self.source_id();
        let ring_buffer = self.buffer.lock();
        ring_buffer.read_wait_queue.wake_all();
        ring_buffer.write_wait_queue.wake_all();
        drop(ring_buffer);
        notify_poll_source(source_id, POLLIN | POLLOUT | POLLHUP);
    }
}
