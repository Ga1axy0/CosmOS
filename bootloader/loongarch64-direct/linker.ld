OUTPUT_ARCH(loongarch)
ENTRY(_start)

BASE_ADDRESS = 0x81000000;

SECTIONS
{
    . = BASE_ADDRESS;

    .text : ALIGN(16) {
        *(.text.entry)
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16) {
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
        *(.bss .bss.*)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.debug*)
    }
}
