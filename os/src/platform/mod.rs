//! Platform-specific machine composition.
//!
//! `arch` describes ISA and privilege-architecture behavior.
//! `drivers` describe reusable device-IP drivers.
//! `platform` binds one concrete machine model to those two layers: MMIO
//! layout, interrupt routing, device probing, poweroff, SMP bring-up, and
//! early console policy all belong here.
#![allow(missing_docs)]

#[cfg(target_arch = "riscv64")]
pub mod riscv;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch;

#[cfg(target_arch = "riscv64")]
pub use riscv::qemu_virt::rtc;

#[cfg(target_arch = "loongarch64")]
pub use loongarch::qemu_virt::rtc;

#[cfg(target_arch = "riscv64")]
pub use riscv::qemu_virt::{
    BlockDeviceImpl, CharDeviceImpl, USER_MMAP_BASE, USER_STACK_BASE, INTERP_BASE, CLOCK_FREQ, MMIO, QEMUExit, QEMU_EXIT_HANDLE, VIRT_RTC,
    VIRT_UART, VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
    console_getchar, console_putchar, console_rx_irq_ready, direct_map_phys_to_virt,
    direct_map_virt_to_phys, early_console_write, handle_external_irq, heap_debug_enabled,
    init_external_irq, init_external_irq_hart, kernel_heap_virtual_window_supported,
    machine_name, mmio_phys_to_virt, probe_platform_devices, rtc_is_supported, shutdown,
    start_secondary_harts, translate_direct_mapped_kernel_va, use_early_console,
    KERNEL_HEAP_BASE, SbiPlatform as PlatformImpl, TRAMPOLINE,
};

#[cfg(target_arch = "loongarch64")]
pub use loongarch::qemu_virt::{
    BlockDeviceImpl, CharDeviceImpl, USER_MMAP_BASE, USER_STACK_BASE, INTERP_BASE, CLOCK_FREQ, IO_ADDR_OFFSET, KERNEL_ADDR_OFFSET, MMIO,
    QEMUExit, QEMU_EXIT_HANDLE, VIRT_RTC, VIRT_UART, VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE,
    VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE, console_getchar, console_putchar,
    console_rx_irq_ready, direct_map_phys_to_virt, direct_map_virt_to_phys,
    early_console_write, handle_external_irq, heap_debug_enabled, init_external_irq,
    init_external_irq_hart, kernel_heap_virtual_window_supported, machine_name,
    mmio_phys_to_virt, probe_platform_devices, rtc_is_supported, shutdown,
    start_secondary_harts, translate_direct_mapped_kernel_va, use_early_console,
    KERNEL_HEAP_BASE, LoongArchPlatform as PlatformImpl, TRAMPOLINE,
};

/// Initialize platform-owned devices and interrupt routing.
pub fn init() {
    rtc::init();
    init_external_irq();
    probe_platform_devices();
}
