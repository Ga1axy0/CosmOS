//! QEMU LoongArch64 virt machine.

/// Direct-mapped uncached I/O virtual-address offset used during early bring-up.
pub const IO_ADDR_OFFSET: usize = 0x8000_0000_0000_0000;
/// Direct-mapped cached kernel-address offset used during early bring-up.
pub const KERNEL_ADDR_OFFSET: usize = 0x9000_0000_0000_0000;

/// QEMU loongarch64 virt clock frequency.
pub const CLOCK_FREQ: usize = 100_000_000;

/// MMIO windows used by the kernel on QEMU loongarch64 virt (uncached DMW0 window).
pub const MMIO: &[(usize, usize)] = &[
    (IO_ADDR_OFFSET | 0x1fe0_0000, 0x10000), // covers all 1fe0_xxxx MMIO
    (IO_ADDR_OFFSET | 0x1fe2_0000, 0x8000),  // VirtIO
];

/// UART MMIO virtual address (uncached DMW0 window).
pub const VIRT_UART: usize = IO_ADDR_OFFSET | 0x1fe0_01e0;
/// RTC-compatible MMIO virtual address (uncached DMW0 window).
pub const VIRT_RTC: usize = IO_ADDR_OFFSET | 0x1fe0_01f8;
/// VirtIO MMIO window base address.
pub const VIRTIO_MMIO_BASE: usize = IO_ADDR_OFFSET | 0x1fe2_0000;
/// Size of each VirtIO MMIO slot.
pub const VIRTIO_MMIO_STRIDE: usize = 0x1000;
/// Number of VirtIO MMIO slots exposed by the board.
pub const VIRTIO_MMIO_SLOTS: usize = 8;
/// First IRQ line assigned to VirtIO MMIO devices.
pub const VIRTIO_MMIO_IRQ_BASE: u32 = 1;
/// Alias for compatibility.
pub const VIRT_UART_EARLY: usize = VIRT_UART;

/// Block device implementation for QEMU virt.
pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
/// Char device implementation for QEMU virt.
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a<VIRT_UART>;

use core::arch::asm;

const EXIT_SUCCESS: u32 = 0x5555;
const EXIT_FAILURE_FLAG: u32 = 0x3333;
const EXIT_FAILURE: u32 = exit_code_encode(1);

/// QEMU exit interface.
pub trait QEMUExit {
    /// Exit with specified return code.
    fn exit(&self, code: u32) -> !;

    /// Exit QEMU using `EXIT_SUCCESS`.
    fn exit_success(&self) -> !;

    /// Exit QEMU using `EXIT_FAILURE`.
    fn exit_failure(&self) -> !;
}

/// LoongArch64 QEMU exit device wrapper.
pub struct LOONGARCH64 {
    addr: u64,
}

const fn exit_code_encode(code: u32) -> u32 {
    (code << 16) | EXIT_FAILURE_FLAG
}

impl LOONGARCH64 {
    /// Create an instance.
    pub const fn new(addr: u64) -> Self {
        Self { addr }
    }
}

impl QEMUExit for LOONGARCH64 {
    fn exit(&self, code: u32) -> ! {
        let code_new = match code {
            EXIT_SUCCESS | EXIT_FAILURE => code,
            _ => exit_code_encode(code),
        };

        unsafe {
            asm!(
                "st.w {0}, {1}, 0",
                in(reg) code_new,
                in(reg) self.addr,
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

const VIRT_TEST: u64 = (IO_ADDR_OFFSET | 0x1fe0_01e0) as u64;

/// Global QEMU exit handle using the virt test device.
pub const QEMU_EXIT_HANDLE: LOONGARCH64 = LOONGARCH64::new(VIRT_TEST);
