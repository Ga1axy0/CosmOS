//! VirtIO network driver wrapper used by the kernel network stack.

use alloc::vec;
use alloc::vec::Vec;
use core::array;
use core::hint::spin_loop;

use virtio_drivers::{
    device::net::VirtIONetRaw,
    transport::mmio::MmioTransport,
    Error,
};

use crate::drivers::virtio::VirtioHal;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, WaitQueueKeyed, WaitReason};

/// Receive buffer length for one Ethernet frame + virtio header.
const RX_BUF_LEN: usize = 2048;
/// Queue size used by this driver wrapper.
const QUEUE_SIZE: usize = 16;

/// VirtIO network device wrapper.
///
/// This wrapper keeps RX buffers pre-posted and provides:
/// - non-blocking receive (`try_recv`)
/// - blocking transmit with token-keyed precise wakeup (`send`)
pub struct VirtIONetDevice {
    irq: u32,
    mac: [u8; 6],
    inner: SpinNoIrqLock<VirtIONetRaw<VirtioHal, MmioTransport<'static>, QUEUE_SIZE>>,
    tx_wait_queue: WaitQueueKeyed<u16>,
    rx_slots: SpinNoIrqLock<[Option<Vec<u8>>; QUEUE_SIZE]>,
}

impl VirtIONetDevice {
    /// Try to create one device instance from an already initialized transport.
    pub fn try_new(transport: MmioTransport<'static>, irq: u32) -> Option<Self> {
        let mut inner = VirtIONetRaw::<VirtioHal, _, QUEUE_SIZE>::new(transport).ok()?;
        let mac = inner.mac_address();

        let mut rx_slots: [Option<Vec<u8>>; QUEUE_SIZE] = array::from_fn(|_| None);
        for _ in 0..QUEUE_SIZE {
            let mut rx_buf = vec![0u8; RX_BUF_LEN];
            // SAFETY: `rx_buf` is stored into `rx_slots` immediately after begin,
            // so it lives until completion and repost.
            let token = unsafe { inner.receive_begin(rx_buf.as_mut_slice()) }.ok()?;
            if token as usize >= QUEUE_SIZE {
                warn!("virtio-net: rx token {} out of range", token);
                return None;
            }
            if rx_slots[token as usize].is_some() {
                warn!("virtio-net: duplicated initial rx token {}", token);
                return None;
            }
            rx_slots[token as usize] = Some(rx_buf);
        }

        Some(Self {
            irq,
            mac,
            inner: SpinNoIrqLock::new(inner),
            tx_wait_queue: WaitQueueKeyed::new(),
            rx_slots: SpinNoIrqLock::new(rx_slots),
        })
    }

    /// IRQ number used by this network device.
    #[inline]
    pub fn irq(&self) -> u32 {
        self.irq
    }

    /// MAC address reported by the device.
    #[inline]
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Acknowledge the device interrupt and wake a waiting TX token if any.
    pub fn handle_irq(&self) {
        let mut inner = self.inner.lock();
        if inner.ack_interrupt().is_empty() {
            return;
        }
        let tx_ready = inner.poll_transmit();
        drop(inner);
        if let Some(token) = tx_ready {
            self.tx_wait_queue.wake_selected(token);
        }
    }

    /// Returns whether TX queue can accept one packet.
    pub fn can_send(&self) -> bool {
        self.inner.lock().can_send()
    }

    /// Returns whether one RX completion is currently available.
    pub fn can_recv(&self) -> bool {
        self.inner.lock().poll_receive().is_some()
    }

    /// Try receive one frame into `out`.
    ///
    /// Returns copied payload length when one packet is available.
    pub fn try_recv(&self, out: &mut [u8]) -> Option<usize> {
        let mut inner = self.inner.lock();
        let token = inner.poll_receive()?;

        let mut rx_slots = self.rx_slots.lock();
        let mut rx_buf = rx_slots.get_mut(token as usize)?.take()?;
        // SAFETY: `rx_buf` is exactly the one passed to `receive_begin` for this `token`.
        let (hdr_len, pkt_len) = unsafe { inner.receive_complete(token, rx_buf.as_mut_slice()) }.ok()?;
        let copy_len = pkt_len.min(out.len());
        if hdr_len + copy_len > rx_buf.len() {
            warn!(
                "virtio-net: malformed rx frame hdr_len={} pkt_len={} buf_len={}",
                hdr_len,
                pkt_len,
                rx_buf.len()
            );
            return None;
        }
        out[..copy_len].copy_from_slice(&rx_buf[hdr_len..hdr_len + copy_len]);

        // SAFETY: The same `rx_buf` is kept in `rx_slots` after repost.
        match unsafe { inner.receive_begin(rx_buf.as_mut_slice()) } {
            Ok(new_token) if (new_token as usize) < QUEUE_SIZE => {
                if rx_slots[new_token as usize].is_some() {
                    warn!("virtio-net: repost token {} already occupied", new_token);
                    return None;
                }
                rx_slots[new_token as usize] = Some(rx_buf);
            }
            Ok(new_token) => {
                warn!("virtio-net: repost token {} out of range", new_token);
                return None;
            }
            Err(e) => {
                warn!("virtio-net: repost receive_begin failed: {:?}", e);
                return None;
            }
        }

        Some(copy_len)
    }

    /// Send one Ethernet frame and wait for TX completion by token.
    pub fn send(&self, frame: &[u8]) -> Result<(), Error> {
        const MAX_HEADER_PAD: usize = 32;
        let mut tx_buf = vec![0u8; frame.len() + MAX_HEADER_PAD];

        let (token, used_len) = {
            let mut inner = self.inner.lock();
            let hdr_len = inner.fill_buffer_header(tx_buf.as_mut_slice())?;
            let used_len = hdr_len + frame.len();
            tx_buf[hdr_len..used_len].copy_from_slice(frame);
            // SAFETY: `tx_buf[..used_len]` remains alive and untouched until completion.
            let token = unsafe { inner.transmit_begin(&tx_buf[..used_len]) }?;
            (token, used_len)
        };

        self.wait_tx_token(token);

        // SAFETY: Same token and same tx buffer as `transmit_begin` above.
        unsafe {
            self.inner
                .lock()
                .transmit_complete(token, &tx_buf[..used_len])?;
        }
        Ok(())
    }

    fn wait_tx_token(&self, token: u16) {
        loop {
            if self.tx_token_ready(token) {
                return;
            }
            if current_task().is_some() {
                self.tx_wait_queue
                    .wait_selected_with_reason_or_skip(token, WaitReason::NetDeviceTx, || {
                        self.tx_token_ready(token)
                    });
                return;
            }

            // Early boot path without schedulable task context.
            spin_loop();
        }
    }

    fn tx_token_ready(&self, token: u16) -> bool {
        let mut inner = self.inner.lock();
        matches!(inner.poll_transmit(), Some(ready) if ready == token)
    }
}
