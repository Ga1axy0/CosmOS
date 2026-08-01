OUTPUT_ARCH(riscv)
ENTRY(_start)

PHYS_BASE = 0x80200000;
KERNEL_OFFSET = 0xffffffc000000000;
VIRT_BASE = KERNEL_OFFSET + PHYS_BASE;

SECTIONS
{
    /*
     * Firmware enters with paging disabled, so the tiny bootstrap must retain
     * a physical VMA.  It installs a temporary identity/high-half pair of
     * gigapage mappings and jumps to `_start_high`.
     */
    . = PHYS_BASE;
    .boot : {
        *(.text.entry)
        . = ALIGN(4K);
        *(.data.boot)
    }
    boot_physical_end = .;

    /*
     * Everything after .boot is linked in the Sv39 high half while retaining
     * a contiguous physical load image beginning immediately after .boot.
     */
    . = VIRT_BASE + (boot_physical_end - PHYS_BASE);
    skernel = VIRT_BASE;

    stext = .;
    .text : AT(ADDR(.text) - KERNEL_OFFSET) {
        *(.text.boot.high)
        . = ALIGN(4K);
        strampoline = .;
        *(.text.trampoline);
        . = ALIGN(4K);
        *(.text .text.*)
    }

    . = ALIGN(4K);
    etext = .;
    srodata = .;
    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(4K);
    erodata = .;
    sdata = .;
    .data : AT(ADDR(.data) - KERNEL_OFFSET) {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }
    . = ALIGN(4K);
    edata = .;
    sbss_with_stack = .;
    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) {
        *(.bss.stack)
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
    }

    . = ALIGN(4K);
    ebss = .;
    ekernel = .;

    /DISCARD/ : {
        *(.eh_frame)
    }
}
