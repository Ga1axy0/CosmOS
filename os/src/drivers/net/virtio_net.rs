//! VirtIO network driver wrapper used by the kernel network stack.

use alloc::vec;
use alloc::vec::Vec;
use core::array;
use core::hint::spin_loop;

use virtio_drivers::{device::net::VirtIONetRaw, transport::SomeTransport, Error};

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
    inner: SpinNoIrqLock<VirtIONetRaw<VirtioHal, SomeTransport<'static>, QUEUE_SIZE>>,
    tx_wait_queue: WaitQueueKeyed<u16>,
    /// TX buffers that are in-flight via non-blocking `try_send`.
    tx_slots: SpinNoIrqLock<[Option<Vec<u8>>; QUEUE_SIZE]>,
    /// Completion marks for blocking `send` path tokens.
    tx_done: SpinNoIrqLock<[bool; QUEUE_SIZE]>,
    rx_slots: SpinNoIrqLock<[Option<Vec<u8>>; QUEUE_SIZE]>,
}

impl VirtIONetDevice {
    /// Try to create one device instance from an already initialized transport.
    pub fn try_new(transport: SomeTransport<'static>, irq: u32) -> Option<Self> {
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
            tx_slots: SpinNoIrqLock::new(array::from_fn(|_| None)),
            tx_done: SpinNoIrqLock::new(array::from_fn(|_| false)),
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
        drop(inner);
        self.reclaim_tx_completions();
    }

    /// Returns whether TX queue can accept one packet.
    pub fn can_send(&self) -> bool {
        self.reclaim_tx_completions();
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
        let (hdr_len, pkt_len) =
            unsafe { inner.receive_complete(token, rx_buf.as_mut_slice()) }.ok()?;
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

        {
            let mut tx_done = self.tx_done.lock();
            tx_done[token as usize] = false;
        }

        self.wait_tx_token(token);

        // SAFETY: Same token and same tx buffer as `transmit_begin` above.
        unsafe {
            self.inner
                .lock()
                .transmit_complete(token, &tx_buf[..used_len])?;
        }
        Ok(())
    }

    /// Try to send one Ethernet frame without blocking.
    ///
    /// Returns:
    /// - `Ok(true)`: queued successfully.
    /// - `Ok(false)`: TX queue currently full.
    /// - `Err(_)`: fatal driver/API failure.
    pub fn try_send(&self, frame: &[u8]) -> Result<bool, Error> {
        const MAX_HEADER_PAD: usize = 32;

        self.reclaim_tx_completions();

        let mut tx_buf = vec![0u8; frame.len() + MAX_HEADER_PAD];
        {
            let mut inner = self.inner.lock();
            if !inner.can_send() {
                return Ok(false);
            }
            let hdr_len = inner.fill_buffer_header(tx_buf.as_mut_slice())?;
            let used_len = hdr_len + frame.len();
            tx_buf[hdr_len..used_len].copy_from_slice(frame);
            tx_buf.truncate(used_len);
            // SAFETY: `tx_buf` is moved into `tx_slots` immediately after begin,
            // so it lives until completion reclaim.
            let token = unsafe { inner.transmit_begin(tx_buf.as_slice()) }?;

            let idx = token as usize;
            assert!(
                idx < QUEUE_SIZE,
                "virtio-net: tx token {} out of range",
                token
            );
            let mut tx_slots = self.tx_slots.lock();
            assert!(
                tx_slots[idx].is_none(),
                "virtio-net: duplicated non-blocking tx token {}",
                token
            );
            tx_slots[idx] = Some(tx_buf);
        }
        Ok(true)
    }

    fn reclaim_tx_completions(&self) {
        loop {
            let blocking_token = {
                let mut inner = self.inner.lock();
                let mut tx_slots = self.tx_slots.lock();
                let Some(token) = inner.poll_transmit() else {
                    return;
                };
                let idx = token as usize;
                if idx >= QUEUE_SIZE {
                    warn!("virtio-net: completion token {} out of range", token);
                    return;
                }

                if let Some(tx_buf) = tx_slots[idx].take() {
                    // SAFETY: buffer exactly matches begin-side storage for this token.
                    if let Err(e) = unsafe { inner.transmit_complete(token, tx_buf.as_slice()) } {
                        warn!(
                            "virtio-net: non-blocking transmit_complete failed token {}: {:?}",
                            token, e
                        );
                    }
                    None
                } else {
                    // Blocking `send` path: waiter owns the original tx buffer and will
                    // call `transmit_complete(token, ...)` after wakeup.
                    Some(token)
                }
            };

            let Some(token) = blocking_token else {
                // Drained one non-blocking completion; keep draining until queue is empty
                // or we hit a blocking token at the head.
                continue;
            };

            let idx = token as usize;
            let should_wake = {
                let mut tx_done = self.tx_done.lock();
                if tx_done[idx] {
                    false
                } else {
                    tx_done[idx] = true;
                    true
                }
            };
            if should_wake {
                self.tx_wait_queue.wake_selected(token);
            }

            // `poll_transmit` is peek-only. Until the blocking waiter consumes this
            // token via `transmit_complete`, it will remain at the head.
            return;
        }
    }

    fn wait_tx_token(&self, token: u16) {
        loop {
            if self.tx_token_ready(token) {
                return;
            }
            if current_task().is_some() {
                self.tx_wait_queue.wait_selected_with_reason_or_skip(
                    token,
                    WaitReason::NetDeviceTx,
                    || self.tx_token_ready(token),
                );
                return;
            }

            // Early boot path without schedulable task context.
            spin_loop();
        }
    }

    fn tx_token_ready(&self, token: u16) -> bool {
        self.reclaim_tx_completions();
        let idx = token as usize;
        if idx >= QUEUE_SIZE {
            return true;
        }
        let mut tx_done = self.tx_done.lock();
        let ready = tx_done[idx];
        if ready {
            tx_done[idx] = false;
        }
        ready
    }
}
