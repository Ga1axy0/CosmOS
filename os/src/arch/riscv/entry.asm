    .section .text.entry
    .equ BOOT_STACK_SHIFT, 20
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 12
    .equ KERNEL_OFFSET, 0xffffffc000000000
    .equ EARLY_RAM_GIGAPAGE_PTE, 0x100000ef
    .equ EARLY_MMIO_GIGAPAGE_PTE, 0x000000ef
    .equ NEXT_RAM_GIGAPAGE_PTE, 0x10000000
    .equ EARLY_HIGH_RAM_ROOT_COUNT, 127

    .globl _start
_start:
    /*
     * The kernel is loaded at 0x8020_0000 and initially executes without
     * paging. Root entries 1 and 2 provide temporary identity mappings from
     * PA 0x4000_0000 through 0xbfff_ffff. This covers both VisionFive 2 RAM
     * (which begins at 0x4000_0000) and the QEMU kernel load address. Entries
     * 257..383 cover the matching direct-RAM aperture, keeping a
     * firmware-provided FDT reachable before the permanent page table exists.
     */
    la t3, boot_page_table
    /* Temporarily expose PA 0..1 GiB at both its identity and MMIO aliases. */
    li t0, EARLY_MMIO_GIGAPAGE_PTE
    sd t0, 0*8(t3)
    li t1, 384
    slli t1, t1, 3
    add t1, t3, t1
    sd t0, 0(t1)
    li t0, EARLY_RAM_GIGAPAGE_PTE
    sd t0, 1*8(t3)
    li t4, NEXT_RAM_GIGAPAGE_PTE
    add t5, t0, t4
    sd t5, 2*8(t3)
    li t1, 257
    slli t1, t1, 3
    add t1, t3, t1
    li t2, EARLY_HIGH_RAM_ROOT_COUNT
.Lmap_early_high_ram:
    sd t0, 0(t1)
    add t0, t0, t4
    addi t1, t1, 8
    addi t2, t2, -1
    bnez t2, .Lmap_early_high_ram

    srli t0, t3, 12
    li t1, 8
    slli t1, t1, 60
    or t0, t0, t1
    csrw satp, t0
    sfence.vma

    /* The early stack is addressed through its physical load alias first. */
    la t0, .Lboot_stack_pa
    ld sp, 0(t0)
    addi t1, a0, 1
    slli t0, t1, BOOT_STACK_SHIFT
    add sp, sp, t0

    la t0, .Lstart_high_va
    ld t0, 0(t0)
    jr t0

    .align 3
.Lboot_stack_pa:
    .dword boot_stack_lower_bound - KERNEL_OFFSET
.Lstart_high_va:
    .dword _start_high

    .section .data.boot
    .balign 4096
boot_page_table:
    .space 4096

    .section .text.boot.high
    .globl _start_high
_start_high:
    /* Switch the early physical stack pointer to its permanent direct alias. */
    li t0, KERNEL_OFFSET
    add sp, sp, t0
    .option push
    .option norelax
    la gp, sdata
    .option pop
    li t0, 0x800
    add gp, gp, t0
    call rust_main

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space BOOT_STACK_SIZE * BOOT_STACK_HARTS
    .globl boot_stack_top
boot_stack_top:
