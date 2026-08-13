//! Static board description for the RISC-V QEMU `virt` machine.

use super::KERNEL_MMIO_OFFSET;

/// default base address for anonymous mmap allocations
pub const USER_MMAP_BASE: usize = 0x1_0000_0000;

/// default base address for the main thread's user stack region
pub const USER_STACK_BASE: usize = 0x0800_0000;

/// base address for loading dynamic linker (interpreter)
pub const INTERP_BASE: usize = 0x2_0000_0000;

/// Block device implementation for QEMU `virt`.
pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
/// Char device implementation for QEMU `virt`.
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a;

use core::arch::asm;

const EXIT_SUCCESS: u32 = 0x5555;
const EXIT_FAILURE_FLAG: u32 = 0x3333;
const EXIT_FAILURE: u32 = exit_code_encode(1);
const EXIT_RESET: u32 = 0x7777;

/// QEMU exit interface.
pub trait QEMUExit {
    /// Exit with the specified return code.
    fn exit(&self, code: u32) -> !;

    /// Exit QEMU using `EXIT_SUCCESS`, aka `0`, if possible.
    fn exit_success(&self) -> !;

    /// Exit QEMU using `EXIT_FAILURE`, aka `1`.
    fn exit_failure(&self) -> !;
}

/// RISC-V QEMU exit wrapper.
pub struct RISCV64 {
    /// Address of the sifive_test mapped device.
    addr: u64,
}

/// Encode the exit code using `EXIT_FAILURE_FLAG`.
const fn exit_code_encode(code: u32) -> u32 {
    (code << 16) | EXIT_FAILURE_FLAG
}

impl RISCV64 {
    /// Create an instance.
    pub const fn new(addr: u64) -> Self {
        RISCV64 { addr }
    }
}

impl QEMUExit for RISCV64 {
    fn exit(&self, code: u32) -> ! {
        let code_new = match code {
            EXIT_SUCCESS | EXIT_FAILURE | EXIT_RESET => code,
            _ => exit_code_encode(code),
        };

        unsafe {
            asm!(
                "sw {0}, 0({1})",
                in(reg) code_new,
                in(reg) self.addr
            );

            loop {
                asm!("wfi", options(nomem, nostack));
            }
        }
    }

    fn exit_success(&self) -> ! {
        self.exit(EXIT_SUCCESS);
    }

    fn exit_failure(&self) -> ! {
        self.exit(EXIT_FAILURE);
    }
}

const VIRT_TEST: u64 = (KERNEL_MMIO_OFFSET + 0x100000) as u64;

/// Global QEMU exit handle using the sifive_test device.
pub const QEMU_EXIT_HANDLE: RISCV64 = RISCV64::new(VIRT_TEST);
