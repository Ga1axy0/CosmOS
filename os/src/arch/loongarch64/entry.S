    .section .text.entry
    .equ BOOT_STACK_SHIFT, 20
    .equ BOOT_STACK_SIZE, 1 << BOOT_STACK_SHIFT
    .equ BOOT_STACK_HARTS, 12

    .globl _start
_start:
    ori       $t0, $zero, 0x1
    lu52i.d   $t0, $t0, -2048
    csrwr     $t0, 0x180
    ori       $t0, $zero, 0x11
    lu52i.d   $t0, $t0, -1792
    csrwr     $t0, 0x181

    # The ELF has one physical entry.  After installing DMW1, continue in the
    # linked cached section used by the rest of the kernel.
    la.abs    $t0, _start_high
    jirl      $zero, $t0, 0

    .section .text.boot.high
    .globl _start_high
_start_high:

    li.w      $t0, 0xb0
    csrwr     $t0, 0x0
    li.w      $t0, 0x00
    csrwr     $t0, 0x1
    csrwr     $t0, 0x2

    # Rust's built-in `loongarch64-unknown-none` target enables `ual` by
    # default, but CosmOS builds all LoongArch code (including core/alloc) with
    # it disabled so one image remains valid on CPUs with or without UAL.
    # Record hardware and firmware state before entering compiler-generated
    # Rust code.  Do not write MISC.ALCL0 here: the portable code-generation
    # baseline does not need UAL, and QEMU correctly treats its unimplemented
    # MISC write as an illegal instruction.
    csrrd     $t4, 0x20
    bnez      $t4, .Lskip_firmware_args
    la.global $t3, loongarch_boot_arg0
    st.d      $a0, $t3, 0
    la.global $t3, loongarch_boot_arg1
    st.d      $a1, $t3, 0
    la.global $t3, loongarch_boot_arg2
    st.d      $a2, $t3, 0
    la.global $t3, loongarch_boot_arg3
    st.d      $a3, $t3, 0
.Lskip_firmware_args:
    ori       $t1, $zero, 1
    cpucfg    $t2, $t1
    csrrd     $t0, 0x3
    bnez      $t4, .Lskip_boot_state_before
    la.global $t3, loongarch_boot_cpucfg1
    st.d      $t2, $t3, 0
    la.global $t3, loongarch_boot_misc_before
    st.d      $t0, $t3, 0
.Lskip_boot_state_before:
    bnez      $t4, .Lskip_boot_state_after
    la.global $t3, loongarch_boot_misc_after
    st.d      $t0, $t3, 0
.Lskip_boot_state_after:

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

    # Preserve the firmware entry ABI in initialized data: `.bss` is cleared
    # later, before boot information is discovered.
    .section .data.bootargs
    .balign 8
    .globl loongarch_boot_arg0
loongarch_boot_arg0:
    .dword 0
    .globl loongarch_boot_arg1
loongarch_boot_arg1:
    .dword 0
    .globl loongarch_boot_arg2
loongarch_boot_arg2:
    .dword 0
    .globl loongarch_boot_arg3
loongarch_boot_arg3:
    .dword 0

    .section .data.bootstate
    .balign 8
    .globl loongarch_boot_cpucfg1
loongarch_boot_cpucfg1:
    .dword 0
    .globl loongarch_boot_misc_before
loongarch_boot_misc_before:
    .dword 0
    .globl loongarch_boot_misc_after
loongarch_boot_misc_after:
    .dword 0
