#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{
    mmap_full, mremap, munmap, MMapFlags, MMapProt, MREMAP_DONTUNMAP, MREMAP_FIXED,
    MREMAP_MAYMOVE,
};

const PAGE_SIZE: usize = 4096;

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let flags = MMapFlags::MAP_PRIVATE | MMapFlags::MAP_ANONYMOUS;
    let prot = MMapProt::PROT_READ | MMapProt::PROT_WRITE;

    let source = mmap_full(0, PAGE_SIZE, prot, flags, 0, 0);
    assert!(source > 0, "initial mmap failed: {}", source);
    let source = source as usize;
    unsafe {
        (source as *mut u8).write_volatile(0x5a);
    }

    let grown = mremap(source, PAGE_SIZE, PAGE_SIZE * 2, MREMAP_MAYMOVE, 0);
    assert!(grown > 0, "MREMAP_MAYMOVE failed: {}", grown);
    let grown = grown as usize;
    unsafe {
        assert_eq!((grown as *const u8).read_volatile(), 0x5a);
        (grown as *mut u8).add(PAGE_SIZE).write_volatile(0xa5);
    }

    let target = mmap_full(0, PAGE_SIZE * 2, prot, flags, 0, 0);
    assert!(target > 0, "fixed target mmap failed: {}", target);
    let target = target as usize;
    let fixed = mremap(
        grown,
        PAGE_SIZE * 2,
        PAGE_SIZE * 2,
        MREMAP_MAYMOVE | MREMAP_FIXED,
        target,
    );
    assert_eq!(fixed, target as isize, "MREMAP_FIXED failed: {}", fixed);
    unsafe {
        assert_eq!((target as *const u8).read_volatile(), 0x5a);
        assert_eq!((target as *const u8).add(PAGE_SIZE).read_volatile(), 0xa5);
    }

    assert_eq!(
        mremap(target, PAGE_SIZE * 2, PAGE_SIZE * 2, MREMAP_DONTUNMAP, 0),
        -95,
        "MREMAP_DONTUNMAP should be unsupported"
    );
    assert_eq!(munmap(target, PAGE_SIZE * 2), 0);
    0
}
