//! LoongArch EFI system-table FDT discovery.

use core::ptr;

use crate::boot::source::{loongarch_early_addr, FdtSource};

/// Locate the FDT configuration table advertised by the LoongArch EFI ABI.
pub(crate) fn fdt_source() -> Option<FdtSource> {
    const SIGNATURE: u64 = 0x5453_5953_2049_4249;
    const NR_TABLES: usize = 104;
    const TABLES: usize = 112;
    const ENTRY_SIZE: usize = 24;
    const DT_GUID: [u8; 16] = [0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0];
    let args = crate::arch::loongarch64::firmware_boot_args();
    if args.arg2 == 0 || args.arg2 & (core::mem::align_of::<u64>() - 1) != 0 || args.arg2 >= crate::platform::KERNEL_ADDR_OFFSET { return None; }
    let table = crate::platform::direct_map_phys_to_virt(args.arg2);
    if unsafe { ptr::read_volatile(table as *const u64) } != SIGNATURE { return None; }
    let count = unsafe { ptr::read_volatile((table + NR_TABLES) as *const u64) as usize };
    if count == 0 || count > 32 { return None; }
    let entries = loongarch_early_addr(unsafe { ptr::read_volatile((table + TABLES) as *const u64) as usize })?;
    for index in 0..count {
        let entry = entries.checked_add(index.checked_mul(ENTRY_SIZE)?)?;
        let guid = unsafe { core::slice::from_raw_parts(entry as *const u8, DT_GUID.len()) };
        if guid == DT_GUID { return FdtSource::from_ptr(unsafe { ptr::read_volatile((entry + 16) as *const usize) }); }
    }
    None
}
