//! U-Boot standalone-application FDT discovery.

use core::{mem::size_of, ptr};
use crate::boot::source::{loongarch_early_addr, FdtSource};

/// Locate the FDT supplied as a `bootelf` argument.
pub(crate) fn fdt_source() -> Option<FdtSource> {
    let args = crate::arch::loongarch64::firmware_boot_args();
    if args.arg0 == 0 || args.arg0 > 8 || args.arg1 == 0 || args.arg1 & (core::mem::align_of::<usize>() - 1) != 0 { return None; }
    let argv = loongarch_early_addr(args.arg1)?;
    for index in 0..args.arg0 {
        let argument = loongarch_early_addr(unsafe { ptr::read_volatile((argv + index * size_of::<usize>()) as *const usize) });
        if let Some(ptr) = argument.and_then(parse_argument).and_then(FdtSource::from_ptr) { return Some(ptr); }
    }
    None
}

fn parse_argument(argument: usize) -> Option<usize> {
    let prefix = unsafe { core::slice::from_raw_parts(argument as *const u8, 4) };
    let mut cursor = argument + usize::from(prefix == b"fdt=") * 4;
    if unsafe { ptr::read_volatile(cursor as *const u8) } == b'0' && unsafe { ptr::read_volatile((cursor + 1) as *const u8) } == b'x' { cursor += 2; }
    let mut value = 0usize;
    for digits in 0..usize::BITS as usize / 4 {
        let byte = unsafe { ptr::read_volatile((cursor + digits) as *const u8) };
        if byte == 0 { return (digits != 0 && value != 0).then_some(value); }
        let digit = match byte { b'0'..=b'9' => (byte - b'0') as usize, b'a'..=b'f' => (byte - b'a' + 10) as usize, b'A'..=b'F' => (byte - b'A' + 10) as usize, _ => return None };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    None
}
