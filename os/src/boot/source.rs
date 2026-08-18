//! Firmware-provided Device Tree sources.

/// A boot-protocol payload that can seed early firmware discovery.
#[derive(Clone, Copy, Debug)]
pub enum BootSource {
    /// A flattened Device Tree blob.
    Fdt(FdtSource),
}

impl BootSource {
    /// Return the FDT carried by this source.
    pub const fn fdt(self) -> FdtSource {
        match self {
            Self::Fdt(source) => source,
        }
    }
}

/// A directly mapped FDT together with its reservation policy.
#[derive(Clone, Copy, Debug)]
pub struct FdtSource {
    /// Directly mapped virtual address of the FDT blob.
    pub ptr: usize,
    /// Whether the FDT backing memory must be reserved in memblock.
    pub reserve_physical_blob: bool,
}

impl FdtSource {
    /// Normalize a firmware physical or direct-mapped FDT address.
    pub fn from_ptr(ptr: usize) -> Option<Self> {
        if ptr == 0 { return None; }
        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        let ptr = if ptr < crate::platform::KERNEL_ADDR_OFFSET {
            crate::platform::direct_map_phys_to_virt(ptr)
        } else { ptr };
        (ptr != 0).then_some(Self { ptr, reserve_physical_blob: true })
    }
}

/// Normalize an address supplied by a LoongArch early-boot ABI.
#[cfg(target_arch = "loongarch64")]
pub(crate) fn loongarch_early_addr(address: usize) -> Option<usize> {
    (address != 0).then(|| {
        if address < crate::platform::KERNEL_ADDR_OFFSET {
            crate::platform::direct_map_phys_to_virt(address)
        } else {
            address
        }
    })
}
