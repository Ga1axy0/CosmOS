use super::BlockDevice;
use crate::hal::hartid;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueue, WaitReason};
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::fmt::Write;
use core::hint::spin_loop;
use core::slice;
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
    pending: SpinNoIrqLock<BTreeMap<u16, Arc<RequestState>>>,
    batch_wait_queue: WaitQueue,
}

// static mut READ_RECORDS: SpinNoIrqLock<([usize; 512], usize)> = SpinNoIrqLock::new(([0; 512], 0));

static READ_OPS: AtomicUsize = AtomicUsize::new(0);
static READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static WRITE_OPS: AtomicUsize = AtomicUsize::new(0);
static WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
static WAIT_POLLS: AtomicUsize = AtomicUsize::new(0);
static TASK_WAITS: AtomicUsize = AtomicUsize::new(0);
static COMPLETE_RECHECK_MISSES: AtomicUsize = AtomicUsize::new(0);
static COMPLETE_WRONG_TOKENS: AtomicUsize = AtomicUsize::new(0);
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
const ADAPTIVE_COMPLETION_SPINS: usize = 32;

#[derive(Clone, Copy, Debug)]
enum RequestKind {
    Read { ptr: usize, len: usize },
    Write { ptr: usize, len: usize },
}

impl RequestKind {
    fn len(self) -> usize {
        match self {
            Self::Read { len, .. } | Self::Write { len, .. } => len,
        }
    }
}

struct RequestData {
    token: u16,
    block_id: usize,
    kind: RequestKind,
    req: BlkReq,
    resp: BlkResp,
    done: bool,
}

struct RequestState {
    inner: SpinNoIrqLock<RequestData>,
    wait_queue: WaitQueue,
}

impl RequestState {
    fn new_read(block_id: usize, buf: &mut [u8]) -> Arc<Self> {
        Arc::new(Self::new(
            block_id,
            RequestKind::Read {
                ptr: buf.as_mut_ptr() as usize,
                len: buf.len(),
            },
        ))
    }

    fn new_write(block_id: usize, data: &[u8]) -> Arc<Self> {
        Arc::new(Self::new(
            block_id,
            RequestKind::Write {
                ptr: data.as_ptr() as usize,
                len: data.len(),
            },
        ))
    }

    fn new(block_id: usize, kind: RequestKind) -> Self {
        Self {
            inner: SpinNoIrqLock::new(RequestData {
                token: 0,
                block_id,
                kind,
                req: BlkReq::default(),
                resp: BlkResp::default(),
                done: false,
            }),
            wait_queue: WaitQueue::new(),
        }
    }

    fn done(&self) -> bool {
        self.inner.lock().done
    }

    fn status(&self) -> RespStatus {
        self.inner.lock().resp.status()
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
        let request = self
            .submit_read_request(block_id, buf)
            .unwrap_or_else(|err| {
                let capacity = self.inner.lock().capacity();
                panic!(
                    "Error when submitting VirtIOBlk read: block_id={} buf_len={} capacity={} err={:?}",
                    block_id,
                    buf.len(),
                    capacity,
                    err
                )
            });
        self.wait_request(&request);
        if request.status() != RespStatus::OK {
            let capacity = self.inner.lock().capacity();
            let token = request.inner.lock().token;
            panic!(
                "VirtIOBlk read response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                request.status()
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
        let request = self
            .submit_write_request(block_id, buf)
            .unwrap_or_else(|err| {
                let capacity = self.inner.lock().capacity();
                panic!(
                    "Error when submitting VirtIOBlk write: block_id={} buf_len={} capacity={} err={:?}",
                    block_id,
                    buf.len(),
                    capacity,
                    err
                )
            });
        self.wait_request(&request);
        if request.status() != RespStatus::OK {
            let capacity = self.inner.lock().capacity();
            let token = request.inner.lock().token;
            panic!(
                "VirtIOBlk write response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                block_id,
                token,
                buf.len(),
                capacity,
                request.status()
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
        let mut in_flight: Vec<Arc<RequestState>> = Vec::new();
        while next < writes.len() || !in_flight.is_empty() {
            while next < writes.len() && in_flight.len() < MAX_WRITE_IN_FLIGHT {
                let write = &writes[next];
                next += 1;
                if write.data.is_empty() {
                    continue;
                }
                assert!(write.data.len() % ::fs::BLOCK_SZ == 0);
                WRITE_OPS.fetch_add(1, Ordering::Relaxed);
                WRITE_BYTES.fetch_add(write.data.len(), Ordering::Relaxed);
                match self.submit_write_request(write.start_block, write.data) {
                    Ok(request) => {
                        in_flight.push(request);
                        update_max_write_in_flight(in_flight.len());
                    }
                    Err(VirtIoError::QueueFull) => {
                        #[cfg(feature = "io_perf_counters")]
                        WRITE_MANY_QUEUE_FULL_WAITS.fetch_add(1, Ordering::Relaxed);
                        WRITE_OPS.fetch_sub(1, Ordering::Relaxed);
                        WRITE_BYTES.fetch_sub(write.data.len(), Ordering::Relaxed);
                        next -= 1;
                        break;
                    }
                    Err(err) => {
                        let capacity = self.inner.lock().capacity();
                        panic!(
                            "Error when submitting VirtIOBlk batched write: block_id={} buf_len={} capacity={} err={:?}",
                            write.start_block,
                            write.data.len(),
                            capacity,
                            err
                        )
                    }
                }
            }

            if !in_flight.is_empty() {
                self.pump_completions();
                let mut idx = 0;
                while idx < in_flight.len() {
                    if !in_flight[idx].done() {
                        idx += 1;
                        continue;
                    }
                    let request = in_flight.swap_remove(idx);
                    if request.status() != RespStatus::OK {
                        let data = request.inner.lock();
                        let capacity = self.inner.lock().capacity();
                        panic!(
                            "VirtIOBlk batched write response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
                            data.block_id,
                            data.token,
                            data.kind.len(),
                            capacity,
                            data.resp.status()
                        );
                    }
                }
                if !in_flight.is_empty() {
                    self.wait_for_batch_progress(&in_flight);
                }
            } else if next < writes.len() {
                self.wait_for_device_progress();
            }
        }
    }
}

impl VirtIOBlock {
    /// Build a wrapper from an initialized VirtIO transport.
    pub fn try_new(transport: SomeTransport<'static>) -> Option<Self> {
        VirtIOBlk::<VirtioHal, _>::new(transport)
            .ok()
            .map(|blk| Self {
                inner: SpinNoIrqLock::new(blk),
                pending: SpinNoIrqLock::new(BTreeMap::new()),
                batch_wait_queue: WaitQueue::new(),
            })
    }

    fn submit_read_request(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> Result<Arc<RequestState>, VirtIoError> {
        let request = RequestState::new_read(block_id, buf);
        let mut device = self.inner.lock();
        let mut data = request.inner.lock();
        let RequestKind::Read { ptr, len } = data.kind else {
            unreachable!();
        };
        let buf = unsafe { slice::from_raw_parts_mut(ptr as *mut u8, len) };
        let req = &mut data.req as *mut BlkReq;
        let resp = &mut data.resp as *mut BlkResp;
        let token = unsafe { device.read_blocks_nb(block_id, &mut *req, buf, &mut *resp)? };
        data.token = token;
        drop(data);
        self.pending.lock().insert(token, Arc::clone(&request));
        super::wake_worker();
        Ok(request)
    }

    fn submit_write_request(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> Result<Arc<RequestState>, VirtIoError> {
        let request = RequestState::new_write(block_id, buf);
        let mut device = self.inner.lock();
        let mut data = request.inner.lock();
        let RequestKind::Write { ptr, len } = data.kind else {
            unreachable!();
        };
        let buf = unsafe { slice::from_raw_parts(ptr as *const u8, len) };
        let req = &mut data.req as *mut BlkReq;
        let resp = &mut data.resp as *mut BlkResp;
        let token = unsafe { device.write_blocks_nb(block_id, &mut *req, buf, &mut *resp)? };
        data.token = token;
        drop(data);
        self.pending.lock().insert(token, Arc::clone(&request));
        super::wake_worker();
        Ok(request)
    }

    fn wait_request(&self, request: &Arc<RequestState>) {
        loop {
            self.pump_completions();
            if request.done() {
                return;
            }

            if current_task().is_some() && crate::hal::local_irqs_enabled() {
                if self.adaptive_pump_until(|| request.done()) {
                    return;
                }
                #[cfg(feature = "io_perf_counters")]
                TASK_WAITS.fetch_add(1, Ordering::Relaxed);
                if self.has_used_completions() {
                    super::wake_worker();
                    continue;
                }
                super::wake_worker();
                request
                    .wait_queue
                    .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || request.done());
                continue;
            }

            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
    }

    fn wait_for_batch_progress(&self, in_flight: &[Arc<RequestState>]) {
        loop {
            self.pump_completions();
            if in_flight.iter().any(|request| request.done()) {
                return;
            }

            if current_task().is_some() && crate::hal::local_irqs_enabled() {
                if self.adaptive_pump_until(|| in_flight.iter().any(|request| request.done())) {
                    return;
                }
                #[cfg(feature = "io_perf_counters")]
                TASK_WAITS.fetch_add(1, Ordering::Relaxed);
                if self.has_used_completions() {
                    super::wake_worker();
                    continue;
                }
                super::wake_worker();
                self.batch_wait_queue
                    .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                        in_flight.iter().any(|request| request.done())
                    });
                continue;
            }

            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
    }

    fn wait_for_device_progress(&self) {
        if current_task().is_some() && crate::hal::local_irqs_enabled() {
            if self.adaptive_pump_until(|| self.has_used_completions()) {
                return;
            }
            #[cfg(feature = "io_perf_counters")]
            TASK_WAITS.fetch_add(1, Ordering::Relaxed);
            super::wake_worker();
            self.batch_wait_queue
                .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                    self.has_used_completions() || !self.has_pending_requests()
                });
            return;
        }

        #[cfg(feature = "io_perf_counters")]
        WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
        spin_loop();
    }

    fn adaptive_pump_until(&self, is_ready: impl Fn() -> bool) -> bool {
        for _ in 0..ADAPTIVE_COMPLETION_SPINS {
            if is_ready() {
                return true;
            }
            self.pump_completions();
            if is_ready() {
                return true;
            }
            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
        false
    }

    /// Returns whether this device has requests waiting for completion.
    pub fn has_pending_requests(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// Returns whether the virtqueue currently exposes at least one used entry.
    pub fn has_used_completions(&self) -> bool {
        self.inner.lock().peek_used().is_some()
    }

    /// Drain completed virtqueue entries and wake the corresponding waiters.
    pub fn pump_completions(&self) -> bool {
        let mut completed_any = false;
        loop {
            let mut device = self.inner.lock();
            let Some(token) = device.peek_used() else {
                break;
            };
            let Some(request) = self.pending.lock().remove(&token) else {
                warn!(
                    "[virtio_blk] used token {} has no pending request; stop completion pump",
                    token
                );
                break;
            };

            let mut data = request.inner.lock();
            let kind = data.kind;
            let req = &data.req as *const BlkReq;
            let resp = &mut data.resp as *mut BlkResp;
            let result = unsafe {
                match kind {
                    RequestKind::Read { ptr, len } => {
                        let buf = slice::from_raw_parts_mut(ptr as *mut u8, len);
                        device.complete_read_blocks(token, &*req, buf, &mut *resp)
                    }
                    RequestKind::Write { ptr, len } => {
                        let buf = slice::from_raw_parts(ptr as *const u8, len);
                        device.complete_write_blocks(token, &*req, buf, &mut *resp)
                    }
                }
            };
            if let Err(err) = result {
                if matches!(err, VirtIoError::WrongToken) {
                    COMPLETE_WRONG_TOKENS.fetch_add(1, Ordering::Relaxed);
                }
                let capacity = device.capacity();
                panic!(
                    "Error when completing VirtIOBlk request: block_id={} token={} kind={:?} buf_len={} capacity={} resp_status={:?} err={:?}",
                    data.block_id,
                    token,
                    data.kind,
                    data.kind.len(),
                    capacity,
                    data.resp.status(),
                    err
                );
            }
            data.done = true;
            drop(data);
            drop(device);
            completed_any = true;
            request.wait_queue.wake_all();
        }
        if completed_any {
            self.wake_batch_waiters();
        }
        completed_any
    }

    fn wake_batch_waiters(&self) -> usize {
        self.batch_wait_queue.wake_all()
    }

    /// Called from external interrupt path for this block device.
    pub fn handle_irq(&self) {
        let mut inner = self.inner.lock();
        if inner.ack_interrupt().is_empty() {
            return;
        }
        crate::drivers::virtio::virtio_dma_rmb();
        drop(inner);
        super::schedule_completion_work();
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
    COMPLETE_RECHECK_MISSES.store(0, Ordering::Relaxed);
    COMPLETE_WRONG_TOKENS.store(0, Ordering::Relaxed);
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
    let _ = writeln!(
        &mut out,
        "  complete_recheck_misses {}",
        load(&COMPLETE_RECHECK_MISSES)
    );
    let _ = writeln!(
        &mut out,
        "  complete_wrong_tokens {}",
        load(&COMPLETE_WRONG_TOKENS)
    );
    #[cfg(feature = "io_perf_counters")]
    {
        let _ = writeln!(&mut out, "  write_many_calls {}", load(&WRITE_MANY_CALLS));
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
