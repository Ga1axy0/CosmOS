use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueue, WaitReason};
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::fmt::Write;
use core::hint::spin_loop;
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fs::{BlockDevice, BlockRead, BlockWrite};
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
    submitted: AtomicUsize,
    completed: AtomicUsize,
    needs_flush: AtomicBool,
    last_submit_ns: AtomicUsize,
    last_completion_ns: AtomicUsize,
    last_stall_warn_ns: AtomicUsize,
}

// static mut READ_RECORDS: SpinNoIrqLock<([usize; 512], usize)> = SpinNoIrqLock::new(([0; 512], 0));

static READ_OPS: AtomicUsize = AtomicUsize::new(0);
static READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static WRITE_OPS: AtomicUsize = AtomicUsize::new(0);
static WRITE_BYTES: AtomicUsize = AtomicUsize::new(0);
static WAIT_POLLS: AtomicUsize = AtomicUsize::new(0);
static TASK_WAITS: AtomicUsize = AtomicUsize::new(0);
static TASK_WAIT_NS: AtomicUsize = AtomicUsize::new(0);
static COMPLETE_RECHECK_MISSES: AtomicUsize = AtomicUsize::new(0);
static COMPLETE_WRONG_TOKENS: AtomicUsize = AtomicUsize::new(0);
static IRQ_EMPTY_ISR_WITH_USED: AtomicUsize = AtomicUsize::new(0);
static DIAG_BLOCK_SUBMITS: AtomicUsize = AtomicUsize::new(0);
static DIAG_BLOCK_WAITS: AtomicUsize = AtomicUsize::new(0);
static DIAG_BLOCK_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static DIAG_BLOCK_IRQS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MANY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MANY_REQS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MANY_MAX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MANY_QUEUE_FULL_WAITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_REQS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_MAX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MANY_QUEUE_FULL_WAITS: AtomicUsize = AtomicUsize::new(0);

const VIRTIO_BLK_QUEUE_SIZE: usize = 16;
// With indirect descriptors a request consumes one descriptor in the main
// ring, so all queue entries can be in flight.  Devices which did not
// negotiate indirect descriptors report QueueFull after roughly five
// three-descriptor requests; the submission loops already treat that as
// backpressure and wait before retrying.
const MAX_READ_IN_FLIGHT: usize = VIRTIO_BLK_QUEUE_SIZE;
const MAX_WRITE_IN_FLIGHT: usize = VIRTIO_BLK_QUEUE_SIZE;
/// Split large contiguous reads so a single page-cache window can use the
/// same in-flight queue as fragmented ext4 reads.
const READ_BATCH_CHUNK_BLOCKS: usize = 128; // 64 KiB with 512-byte blocks.
const ADAPTIVE_COMPLETION_SPINS: usize = 8;
const STALL_WARN_AFTER_NS: usize = 500_000_000;
const STALL_WARN_INTERVAL_NS: usize = 1_000_000_000;

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

    fn name(self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
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
    submitted_ns: usize,
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
                submitted_ns: 0,
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
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_blocks(block_id, buf);
    }

    /// Read contiguous blocks from the virtio_blk device.
    fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        let _probe = crate::probe_scope!("virtio.read_blocks");
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

    /// Read multiple independent contiguous ranges with several requests in
    /// flight. The method remains synchronous to callers, but submission is
    /// decoupled from completion so fragmented ext4 reads can use the queue.
    fn read_blocks_many(&self, reads: &mut [BlockRead<'_>]) {
        let _probe = crate::probe_scope!("virtio.read_blocks_many");
        let total_reqs = reads.iter().filter(|read| !read.data.is_empty()).count();
        if total_reqs == 0 {
            return;
        }
        if total_reqs == 1 {
            if let Some(read) = reads.iter_mut().find(|read| !read.data.is_empty()) {
                if read.data.len() <= READ_BATCH_CHUNK_BLOCKS * ::fs::BLOCK_SZ {
                    self.read_blocks(read.start_block, read.data);
                    return;
                }
            }
        }

        // The block-cache/ext4 layers often produce one large contiguous
        // range. Split it into bounded requests so the same asynchronous
        // submission path is used for both contiguous and fragmented files.
        let mut chunks = Vec::new();
        for read in reads.iter_mut().filter(|read| !read.data.is_empty()) {
            let start_block = read.start_block;
            for (chunk_idx, data) in read
                .data
                .chunks_mut(READ_BATCH_CHUNK_BLOCKS * ::fs::BLOCK_SZ)
                .enumerate()
            {
                chunks.push(BlockRead {
                    start_block: start_block + chunk_idx * READ_BATCH_CHUNK_BLOCKS,
                    data,
                });
            }
        }
        self.submit_read_batch(&mut chunks);
    }

    /// Write a block to the virtio_blk device
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_blocks(block_id, buf);
    }

    /// Write contiguous blocks to the virtio_blk device.
    fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        let _probe = crate::probe_scope!("virtio.write_blocks");
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
        let _probe = crate::probe_scope!("virtio.write_blocks_many");
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
        let _multi_probe = crate::probe_scope!("virtio.write_multi_batch");
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
                submitted: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                needs_flush: AtomicBool::new(false),
                last_submit_ns: AtomicUsize::new(0),
                last_completion_ns: AtomicUsize::new(0),
                last_stall_warn_ns: AtomicUsize::new(0),
            })
    }

    /// Flush writes accepted by the virtio block device to its backing storage.
    ///
    /// The synchronous flush request in virtio-drivers assumes that no other
    /// request is in the queue.  Drain our asynchronous requests first, and
    /// avoid issuing a flush to read-only/unused devices.  The latter matters
    /// when QEMU exposes a boot image alongside the writable root disk.
    pub fn flush(&self) -> Result<(), VirtIoError> {
        if !self.needs_flush.load(Ordering::Acquire) {
            return Ok(());
        }

        while self.has_pending_requests() {
            self.pump_completions();
            if self.has_pending_requests() {
                self.wait_for_device_progress();
            }
        }

        if !self.needs_flush.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let result = self.inner.lock().flush();
        if result.is_err() {
            self.needs_flush.store(true, Ordering::Release);
        }
        result
    }

    fn submit_read_batch(&self, reads: &mut [BlockRead<'_>]) {
        let total_reqs = reads.iter().filter(|read| !read.data.is_empty()).count();
        if total_reqs == 0 {
            return;
        }
        let _multi_probe = crate::probe_scope!("virtio.read_multi_batch");
        #[cfg(feature = "io_perf_counters")]
        {
            READ_MANY_CALLS.fetch_add(1, Ordering::Relaxed);
            READ_MANY_REQS.fetch_add(total_reqs, Ordering::Relaxed);
        }

        let mut next = 0usize;
        let mut in_flight: Vec<Arc<RequestState>> = Vec::new();
        while next < reads.len() || !in_flight.is_empty() {
            while next < reads.len() && in_flight.len() < MAX_READ_IN_FLIGHT {
                let read = &mut reads[next];
                next += 1;
                if read.data.is_empty() {
                    continue;
                }
                assert!(read.data.len() % ::fs::BLOCK_SZ == 0);
                #[cfg(feature = "io_perf_counters")]
                {
                    READ_OPS.fetch_add(1, Ordering::Relaxed);
                    READ_BYTES.fetch_add(read.data.len(), Ordering::Relaxed);
                }
                match self.submit_read_request(read.start_block, read.data) {
                    Ok(request) => {
                        in_flight.push(request);
                        update_max_read_in_flight(in_flight.len());
                    }
                    Err(VirtIoError::QueueFull) => {
                        #[cfg(feature = "io_perf_counters")]
                        {
                            READ_MANY_QUEUE_FULL_WAITS.fetch_add(1, Ordering::Relaxed);
                            READ_OPS.fetch_sub(1, Ordering::Relaxed);
                            READ_BYTES.fetch_sub(read.data.len(), Ordering::Relaxed);
                        }
                        next -= 1;
                        break;
                    }
                    Err(err) => {
                        let capacity = self.inner.lock().capacity();
                        panic!(
                            "Error when submitting VirtIOBlk batched read: block_id={} buf_len={} capacity={} err={:?}",
                            read.start_block,
                            read.data.len(),
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
                            "VirtIOBlk batched read response error: block_id={} token={} buf_len={} capacity={} resp_status={:?}",
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
            } else if next < reads.len() {
                self.wait_for_device_progress();
            }
        }
    }

    fn submit_read_request(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> Result<Arc<RequestState>, VirtIoError> {
        let _probe = crate::probe_scope!("virtio.submit_read_request");
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
        let submitted_ns = now_ns();
        data.token = token;
        data.submitted_ns = submitted_ns;
        drop(data);
        self.pending.lock().insert(token, Arc::clone(&request));
        let diag = DIAG_BLOCK_SUBMITS.fetch_add(1, Ordering::Relaxed);
        if diag < 64 {
            debug!(
                "[diag][blk] submit self={:#x} op=read block={} len={} token={} pending={}",
                self as *const Self as usize,
                block_id,
                len,
                token,
                self.pending_request_count(),
            );
        }
        self.submitted.fetch_add(1, Ordering::Relaxed);
        self.last_submit_ns.store(submitted_ns, Ordering::Release);
        super::wake_worker();
        Ok(request)
    }

    fn submit_write_request(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> Result<Arc<RequestState>, VirtIoError> {
        let _probe = crate::probe_scope!("virtio.submit_write_request");
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
        let submitted_ns = now_ns();
        data.token = token;
        data.submitted_ns = submitted_ns;
        drop(data);
        self.pending.lock().insert(token, Arc::clone(&request));
        let diag = DIAG_BLOCK_SUBMITS.fetch_add(1, Ordering::Relaxed);
        if diag < 64 {
            debug!(
                "[diag][blk] submit self={:#x} op=write block={} len={} token={} pending={}",
                self as *const Self as usize,
                block_id,
                len,
                token,
                self.pending_request_count(),
            );
        }
        self.needs_flush.store(true, Ordering::Release);
        self.submitted.fetch_add(1, Ordering::Relaxed);
        self.last_submit_ns.store(submitted_ns, Ordering::Release);
        super::wake_worker();
        Ok(request)
    }

    fn wait_request(&self, request: &Arc<RequestState>) {
        let _probe = crate::probe_scope!("virtio.wait_request");
        loop {
            self.pump_completions();
            if request.done() {
                return;
            }

            if current_task().is_some() && crate::hal::local_irqs_enabled() {
                if self.adaptive_pump_until(|| request.done()) {
                    return;
                }
                if self.has_used_completions() {
                    super::wake_worker();
                    continue;
                }
                super::wake_worker();
                let diag = DIAG_BLOCK_WAITS.fetch_add(1, Ordering::Relaxed);
                if diag < 64 {
                    let data = request.inner.lock();
                    debug!(
                        "[diag][blk] wait self={:#x} op={} block={} token={} pending={} waiters={}",
                        self as *const Self as usize,
                        data.kind.name(),
                        data.block_id,
                        data.token,
                        self.pending_request_count(),
                        request.wait_queue.debug_waiter_count(),
                    );
                }
                let wait_start = now_ns();
                request
                    .wait_queue
                    .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || request.done());
                TASK_WAITS.fetch_add(1, Ordering::Relaxed);
                TASK_WAIT_NS.fetch_add(now_ns().saturating_sub(wait_start), Ordering::Relaxed);
                continue;
            }

            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
    }

    fn wait_for_batch_progress(&self, in_flight: &[Arc<RequestState>]) {
        let _probe = crate::probe_scope!("virtio.wait_for_batch_progress");
        loop {
            self.pump_completions();
            if in_flight.iter().any(|request| request.done()) {
                return;
            }

            if current_task().is_some() && crate::hal::local_irqs_enabled() {
                if self.adaptive_pump_until(|| in_flight.iter().any(|request| request.done())) {
                    return;
                }
                if self.has_used_completions() {
                    super::wake_worker();
                    continue;
                }
                super::wake_worker();
                let wait_start = now_ns();
                self.batch_wait_queue
                    .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                        in_flight.iter().any(|request| request.done())
                    });
                TASK_WAITS.fetch_add(1, Ordering::Relaxed);
                TASK_WAIT_NS.fetch_add(now_ns().saturating_sub(wait_start), Ordering::Relaxed);
                continue;
            }

            #[cfg(feature = "io_perf_counters")]
            WAIT_POLLS.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
    }

    fn wait_for_device_progress(&self) {
        let _probe = crate::probe_scope!("virtio.wait_for_device_progress");
        if current_task().is_some() && crate::hal::local_irqs_enabled() {
            if self.adaptive_pump_until(|| self.has_used_completions()) {
                return;
            }
            super::wake_worker();
            let wait_start = now_ns();
            self.batch_wait_queue
                .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                    self.has_used_completions() || !self.has_pending_requests()
                });
            TASK_WAITS.fetch_add(1, Ordering::Relaxed);
            TASK_WAIT_NS.fetch_add(now_ns().saturating_sub(wait_start), Ordering::Relaxed);
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

    /// Number of requests currently tracked as pending.
    pub fn pending_request_count(&self) -> usize {
        self.pending.lock().len()
    }

    /// Returns whether the virtqueue currently exposes at least one used entry.
    pub fn has_used_completions(&self) -> bool {
        let mut inner = self.inner.lock();
        // Order the CPU read of the DMA-written used ring after the device's
        // DMA writes (Acquire on used.idx alone does not suffice for device DMA).
        crate::drivers::virtio::virtio_dma_rmb();
        inner.peek_used().is_some()
    }

    fn pump_completions_limited(&self, count_budget: usize) -> usize {
        let mut completed = 0usize;
        while completed < count_budget {
            let mut device = self.inner.lock();
            // See `handle_irq`: an I/O read fence is required before reading the
            // DMA-written used ring, otherwise a just-completed entry can be
            // invisible and this pump drains nothing.
            crate::drivers::virtio::virtio_dma_rmb();
            let Some(token) = device.peek_used() else {
                break;
            };
            let request = self.pending.lock().remove(&token);
            let Some(request) = request else {
                let miss_count = COMPLETE_RECHECK_MISSES.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(
                    "[virtio_blk][complete] used token {} has no pending request; \
                     stop completion pump miss_count={} pending={} submitted={} completed={}",
                    token,
                    miss_count,
                    self.pending_request_count(),
                    self.submitted.load(Ordering::Relaxed),
                    self.completed.load(Ordering::Relaxed),
                );
                break;
            };

            let mut data = request.inner.lock();
            let kind = data.kind;
            let block_id = data.block_id;
            let diag = DIAG_BLOCK_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
            if diag < 64 {
                debug!(
                    "[diag][blk] complete self={:#x} op={} block={} token={} pending_before={}",
                    self as *const Self as usize,
                    kind.name(),
                    block_id,
                    token,
                    self.pending_request_count(),
                );
            }
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
            self.completed.fetch_add(1, Ordering::Relaxed);
            self.last_completion_ns.store(now_ns(), Ordering::Release);
            drop(data);
            drop(device);
            completed += 1;
            request.wait_queue.wake_all();
        }
        if completed != 0 {
            self.wake_batch_waiters();
        }
        completed
    }

    /// Drain at most `budget` completed virtqueue entries and wake their waiters.
    pub fn pump_completions_budget(&self, budget: usize) -> usize {
        self.pump_completions_limited(budget)
    }

    /// Drain every completion currently visible in the virtqueue.
    pub fn pump_completions(&self) -> bool {
        self.pump_completions_budget(usize::MAX) != 0
    }

    fn wake_batch_waiters(&self) -> usize {
        self.batch_wait_queue.wake_all()
    }

    /// Called from external interrupt path for this block device.
    pub fn handle_irq(&self) {
        let mut inner = self.inner.lock();
        let isr_set = !inner.ack_interrupt().is_empty();
        // The device writes the used-ring entry and increments used.idx via DMA,
        // THEN raises the IRQ. The Acquire load virtio-drivers uses to read
        // used.idx orders CPU-vs-CPU accesses only — it does NOT order against
        // the device's DMA writes. Without an I/O read fence here first,
        // `peek_used` can observe the used ring as still empty even though the
        // device has completed and asserted the interrupt; combined with an
        // already-acked ISR this made handle_irq return early, the entry became
        // visible only later with no further IRQ, and the worker/waiter slept
        // forever (the lost-IRQ stall). Fence BEFORE peeking so the DMA writes
        // are visible, then fall back to the queue state so a real completion
        // never fails to schedule the worker.
        crate::drivers::virtio::virtio_dma_rmb();
        // Even when the ISR reads back empty (a spurious EXTIOI re-fire after
        // the device already de-asserted, or a re-assertion racing the EOI), a
        // completion may still be sitting unread in the used ring.
        let has_used = inner.peek_used().is_some();
        let diag = DIAG_BLOCK_IRQS.fetch_add(1, Ordering::Relaxed);
        if diag < 64 {
            debug!(
                "[diag][blk] irq self={:#x} isr={} used={} pending={}",
                self as *const Self as usize,
                isr_set,
                has_used,
                self.pending_request_count(),
            );
        }
        if !isr_set && !has_used {
            return;
        }
        if !isr_set && has_used {
            let count = IRQ_EMPTY_ISR_WITH_USED.fetch_add(1, Ordering::Relaxed) + 1;
            if should_log_sample(count) {
                warn!(
                    "[virtio_blk][irq] empty ISR but used ring is non-empty count={} \
                     pending={} submitted={} completed={}",
                    count,
                    self.pending_request_count(),
                    self.submitted.load(Ordering::Relaxed),
                    self.completed.load(Ordering::Relaxed),
                );
            }
        }
        drop(inner);
        super::schedule_completion_work();
    }

    /// Emit WARN-level diagnostics if requests are stuck in flight.
    pub fn warn_if_stalled(
        &self,
        irq: u32,
        now_ns: usize,
        worker_waiters: usize,
        worker_work_pending: bool,
        worker_sleeps: usize,
        worker_wakes: usize,
        completion_events: usize,
        worker: &super::BlockWorkerDebugSnapshot,
    ) {
        let (pending_len, first_request) = {
            let pending = self.pending.lock();
            (
                pending.len(),
                pending
                    .iter()
                    .next()
                    .map(|(token, request)| (*token, Arc::clone(request))),
            )
        };
        if pending_len == 0 {
            return;
        }

        let last_submit = self.last_submit_ns.load(Ordering::Acquire);
        let last_completion = self.last_completion_ns.load(Ordering::Acquire);
        let last_activity = last_submit.max(last_completion);
        if last_activity == 0 {
            return;
        }
        let idle_ns = now_ns.saturating_sub(last_activity);
        if idle_ns < STALL_WARN_AFTER_NS {
            return;
        }

        let last_warn = self.last_stall_warn_ns.load(Ordering::Acquire);
        if now_ns.saturating_sub(last_warn) < STALL_WARN_INTERVAL_NS {
            return;
        }
        if self
            .last_stall_warn_ns
            .compare_exchange(last_warn, now_ns, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let has_used = self.has_used_completions();
        let first = first_request
            .map(|(token, request)| request_debug_summary(token, &request, now_ns))
            .unwrap_or(RequestDebugSummary::empty());
        warn!(
            "[virtio_blk][stall] irq={} pending={} used={} worker_waiters={} \
             worker_work_pending={} worker_sleeps={} worker_wakes={} completion_events={} \
             worker_task={:#x} worker_status={:?} worker_wait={:?} worker_on_cpu={} \
             worker_on_rq={} worker_last_cpu={} worker_has_wq={} worker_pending={:#x} \
             worker_mask={:#x} worker_resched={:?} worker_loops={} worker_pump_calls={} \
             worker_pump_completed={} worker_in_pump={} worker_last_loop_age_ms={:?} \
             worker_last_pump_age_ms={:?} worker_last_sched_op={:?} \
             submitted={} completed={} idle_ms={} last_submit_age_ms={} \
             last_completion_age_ms={} batch_waiters={} first_token={} first_block={} \
             first_op={} first_len={} first_done={} first_age_ms={} first_waiters={}",
            irq,
            pending_len,
            has_used,
            worker_waiters,
            worker_work_pending,
            worker_sleeps,
            worker_wakes,
            completion_events,
            worker.task_ptr,
            worker.status,
            worker.wait,
            worker.on_cpu,
            worker.on_rq,
            worker.last_cpu,
            worker.has_wq,
            worker.pending,
            worker.mask,
            worker.resched,
            worker.loops,
            worker.pump_calls,
            worker.pump_completed,
            worker.in_pump,
            worker.last_loop_age_ms,
            worker.last_pump_age_ms,
            worker.last_sched_op,
            self.submitted.load(Ordering::Relaxed),
            self.completed.load(Ordering::Relaxed),
            ns_to_ms(idle_ns),
            ns_to_ms(now_ns.saturating_sub(last_submit)),
            ns_to_ms(now_ns.saturating_sub(last_completion)),
            self.batch_wait_queue.debug_waiter_count(),
            first.token,
            first.block_id,
            first.op,
            first.len,
            first.done,
            first.age_ms,
            first.waiters,
        );
    }
}

struct RequestDebugSummary {
    token: u16,
    block_id: usize,
    op: &'static str,
    len: usize,
    done: bool,
    age_ms: usize,
    waiters: usize,
}

impl RequestDebugSummary {
    fn empty() -> Self {
        Self {
            token: u16::MAX,
            block_id: usize::MAX,
            op: "none",
            len: 0,
            done: false,
            age_ms: 0,
            waiters: 0,
        }
    }
}

fn request_debug_summary(
    token: u16,
    request: &Arc<RequestState>,
    now_ns: usize,
) -> RequestDebugSummary {
    let (block_id, op, len, done, submitted_ns) = {
        let data = request.inner.lock();
        (
            data.block_id,
            data.kind.name(),
            data.kind.len(),
            data.done,
            data.submitted_ns,
        )
    };
    RequestDebugSummary {
        token,
        block_id,
        op,
        len,
        done,
        age_ms: ns_to_ms(now_ns.saturating_sub(submitted_ns)),
        waiters: request.wait_queue.debug_waiter_count(),
    }
}

fn now_ns() -> usize {
    crate::timer::get_time_ns() as usize
}

fn ns_to_ms(ns: usize) -> usize {
    ns / 1_000_000
}

fn should_log_sample(count: usize) -> bool {
    count <= 16 || count.is_power_of_two()
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
    TASK_WAIT_NS.store(0, Ordering::Relaxed);
    COMPLETE_RECHECK_MISSES.store(0, Ordering::Relaxed);
    COMPLETE_WRONG_TOKENS.store(0, Ordering::Relaxed);
    IRQ_EMPTY_ISR_WITH_USED.store(0, Ordering::Relaxed);
    #[cfg(feature = "io_perf_counters")]
    {
        READ_MANY_CALLS.store(0, Ordering::Relaxed);
        READ_MANY_REQS.store(0, Ordering::Relaxed);
        READ_MANY_MAX_INFLIGHT.store(0, Ordering::Relaxed);
        READ_MANY_QUEUE_FULL_WAITS.store(0, Ordering::Relaxed);
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
    let _ = writeln!(&mut out, "  task_wait_ns {}", load(&TASK_WAIT_NS));
    #[cfg(feature = "io_perf_counters")]
    {
        let _ = writeln!(&mut out, "  read_many_calls {}", load(&READ_MANY_CALLS));
        let _ = writeln!(&mut out, "  read_many_reqs {}", load(&READ_MANY_REQS));
        let _ = writeln!(
            &mut out,
            "  read_many_max_inflight {}",
            load(&READ_MANY_MAX_INFLIGHT)
        );
        let _ = writeln!(
            &mut out,
            "  read_many_queue_full_waits {}",
            load(&READ_MANY_QUEUE_FULL_WAITS)
        );
    }
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
    let _ = writeln!(
        &mut out,
        "  irq_empty_isr_with_used {}",
        load(&IRQ_EMPTY_ISR_WITH_USED)
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
fn update_max_read_in_flight(value: usize) {
    READ_MANY_MAX_INFLIGHT.fetch_max(value, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
fn update_max_read_in_flight(_value: usize) {}

#[cfg(feature = "io_perf_counters")]
fn update_max_write_in_flight(value: usize) {
    WRITE_MANY_MAX_INFLIGHT.fetch_max(value, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
fn update_max_write_in_flight(_value: usize) {}
