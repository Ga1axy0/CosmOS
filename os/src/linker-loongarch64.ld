OUTPUT_ARCH(loongarch)
ENTRY(_start)
VIRT_BASE = 0x9000000090000000;
PHYS_BASE = 0x90000000;

SECTIONS
{
    . = VIRT_BASE;
    skernel = .;

    stext = .;
    .text : AT(PHYS_BASE) {
        *(.text.entry)
        . = ALIGN(4K);
        strampoline = .;
        *(.text.trampoline);
        . = ALIGN(4K);
        *(.text .text.*)
    }

    . = ALIGN(4K);
    etext = .;
    srodata = .;
    .rodata : AT(PHYS_BASE + (srodata - VIRT_BASE)) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(4K);
    erodata = .;
    sdata = .;
    .data : AT(PHYS_BASE + (sdata - VIRT_BASE)) {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    . = ALIGN(4K);
    edata = .;
    sbss_with_stack = .;
    .bss : AT(PHYS_BASE + (sbss_with_stack - VIRT_BASE)) {
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
