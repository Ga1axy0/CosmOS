OUTPUT_ARCH(loongarch)
ENTRY(_start)
VIRT_BASE = 0x9000000090000000;
/*
 * QEMU's direct ELF loader starts the CPU at an untranslated physical entry.
 * The Nebula U-Boot already executes through DMW1 and its `bootelf` can only
 * access the cached alias.  Both images use the same `_start`; only their ELF
 * VMA/LMA representation differs.
 */
PHYS_BASE = DEFINED(NEBULA_BOOT) ? VIRT_BASE : 0x90000000;
KERNEL_OFFSET = VIRT_BASE - PHYS_BASE;

SECTIONS
{
    /*
     * Firmware enters with address translation disabled.  Keep the unique
     * ELF entry point at its physical load address; this tiny bootstrap only
     * installs the DMWs and transfers control to `_start_high`.
     */
    . = PHYS_BASE;
    .boot : {
        KEEP(*(.text.entry))
    }
    boot_physical_end = .;

    /* Link the rest in the cached DMW while retaining contiguous LMAs. */
    . = VIRT_BASE + (boot_physical_end - PHYS_BASE);
    skernel = VIRT_BASE;

    stext = .;
    .text : AT(ADDR(.text) - KERNEL_OFFSET) {
        KEEP(*(.text.boot.high))
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
