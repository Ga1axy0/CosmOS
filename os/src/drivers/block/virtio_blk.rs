use super::BlockDevice;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};
use core::hint::spin_loop;
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

impl BlockDevice for VirtIOBlock {
    /// Read a block from the virtio_blk device
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
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
