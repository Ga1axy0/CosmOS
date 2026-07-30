    .section .text.entry
    .equ BOOT_STACK_SHIFT, 20
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 8
    .equ KERNEL_OFFSET, 0xffffffc000000000
    .equ EARLY_RAM_GIGAPAGE_PTE, 0x200000ef
    .equ NEXT_RAM_GIGAPAGE_PTE, 0x10000000
    .equ EARLY_HIGH_RAM_ROOT_COUNT, 126

    .globl _start
_start:
    /*
     * The kernel is loaded at 0x8020_0000 and initially executes without
     * paging.  Root entry 2 provides the temporary identity map for the first
     * RAM gigapage. Entries 258..383 cover the direct-RAM aperture beginning
     * at 0xffff_ffc0_8000_0000, up to (but not including) the dedicated MMIO
     * root. This keeps the firmware-provided FDT reachable for every supported
     * QEMU memory size before the permanent page table is constructed.
     */
    la t3, boot_page_table
    li t0, EARLY_RAM_GIGAPAGE_PTE
    sd t0, 2*8(t3)
    li t1, 258
    slli t1, t1, 3
    add t1, t3, t1
    li t2, EARLY_HIGH_RAM_ROOT_COUNT
    li t4, NEXT_RAM_GIGAPAGE_PTE
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
