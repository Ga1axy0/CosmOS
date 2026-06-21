use super::BlockDevice;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};
use alloc::{boxed::Box, string::String, vec::Vec};
use core::error;
use core::fmt::Write;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};
use fs::BlockWrite;
use virtio_drivers::{
    device::blk::{BlkReq, BlkResp, RespStatus, VirtIOBlk},
    transport::SomeTransport,
    Error as VirtIoError,
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
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_REQS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_MAX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_QUEUE_FULL_WAITS: AtomicUsize = AtomicUsize::new(0);

const VIRTIO_BLK_QUEUE_SIZE: usize = 16;
const VIRTIO_BLK_WRITE_DESCS: usize = 3;
const MAX_WRITE_IN_FLIGHT: usize = VIRTIO_BLK_QUEUE_SIZE / VIRTIO_BLK_WRITE_DESCS;

struct PendingWrite<'a> {
    token: u16,
    block_id: usize,
    data: &'a [u8],
    req: BlkReq,
    resp: BlkResp,
}

impl<'a> PendingWrite<'a> {
    fn new(block_id: usize, data: &'a [u8]) -> Self {
        Self {
            token: 0,
            block_id,
            data,
            req: BlkReq::default(),
            resp: BlkResp::default(),
        }
    }
}

impl BlockDevice for VirtIOBlock {
    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_blocks(block_id, buf);
    }

    /// Read contiguous blocks from the virtio_blk device.
    fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        assert!(buf.len() % ::fs::BLOCK_SZ == 0);
        #[cfg(feature = "io_perf_counters")]
        {
            READ_OPS.fetch_add(1, Ordering::Relaxed);
            READ_BYTES.fetch_add(buf.len(), Ordering::Relaxed);
        }
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
        #[cfg(feature = "io_perf_counters")]
        {
            WRITE_OPS.fetch_add(1, Ordering::Relaxed);
            WRITE_BYTES.fetch_add(buf.len(), Ordering::Relaxed);
        }
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

    /// Write multiple independent contiguous ranges with several requests in flight.
    fn write_blocks_many(&self, writes: &[BlockWrite<'_>]) {
        let total_reqs = writes.iter().filter(|write| !write.data.is_empty()).count();
        if total_reqs == 0 {
            return;
        }
        if total_reqs == 1 {
            if let Some(write) = writes.iter().find(|write| !write.data.is_empty()) {
                self.write_blocks(write.start_block, write.data);
            }
            return;
        }

        #[cfg(feature = "io_perf_counters")]
        {
            WRITE_MANY_CALLS.fetch_add(1, Ordering::Relaxed);
            WRITE_MANY_REQS.fetch_add(total_reqs, Ordering::Relaxed);
        }

        let mut next = 0usize;
        let mut in_flight: Vec<Box<PendingWrite<'_>>> = Vec::new();
        while next < writes.len() || !in_flight.is_empty() {
            while next < writes.len() && in_flight.len() < MAX_WRITE_IN_FLIGHT {
                let write = &writes[next];
                next += 1;
                if write.data.is_empty() {
                    continue;
                }
                assert!(write.data.len() % ::fs::BLOCK_SZ == 0);
                let mut pending = Box::new(PendingWrite::new(write.start_block, write.data));
                WRITE_OPS.fetch_add(1, Ordering::Relaxed);
                WRITE_BYTES.fetch_add(write.data.len(), Ordering::Relaxed);
                let token = unsafe {
                    let mut inner = self.inner.lock();
                    match inner.write_blocks_nb(
                        pending.block_id,
                        &mut pending.req,
                        pending.data,
                        &mut pending.resp,
                    ) {
                        Ok(token) => token,
                        Err(VirtIoError::QueueFull) => {
                            #[cfg(feature = "io_perf_counters")]
                            WRITE_MANY_QUEUE_FULL_WAITS.fetch_add(1, Ordering::Relaxed);
                            WRITE_OPS.fetch_sub(1, Ordering::Relaxed);
                            WRITE_BYTES.fetch_sub(write.data.len(), Ordering::Relaxed);
                            next -= 1;
                            break;
                        }
                        Err(err) => {
                            let capacity = inner.capacity();
                            panic!(
                                "Error when submitting VirtIOBlk batched write: block_id={} buf_len={} capacity={} err={:?}",
                                pending.block_id,
                                pending.data.len(),
                                capacity,
                                err
                            )
                        }
                    }
                };
                pending.token = token;
                in_flight.push(pending);
                update_max_write_in_flight(in_flight.len());
            }

            if !in_flight.is_empty() {
                let token = self.wait_owned_used_token(&in_flight);
                let idx = in_flight
                    .iter()
                    .position(|pending| pending.token == token)
                    .expect("ready token missing from batched write set");
                let mut pending = in_flight.swap_remove(idx);
                let result = unsafe {
                    self.inner.lock().complete_write_blocks(
                        pending.token,
                        &pending.req,
                        pending.data,
                        &mut pending.resp,
                    )
                };
                if let Err(err) = result {
                    let capacity = self.inner.lock().capacity();
                    panic!(
                        "Error when completing VirtIOBlk batched write: block_id={} token={} buf_len={} capacity={} resp_status={:?} err={:?}",
                        pending.block_id,
                        pending.token,
                        pending.data.len(),
                        capacity,
                        pending.resp.status(),
                        err
                    );
                }
                if pending.resp.status() != RespStatus::OK {
                    let capacity = self.inner.lock().capacity();
                    panic!(
                        "VirtIOBlk batched write response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                        pending.block_id,
                        pending.token,
                        pending.data.len(),
                        capacity,
                        pending.resp.status()
                    );
                }
            } else if next < writes.len() {
                WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
                spin_loop();
            }
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
        let irq_disabled = !crate::hal::local_irqs_enabled();
        if current_task().is_none() || irq_disabled {
            while !self.token_ready(token) {
                #[cfg(feature = "io_perf_counters")]
                WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
                error!("spin_loop");
                spin_loop();
            }
            return;
        }

        // Task context path: park current task and wait for precise token wakeup.
        crate::trap::assert_can_sleep("virtio_blk::wait_token");
        #[cfg(feature = "io_perf_counters")]
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

    fn wait_owned_used_token(&self, in_flight: &[Box<PendingWrite<'_>>]) -> u16 {
        loop {
            if let Some(token) = self.inner.lock().peek_used() {
                if in_flight.iter().any(|pending| pending.token == token) {
                    return token;
                }
            }
            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
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
    #[cfg(feature = "io_perf_counters")]
    {
        WRITE_MANY_CALLS.store(0, Ordering::Relaxed);
        WRITE_MANY_REQS.store(0, Ordering::Relaxed);
        WRITE_MANY_MAX_INFLIGHT.store(0, Ordering::Relaxed);
        WRITE_MANY_QUEUE_FULL_WAITS.store(0, Ordering::Relaxed);
    }
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
    #[cfg(feature = "io_perf_counters")]
    {
        let _ = writeln!(
            &mut out,
            "  write_many_calls {}",
            load(&WRITE_MANY_CALLS)
        );
        let _ = writeln!(&mut out, "  write_many_reqs {}", load(&WRITE_MANY_REQS));
        let _ = writeln!(
            &mut out,
            "  write_many_max_inflight {}",
            load(&WRITE_MANY_MAX_INFLIGHT)
        );
        let _ = writeln!(
            &mut out,
            "  write_many_queue_full_waits {}",
            load(&WRITE_MANY_QUEUE_FULL_WAITS)
        );
    }
    out
}

#[cfg(feature = "io_perf_counters")]
fn update_max_write_in_flight(value: usize) {
    WRITE_MANY_MAX_INFLIGHT.fetch_max(value, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
fn update_max_write_in_flight(_value: usize) {}
