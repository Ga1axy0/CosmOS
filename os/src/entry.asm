    .section .text.entry
    .globl _start
_start:
    lui gp, %hi(__global_pointer$)
    addi gp, gp, %lo(__global_pointer$)
    la sp, boot_stack_lower_bound
    addi t1, a0, 1
    slli t0, t1, 16
    add sp, sp, t0
    call rust_main

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space (4096 * 16) * 8
    .globl boot_stack_top
boot_stack_top:
