//! Trait for a chardev.

use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::board::CharDeviceImpl;

mod ns16550a;

pub use ns16550a::NS16550a;

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