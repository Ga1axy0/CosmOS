    .section .text.entry
    .equ BOOT_STACK_SHIFT, 18
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 8

    .globl _start
_start:
    lui gp, %hi(__global_pointer$)
    addi gp, gp, %lo(__global_pointer$)
    la sp, boot_stack_lower_bound
    addi t1, a0, 1
    slli t0, t1, BOOT_STACK_SHIFT
    add sp, sp, t0
    call rust_main

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space BOOT_STACK_SIZE * BOOT_STACK_HARTS
    .globl boot_stack_top
boot_stack_top:
