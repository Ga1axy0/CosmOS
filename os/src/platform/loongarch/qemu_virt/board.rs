//! Static board description for the LoongArch64 QEMU `virt` machine.

/// default base address for anonymous mmap allocations
pub const USER_MMAP_BASE: usize = 0x20_0000_0000;

/// default base address for the main thread's user stack region
pub const USER_STACK_BASE: usize = 0x3e_0000_0000;

/// base address for loading dynamic linker (interpreter)
pub const INTERP_BASE: usize = 0x1e_0000_0000;

/// Direct-mapped uncached I/O virtual-address offset used during early bring-up.
pub const IO_ADDR_OFFSET: usize = 0x8000_0000_0000_0000;
/// Direct-mapped cached kernel-address offset used during early bring-up.
pub const KERNEL_ADDR_OFFSET: usize = 0x9000_0000_0000_0000;

/// UART used only before the firmware FDT has selected the runtime console.
pub const VIRT_UART: usize = IO_ADDR_OFFSET | 0x1fe0_01e0;

/// Block device implementation for QEMU `virt`.
pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
/// Char device implementation for QEMU `virt`.
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a;

use core::arch::asm;

const EXIT_SUCCESS: u32 = 0x5555;
const EXIT_FAILURE: u32 = 0x0001_3333;

/// QEMU exit interface.
pub trait QEMUExit {
    /// Exit with specified return code.
    fn exit(&self, code: u32) -> !;

    /// Exit QEMU using `EXIT_SUCCESS`.
    fn exit_success(&self) -> !;

    /// Exit QEMU using `EXIT_FAILURE`.
    fn exit_failure(&self) -> !;
}

/// LoongArch64 QEMU power-management wrapper.
pub struct LOONGARCH64 {
    sleep_ctl_addr: u64,
}

// QEMU `virt` exposes ACPI GED power-management registers at:
//   VIRT_GED_EVT_ADDR = 0x100e0000
//   VIRT_GED_REG_ADDR = VIRT_GED_EVT_ADDR + ACPI_GED_EVT_SEL_LEN(0x4)
//                     + MEMORY_HOTPLUG_IO_LEN(24)
//                     = 0x100e001c
// `ACPI_GED_REG_SLEEP_CTL` is offset 0 and powers off the VM when written
// with SLP_EN | (S5 << SLP_TYP_POS) = 0x34.
const GED_SLEEP_CTL_VALUE: u8 = 0x34;
const GED_REG_BASE: u64 = (IO_ADDR_OFFSET | 0x100e_001c) as u64;

impl LOONGARCH64 {
    /// Create an instance.
    pub const fn new(addr: u64) -> Self {
        Self {
            sleep_ctl_addr: addr,
        }
    }
}

impl QEMUExit for LOONGARCH64 {
    fn exit(&self, _code: u32) -> ! {
        unsafe {
            asm!(
                "st.b {value}, {addr}, 0",
                value = in(reg) GED_SLEEP_CTL_VALUE,
                addr = in(reg) self.sleep_ctl_addr,
            );
            loop {
                asm!("idle 0", options(nomem, nostack));
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

const VIRT_TEST: u64 = GED_REG_BASE;

/// Global QEMU exit handle using the ACPI GED poweroff register.
pub const QEMU_EXIT_HANDLE: LOONGARCH64 = LOONGARCH64::new(VIRT_TEST);
