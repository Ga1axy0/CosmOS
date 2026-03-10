//! Trait for a chardev.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;

use crate::board::CharDeviceImpl;

mod ns16550a;

pub use ns16550a::NS16550a;

static UART_READY: AtomicBool = AtomicBool::new(false);

/// Character device abstraction used by the kernel.
///
/// For now we keep it minimal: byte-oriented read/write and an optional IRQ hook.
pub trait CharDevice: Sync + Send {
   /// Write a ch to device.
   fn write(&self, ch: u8);
   /// Read a ch to device.
   fn read(&self) -> u8;
   /// Calls when interrupt comes.
   fn handle_irq(&self) {
      // default: no IRQ support
   }
}

lazy_static! {
   /// Singleton of UART impl.
   pub static ref UART: Arc<CharDeviceImpl> = Arc::new(CharDeviceImpl::new());
}

/// Explicitly initializes the global UART device during early boot.
pub fn init() {
   lazy_static::initialize(&UART);
}

/// Returns whether the UART has finished initialization.
pub fn uart_ready() -> bool {
   UART_READY.load(Ordering::Acquire)
}

/// Marks the UART as initialized and ready for normal logging/output.
pub fn set_uart_ready() {
   UART_READY.store(true, Ordering::Release);
}
