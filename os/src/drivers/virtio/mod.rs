//! Shared VirtIO HAL glue for CosmOS.
//!
//! This module provides the `virtio-drivers` [`Hal`] implementation used by
//! both block and network drivers.

use alloc::vec::Vec;
use core::ptr::NonNull;
use lazy_static::lazy_static;
use virtio_drivers::{BufferDirection, Hal, PhysAddr as VirtioPhysAddr};

use crate::config::PAGE_SIZE;
use crate::mm::{
    frame_alloc_contiguous, frame_dealloc_range, kernel_token, phys_to_virt, ContiguousFrames,
    PageTable, PhysAddr as KernelPhysAddr, PhysPageNum, VirtAddr,
};
use crate::sync::SpinNoIrqLock;

lazy_static! {
    /// Tracks DMA frames allocated for VirtIO queues so their lifetime extends
    /// across device usage.
    static ref QUEUE_FRAMES: SpinNoIrqLock<Vec<ContiguousFrames>> = SpinNoIrqLock::new(Vec::new());

    /// Bounce mappings for shared buffers that are not physically contiguous.
    static ref BOUNCE_MAPPINGS: SpinNoIrqLock<Vec<BounceMapping>> = SpinNoIrqLock::new(Vec::new());
}

struct BounceMapping {
    /// Paddr returned to virtio-drivers::Hal::share (may include page offset).
    shared_paddr: VirtioPhysAddr,
    /// Kernel virtual address of the bounced buffer start (same offset as original).
    bounced_vaddr: usize,
    /// Contiguous frames backing this bounced region.
    frames: ContiguousFrames,
}

#[inline]
fn translate_kernel_va(va: usize) -> usize {
    if let Some(pa) = crate::platform::translate_direct_mapped_kernel_va(va) {
        return pa;
    }

    PageTable::from_token(kernel_token())
        .translate_va(VirtAddr::from(va))
        .expect("virtio: translate_va failed")
        .0
}

fn is_physically_contiguous(ptr: usize, len: usize) -> bool {
    if len <= 1 {
        return true;
    }

    let start = ptr;
    let end = ptr + len - 1;
    let start_page = start & !(PAGE_SIZE - 1);
    let end_page = end & !(PAGE_SIZE - 1);

    let mut expected_pa_page = translate_kernel_va(start_page);
    let mut va_page = start_page;
    loop {
        let pa_page = translate_kernel_va(va_page);
        if pa_page != expected_pa_page {
            return false;
        }
        if va_page == end_page {
            break;
        }
        va_page += PAGE_SIZE;
        expected_pa_page += PAGE_SIZE;
    }

    true
}

fn alloc_contiguous_frames(pages: usize) -> ContiguousFrames {
    assert!(pages > 0);
    let pages = pages
        .checked_next_power_of_two()
        .expect("virtio contiguous allocation size overflow");
    frame_alloc_contiguous(pages, pages).unwrap_or_else(|| {
        panic!(
            "virtio bounce alloc: failed to get {} contiguous pages",
            pages
        )
    })
}

#[inline]
unsafe fn copy_to_bounce(src: usize, dst: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: caller guarantees pointers are valid and non-overlapping.
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len);
    }
}

#[inline]
unsafe fn copy_from_bounce(src: usize, dst: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: caller guarantees pointers are valid and non-overlapping.
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, len);
    }
}

/// The HAL implementation used by `virtio-drivers` in CosmOS.
pub struct VirtioHal;

#[inline]
pub(crate) fn virtio_dma_rmb() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags));
    }

    #[cfg(not(target_arch = "riscv64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (VirtioPhysAddr, NonNull<u8>) {
        assert!(pages > 0);

        let frames = alloc_contiguous_frames(pages);
        let ppn_base = frames.start_ppn();

        let pa: KernelPhysAddr = ppn_base.into();
        let paddr = pa.0 as VirtioPhysAddr;
        let vaddr =
            NonNull::new(phys_to_virt(pa.0) as *mut u8).expect("virtio dma_alloc: null vaddr");
        QUEUE_FRAMES.lock().push(frames);
        (paddr, vaddr)
    }

    unsafe fn dma_dealloc(paddr: VirtioPhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        if pages == 0 {
            return 0;
        }

        let ppn: PhysPageNum = KernelPhysAddr::from(paddr as usize).into();
        let mut frames = QUEUE_FRAMES.lock();
        if let Some(idx) = frames
            .iter()
            .position(|f| f.start_ppn() == ppn && f.pages() >= pages)
        {
            frames.swap_remove(idx);
        } else {
            let pages = pages
                .checked_next_power_of_two()
                .expect("virtio dma_dealloc size overflow");
            frame_dealloc_range(ppn, pages);
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: VirtioPhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(crate::platform::mmio_phys_to_virt(paddr as usize) as *mut u8)
            .expect("virtio mmio_phys_to_virt: null")
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> VirtioPhysAddr {
        let ptr = buffer.as_ptr() as *const u8 as usize;
        let len = buffer.len();

        if is_physically_contiguous(ptr, len) {
            return translate_kernel_va(ptr) as VirtioPhysAddr;
        }

        // Fall back to contiguous physical bounce buffer.
        let page_offset = ptr & (PAGE_SIZE - 1);
        let total = page_offset + len;
        let pages = total.div_ceil(PAGE_SIZE);
        let frames = alloc_contiguous_frames(pages);

        let base_ppn = frames.start_ppn();
        let base_pa: KernelPhysAddr = base_ppn.into();
        let base_pa_usize = base_pa.0;
        let bounced_vaddr = phys_to_virt(base_pa_usize) + page_offset;
        let shared_paddr = (base_pa_usize + page_offset) as VirtioPhysAddr;

        if matches!(
            direction,
            BufferDirection::DriverToDevice | BufferDirection::Both
        ) {
            // SAFETY: `ptr..ptr+len` and bounce range are valid and non-overlapping.
            unsafe {
                copy_to_bounce(ptr, bounced_vaddr, len);
            }
        }

        BOUNCE_MAPPINGS.lock().push(BounceMapping {
            shared_paddr,
            bounced_vaddr,
            frames,
        });

        shared_paddr
    }

    unsafe fn unshare(paddr: VirtioPhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let mapping = {
            let mut mappings = BOUNCE_MAPPINGS.lock();
            mappings
                .iter()
                .position(|m| m.shared_paddr == paddr)
                .map(|idx| mappings.swap_remove(idx))
        };

        let Some(mapping) = mapping else {
            // Direct-mapped physically contiguous buffer path.
            return;
        };

        let ptr = buffer.as_ptr() as *mut u8 as usize;
        let len = buffer.len();

        if matches!(
            direction,
            BufferDirection::DeviceToDriver | BufferDirection::Both
        ) {
            // SAFETY: `ptr..ptr+len` and bounce range are valid and non-overlapping.
            unsafe {
                copy_from_bounce(mapping.bounced_vaddr, ptr, len);
            }
        }

        drop(mapping.frames);
    }
}
