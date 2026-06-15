use super::BlockDevice;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};
use alloc::string::String;
use core::fmt::Write;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};
use virtio_drivers::{
    device::blk::{BlkReq, BlkResp, RespStatus, VirtIOBlk},
    transport::SomeTransport,
};

use crate::drivers::virtio::VirtioHal;

/// VirtIOBlock device driver strcuture for virtio_blk device
pub struct VirtIOBlock {
    inner: SpinNoIrqLock<VirtIOBlk<VirtioHal, SomeTransport<'static>>>,
    wait_queue: WaitQueueKeyed<u16>,
}

// static mut READ_RECORDS: SpinNoIrqLock<([usize; 512], usize)> = SpinNoIrqLock::new(([0; 512], 0));

static READ_OPS: AtomicUsize = AtomicUsize::new(0);
static READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static WRITE_OPS: AtomicUsize = AtomicUsize::new(0);
static WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
static WAIT_POLLS: AtomicUsize = AtomicUsize::new(0);
static TASK_WAITS: AtomicUsize = AtomicUsize::new(0);

impl BlockDevice for VirtIOBlock {
    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_blocks(block_id, buf);
    }

    /// Read contiguous blocks from the virtio_blk device.
    fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        assert!(buf.len() % ::fs::BLOCK_SZ == 0);
        READ_OPS.fetch_add(1, Ordering::Relaxed);
        READ_BYTES.fetch_add(buf.len(), Ordering::Relaxed);
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        
        // unsafe {
        //     let mut records = READ_RECORDS.lock();
        //     let idx = records.1 % 512;
        //     records.0[idx] = block_id;
        //     records.1 += 1;
        //     if idx.is_multiple_of(512) {
        //         // Debug-print the inner array, not the lock guard (which doesn't implement Debug).
        //         warn!(
        //             "Recent 512 VirtIOBlk read block_ids: {:?}",
        //             &records.0
        //         );
        //     }
        // }

        // debug!("Submitting VirtIOBlk read for block_id {}", block_id);
        let token = unsafe {
            self.inner
                .lock()
                .read_blocks_nb(block_id, &mut req, buf, &mut resp)
                .unwrap_or_else(|err| {
                    let capacity = self.inner.lock().capacity();
                    panic!(
                        "Error when submitting VirtIOBlk read: block_id={} buf_len={} capacity={} err={:?}",
                        block_id,
                        buf.len(),
                        capacity,
                        err
                    )
                })
        };
        self.wait_token(token);
        let result = unsafe {
            self.inner
                .lock()
                .complete_read_blocks(token, &req, buf, &mut resp)
        };
        if let Err(err) = result {
            let capacity = self.inner.lock().capacity();
            panic!(
                "Error when completing VirtIOBlk read: block_id={} token={} buf_len={} capacity={} resp_status={:?} err={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                resp.status(),
                err
            );
        }
        if resp.status() != RespStatus::OK {
            let capacity = self.inner.lock().capacity();
            panic!(
                "VirtIOBlk read response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                resp.status()
            );
        }
    }
    /// Write a block to the virtio_blk device
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_blocks(block_id, buf);
    }

    /// Write contiguous blocks to the virtio_blk device.
    fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        assert!(buf.len() % ::fs::BLOCK_SZ == 0);
        WRITE_OPS.fetch_add(1, Ordering::Relaxed);
        WRITE_BYTES.fetch_add(buf.len(), Ordering::Relaxed);
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = unsafe {
            self.inner
                .lock()
                .write_blocks_nb(block_id, &mut req, buf, &mut resp)
                .unwrap_or_else(|err| {
                    let capacity = self.inner.lock().capacity();
                    panic!(
                        "Error when submitting VirtIOBlk write: block_id={} buf_len={} capacity={} err={:?}",
                        block_id,
                        buf.len(),
                        capacity,
                        err
                    )
                })
        };
        self.wait_token(token);
        let result = unsafe {
            self.inner
                .lock()
                .complete_write_blocks(token, &req, buf, &mut resp)
        };
        if let Err(err) = result {
            let capacity = self.inner.lock().capacity();
            panic!(
                "Error when completing VirtIOBlk write: block_id={} token={} buf_len={} capacity={} resp_status={:?} err={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                resp.status(),
                err
            );
        }
        if resp.status() != RespStatus::OK {
            let capacity = self.inner.lock().capacity();
            panic!(
                "VirtIOBlk write response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                resp.status()
            );
        }
    }
}

impl VirtIOBlock {
    /// Build a wrapper from an initialized VirtIO transport.
    pub fn try_new(transport: SomeTransport<'static>) -> Option<Self> {
        VirtIOBlk::<VirtioHal, _>::new(transport).ok().map(|blk| Self {
            inner: SpinNoIrqLock::new(blk),
            wait_queue: WaitQueueKeyed::new(),
        })
    }

    fn wait_token(&self, token: u16) {
        // TODO Enable kernel interrupt in more cases.
        let irq_disabled = !crate::hal::local_irqs_enabled();
        if current_task().is_none() || irq_disabled {
            while !self.token_ready(token) {
                WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
                spin_loop();
            }
            return;
        }

        // Task context path: park current task and wait for precise token wakeup.
        TASK_WAITS.fetch_add(1, Ordering::Relaxed);
        self.wait_queue
            .wait_selected_with_reason_or_skip(token, WaitReason::BlockDeviceIo, || {
                self.token_ready(token)
            });
    }

    fn token_ready(&self, token: u16) -> bool {
        let mut inner = self.inner.lock();
        matches!(inner.peek_used(), Some(ready) if ready == token)
    }

    /// Called from external interrupt path for this block device.
    pub fn handle_irq(&self) {
        let mut inner = self.inner.lock();
        if inner.ack_interrupt().is_empty() {
            return;
        }
        let ready = inner.peek_used();
        drop(inner);
        if let Some(token) = ready {
            self.wait_queue.wake_selected(token);
        }
    }
}

fn load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

pub fn reset_perf_counters() {
    READ_OPS.store(0, Ordering::Relaxed);
    READ_BYTES.store(0, Ordering::Relaxed);
    WRITE_OPS.store(0, Ordering::Relaxed);
    WRITE_BYTES.store(0, Ordering::Relaxed);
    WAIT_POLLS.store(0, Ordering::Relaxed);
    TASK_WAITS.store(0, Ordering::Relaxed);
}

pub fn render_perf_counters() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "virtio_blk:");
    let _ = writeln!(&mut out, "  read_ops {}", load(&READ_OPS));
    let _ = writeln!(&mut out, "  read_bytes {}", load(&READ_BYTES));
    let _ = writeln!(&mut out, "  write_ops {}", load(&WRITE_OPS));
    let _ = writeln!(&mut out, "  write_bytes {}", load(&WRITE_BYTES));
    let _ = writeln!(&mut out, "  wait_polls {}", load(&WAIT_POLLS));
    let _ = writeln!(&mut out, "  task_waits {}", load(&TASK_WAITS));
    out
}
