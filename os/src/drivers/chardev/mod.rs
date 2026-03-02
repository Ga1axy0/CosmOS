use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::board::CharDeviceImpl;

mod ns16550a;

pub use ns16550a::NS16550a;

/// Character device abstraction used by the kernel.
///
/// For now we keep it minimal: byte-oriented read/write and an optional IRQ hook.
pub trait CharDevice: Sync + Send {
   fn write(&self, ch: u8);
   fn read(&self) -> u8;
   fn handle_irq(&self) {
      // default: no IRQ support
   }
}

lazy_static! {
   pub static ref UART: Arc<CharDeviceImpl> = Arc::new(CharDeviceImpl::new());
}   