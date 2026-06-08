    .section .text.entry
    .equ BOOT_STACK_SHIFT, 20
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 8
    .equ UART_BASE_EARLY, 0x800000001fe001e0
    .equ UART_THR, 0x0
    .equ UART_LSR, 0x5
    .equ UART_LSR_THRE, 0x20

    .macro uart_putc reg
1:
    ld.bu     $t4, $t3, UART_LSR
    andi      $t4, $t4, UART_LSR_THRE
    beqz      $t4, 1b
    st.b      \reg, $t3, UART_THR
    .endm

    .globl _start
_start:
    ori       $t0, $zero, 0x1
    lu52i.d   $t0, $t0, -2048
    csrwr     $t0, 0x180
    ori       $t0, $zero, 0x11
    lu52i.d   $t0, $t0, -1792
    csrwr     $t0, 0x181

    csrrd     $a0, 0x20
    bnez      $a0, 1f

    li.w      $t0, 0xb0
    csrwr     $t0, 0x0
    li.w      $t0, 0x00
    csrwr     $t0, 0x1
    csrwr     $t0, 0x2

    li.d      $t3, UART_BASE_EARLY
    li.w      $t5, 'B'
    uart_putc $t5

    la.global $sp, boot_stack_lower_bound
    li.d      $t2, BOOT_STACK_SIZE
    add.d     $sp, $sp, $t2
    bl        rust_main

1:
    la.global $sp, boot_stack_lower_bound
    addi.d    $t1, $a0, 1
    slli.d    $t0, $t1, BOOT_STACK_SHIFT
    add.d     $sp, $sp, $t0
    bl        rust_main

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space BOOT_STACK_SIZE * BOOT_STACK_HARTS
    .globl boot_stack_top
boot_stack_top:
