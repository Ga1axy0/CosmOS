//! Address-space identifier allocation.
//!
//! The first implementation deliberately does not recycle RISC-V ASIDs.  This
//! makes removing the trap-path TLB flushes independent of address-space
//! teardown ordering: an ASID cannot refer to a different root page table
//! during one boot.  If the hardware namespace is exhausted, new address
//! spaces fall back to ASID 0 and the trampoline retains its compatibility
//! flush for those tokens.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ASID_MASK: AtomicUsize = AtomicUsize::new(0);
static NEXT_USER_ASID: AtomicUsize = AtomicUsize::new(1);
static FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// Reserve ASID 0 for the kernel page table and compatibility fallback.
pub const KERNEL_ASID: usize = 0;

/// Probe and publish the usable RISC-V ASID namespace.
pub fn init() {
    let mask = unsafe { crate::hal::probe_address_space_id_mask() };
    ASID_MASK.store(mask, Ordering::Release);
    if mask == 0 {
        warn!("[tlb] hardware ASIDs unavailable; retaining trap-path compatibility flushes");
    } else {
        info!(
            "[tlb] ASID allocator initialized: mask={:#x}, user_asids={}",
            mask, mask
        );
    }
}

/// Allocate one boot-unique user ASID.
///
/// Returning zero selects the conservative compatibility mode.  ASIDs are not
/// recycled in this first version; safe reuse will require a completed
/// ASID-wide shootdown on every hart that has cached the old generation.
pub fn allocate_user_asid() -> usize {
    let mask = ASID_MASK.load(Ordering::Acquire);
    let allocated = NEXT_USER_ASID.fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
        (next != 0 && next <= mask).then_some(next + 1)
    });
    match allocated {
        Ok(asid) => asid,
        Err(_) => {
            if !FALLBACK_WARNED.swap(true, Ordering::AcqRel) {
                warn!("[tlb] user ASID namespace exhausted; new address spaces use ASID 0");
            }
            KERNEL_ASID
        }
    }
}
