    .section .text.entry
    .equ BOOT_STACK_SHIFT, 20
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 8

    .globl _start
_start:
    ori       $t0, $zero, 0x1
    lu52i.d   $t0, $t0, -2048
    csrwr     $t0, 0x180
    ori       $t0, $zero, 0x11
    lu52i.d   $t0, $t0, -1792
    csrwr     $t0, 0x181

    li.w      $t0, 0xb0
    csrwr     $t0, 0x0
    li.w      $t0, 0x00
    csrwr     $t0, 0x1
    csrwr     $t0, 0x2

    csrrd     $a0, 0x20
    # Do not branch to a numeric local label here: `uart_putc` expands its
    # own `1:` label, and secondaries would jump into the middle of the
    # bootstrap-hart UART polling sequence with an uninitialized `$t3`.
    bnez      $a0, .Lsecondary_start

    la.global $sp, boot_stack_lower_bound
    li.d      $t2, BOOT_STACK_SIZE
    add.d     $sp, $sp, $t2
    bl        rust_main

.Lsecondary_start:
    la.global $sp, boot_stack_lower_bound
    addi.d    $t1, $a0, 1
    slli.d    $t0, $t1, BOOT_STACK_SHIFT
    add.d     $sp, $sp, $t0
    bl        rust_main

    # Keep early boot stacks out of `.bss`: secondary harts may start running
    # while the bootstrap hart is still in the one-time memory-init path, so
    # using a preloaded data section lets us rule out any interaction with
    # zeroing or other `.bss`-specific handling during bring-up.
    .section .data.bootstack
    .balign 16
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space BOOT_STACK_SIZE * BOOT_STACK_HARTS
    .globl boot_stack_top
boot_stack_top:
