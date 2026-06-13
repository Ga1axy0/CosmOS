//! Static board description for the RISC-V QEMU `virt` machine.

/// default base address for anonymous mmap allocations
pub const USER_MMAP_BASE: usize = 0x1000_0000;

/// default base address for the main thread's user stack region
pub const USER_STACK_BASE: usize = 0x0800_0000;

/// base address for loading dynamic linker (interpreter)
pub const INTERP_BASE: usize = 0x4000_0000;

/// Clock frequency.
pub const CLOCK_FREQ: usize = 12_500_000;

/// MMIO windows exposed by the machine.
pub const MMIO: &[(usize, usize)] = &[
    (0x0C00_0000, 0x400000), // PLIC
    (0x0010_0000, 0x00_2000), // VIRT_TEST/RTC
    (0x1000_0000, 0x100),    // UART0 (NS16550a)
    (0x1000_1000, 0x8000),   // VirtIO MMIO devices, 8 slots, each slot occupies 0x1000 bytes
];

/// UART0 MMIO base address.
pub const VIRT_UART: usize = 0x1000_0000;
/// Goldfish RTC MMIO base address.
pub const VIRT_RTC: usize = 0x0010_1000;
/// VirtIO MMIO window base address.
pub const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
/// Size of each VirtIO MMIO slot.
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000;
/// Number of VirtIO MMIO slots exposed by the machine.
pub const VIRTIO_MMIO_SLOTS: usize = 8;
/// First IRQ line assigned to VirtIO MMIO devices.
pub const VIRTIO_MMIO_IRQ_BASE: u32 = 1;

/// Block device implementation for QEMU `virt`.
pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
/// Char device implementation for QEMU `virt`.
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a<VIRT_UART>;

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

const VIRT_TEST: u64 = 0x100000;

/// Global QEMU exit handle using the sifive_test device.
pub const QEMU_EXIT_HANDLE: RISCV64 = RISCV64::new(VIRT_TEST);
