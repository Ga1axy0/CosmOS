//! Open Firmware / Device Tree access.
//!
//! The boot layer owns only the FDT blob and early memory discovery.  This
//! module is the single place where DT wire-format and address semantics are
//! exposed to platform code and drivers.

pub mod address;
pub mod block;
pub mod compat;
pub mod fdt;
pub mod irq;
pub mod net;
pub(crate) mod node;
pub(crate) mod scan;
pub mod pci;
pub mod resource;
pub mod registry;

pub use fdt::{FdtBlob, FdtError, FdtVisitor};
pub use resource::DeviceResource;
