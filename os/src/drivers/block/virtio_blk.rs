use super::BlockDevice;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};
use core::hint::spin_loop;
use riscv::register::sstatus;
use virtio_drivers::{
    device::blk::{BlkReq, BlkResp, RespStatus, VirtIOBlk},
    transport::mmio::MmioTransport,
};

use crate::drivers::virtio::VirtioHal;

/// VirtIOBlock device driver strcuture for virtio_blk device
pub struct VirtIOBlock {
    inner: SpinNoIrqLock<VirtIOBlk<VirtioHal, MmioTransport<'static>>>,
    wait_queue: WaitQueueKeyed<u16>,
}

impl BlockDevice for VirtIOBlock {
    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        // debug!("Submitting VirtIOBlk read for block_id {}", block_id);
        let token = unsafe {
            self.inner
                .lock()
                .read_blocks_nb(block_id, &mut req, buf, &mut resp)
                .expect("Error when submitting VirtIOBlk read")
        };
        self.wait_token(token);
        unsafe {
            self.inner
                .lock()
                .complete_read_blocks(token, &req, buf, &mut resp)
                .expect("Error when completing VirtIOBlk read");
        }
        assert_eq!(
            resp.status(),
            RespStatus::OK,
            "Error when completing VirtIOBlk read"
        );
    }
    /// Write a block to the virtio_blk device
    fn write_block(&self, block_id: usize, buf: &[u8]) {
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let token = unsafe {
            self.inner
                .lock()
                .write_blocks_nb(block_id, &mut req, buf, &mut resp)
                .expect("Error when submitting VirtIOBlk write")
        };
        self.wait_token(token);
        unsafe {
            self.inner
                .lock()
                .complete_write_blocks(token, &req, buf, &mut resp)
                .expect("Error when completing VirtIOBlk write");
        }
        assert_eq!(
            resp.status(),
            RespStatus::OK,
            "Error when completing VirtIOBlk write"
        );
    }
}

impl VirtIOBlock {
    /// Build a wrapper from an initialized MMIO transport.
    pub fn try_new(transport: MmioTransport<'static>) -> Option<Self> {
        VirtIOBlk::<VirtioHal, _>::new(transport).ok().map(|blk| Self {
            inner: SpinNoIrqLock::new(blk),
            wait_queue: WaitQueueKeyed::new(),
        })
    }

    fn wait_token(&self, token: u16) {
        // TODO Enable kernel interrupt in more cases.
        let irq_disabled = !sstatus::read().sie();
        if current_task().is_none() || irq_disabled {
            while !self.token_ready(token) {
                spin_loop();
            }
            return;
        }

        // Task context path: park current task and wait for precise token wakeup.
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
