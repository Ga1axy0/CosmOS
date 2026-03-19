use super::BlockDevice;
use crate::mm::{
    frame_alloc, frame_dealloc, kernel_token, FrameTracker, PageTable, PhysAddr, PhysPageNum,
    StepByOne, VirtAddr,
};
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};
use alloc::{collections::VecDeque, vec::Vec};
use lazy_static::*;
use virtio_drivers::{BlkResp, Hal, RespStatus, VirtIOBlk, VirtIOHeader};

#[allow(unused)]
const VIRTIO0: usize = 0x10001000;
/// VirtIOBlock device driver strcuture for virtio_blk device
pub struct VirtIOBlock {
    inner: SpinNoIrqLock<VirtIOBlk<'static, VirtioHal>>,
    wait_queue: WaitQueueKeyed<u16>,
    completed: SpinNoIrqLock<VecDeque<u16>>,
}

lazy_static! {
    /// The global io data queue for virtio_blk device
    static ref QUEUE_FRAMES: SpinNoIrqLock<Vec<FrameTracker>> = unsafe { SpinNoIrqLock::new(Vec::new()) };
}

impl BlockDevice for VirtIOBlock {
    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        let mut resp = BlkResp::default();
        let token = unsafe {
            self.inner
                .lock()
                .read_block_nb(block_id, buf, &mut resp)
                .expect("Error when submitting VirtIOBlk read")
        };
        self.wait_token(token);
        assert_eq!(
            resp.status(),
            RespStatus::Ok,
            "Error when completing VirtIOBlk read"
        );
    }
    /// Write a block to the virtio_blk device
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        let mut resp = BlkResp::default();
        let token = unsafe {
            self.inner
                .lock()
                .write_block_nb(block_id, buf, &mut resp)
                .expect("Error when submitting VirtIOBlk write")
        };
        self.wait_token(token);
        assert_eq!(
            resp.status(),
            RespStatus::Ok,
            "Error when completing VirtIOBlk write"
        );
    }
}

impl VirtIOBlock {
    #[allow(unused)]
    /// Create a new VirtIOBlock driver with VIRTIO0 base_addr for virtio_blk device
    pub fn new() -> Self {
        unsafe {
            Self {
                inner: SpinNoIrqLock::new(
                    VirtIOBlk::<VirtioHal>::new(&mut *(VIRTIO0 as *mut VirtIOHeader)).unwrap(),
                ),
                wait_queue: WaitQueueKeyed::new(),
                completed: SpinNoIrqLock::new(VecDeque::new()),
            }
        }
    }

    /// Try to initialise a VirtIO block device at `base_addr`.
    ///
    /// Returns `None` if there is no VirtIO block device at that address
    /// (e.g. the MMIO slot is empty or maps a different device type).
    pub fn try_new(base_addr: usize) -> Option<Self> {
        unsafe {
            VirtIOBlk::<VirtioHal>::new(&mut *(base_addr as *mut VirtIOHeader))
                .ok()
                .map(|blk| Self {
                    inner: SpinNoIrqLock::new(blk),
                    wait_queue: WaitQueueKeyed::new(),
                    completed: SpinNoIrqLock::new(VecDeque::new()),
                })
        }
    }

    fn wait_token(&self, token: u16) {
        loop {
            if self.take_completed_token(token) {
                return;
            }
            if current_task().is_some() {
                self.wait_queue
                    .wait_selected_with_reason(token, WaitReason::BlockDeviceIo);
                return;
            }

            // Early boot path: no schedulable task context exists yet,
            // so we cannot block via wait queue and must poll completions.
            self.collect_completed_tokens(false);
        }
    }

    fn take_completed_token(&self, token: u16) -> bool {
        let mut completed = self.completed.lock();
        if let Some(pos) = completed.iter().position(|&done| done == token) {
            completed.remove(pos);
            true
        } else {
            false
        }
    }

    fn collect_completed_tokens(&self, wake_waiters: bool) {
        let mut inner = self.inner.lock();
        let mut completed = self.completed.lock();
        while let Ok(token) = inner.pop_used() {
            if wake_waiters && self.wait_queue.wake_selected(token) {
                continue;
            }
            completed.push_back(token);
        }
    }

    /// Called from external interrupt path for this block device.
    pub fn handle_irq(&self) {
        let mut inner = self.inner.lock();
        if !inner.ack_interrupt() {
            return;
        }
        drop(inner);
        self.collect_completed_tokens(true);
    }
}

pub struct VirtioHal;

impl Hal for VirtioHal {
    /// allocate memory for virtio_blk device's io data queue
    fn dma_alloc(pages: usize) -> usize {
        let mut ppn_base = PhysPageNum(0);
        for i in 0..pages {
            let frame = frame_alloc().unwrap();
            if i == 0 {
                ppn_base = frame.ppn;
            }
            assert_eq!(frame.ppn.0, ppn_base.0 + i);
            QUEUE_FRAMES.lock().push(frame);
        }
        let pa: PhysAddr = ppn_base.into();
        pa.0
    }
    /// free memory for virtio_blk device's io data queue
    fn dma_dealloc(pa: usize, pages: usize) -> i32 {
        let pa = PhysAddr::from(pa);
        let mut ppn_base: PhysPageNum = pa.into();
        for _ in 0..pages {
            frame_dealloc(ppn_base);
            ppn_base.step();
        }
        0
    }
    /// translate physical address to virtual address for virtio_blk device
    fn phys_to_virt(addr: usize) -> usize {
        addr
    }
    /// translate virtual address to physical address for virtio_blk device
    fn virt_to_phys(vaddr: usize) -> usize {
        PageTable::from_token(kernel_token())
            .translate_va(VirtAddr::from(vaddr))
            .unwrap()
            .0
    }
}
