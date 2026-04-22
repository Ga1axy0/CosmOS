//! Loopback network device implementation.
//!
//! Provides a software-only loopback device with a public queue for direct packet injection.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::{
    phy::{self, Device, DeviceCapabilities, Medium},
    time::Instant,
};

/// A loopback device with public queue access.
///
/// This is similar to smoltcp::phy::Loopback but exposes the queue
/// so we can directly inject packets without going through the Token API.
pub(super) struct Loopback {
    pub(super) queue: VecDeque<Vec<u8>>,
    medium: Medium,
}

impl Loopback {
    pub(super) fn new(medium: Medium) -> Self {
        Self {
            queue: VecDeque::new(),
            medium,
        }
    }
}

impl Device for Loopback {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 65535;
        caps.medium = self.medium;
        caps
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.queue.pop_front().map(|buffer| {
            let rx = RxToken { buffer };
            let tx = TxToken { queue: &mut self.queue };
            (rx, tx)
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { queue: &mut self.queue })
    }
}

pub(super) struct RxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

pub(super) struct TxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
}

impl<'a> phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0; len];
        let result = f(&mut buffer);
        self.queue.push_back(buffer);
        result
    }
}
