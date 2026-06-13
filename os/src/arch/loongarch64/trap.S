    # TLB refill handler — must be 4K-aligned, placed first in trampoline section.
    # Uses hardware page-table walker: PGD → lddir × N → ldpte × 2 → tlbfill.
    .section .text.trampoline
    .equ      CSR_SAVE, 0x30
    .equ      CSR_SAVE1, 0x31
    .altmacro
    .macro    STORE_X n
    st.d      $r\n, $sp, \n*8
    .endm
    .macro    LOAD_X n
    ld.d      $r\n, $sp, \n*8
    .endm
    .globl __tlb_refill
    .balign 4096
__tlb_refill:
    csrwr   $t0, 0x8b          # save t0 to TLBRSAVE
    csrrd   $t0, 0x1b          # t0 = effective PGD selected by hardware
    lddir   $t0, $t0, 2        # walk root → mid (Dir2 → Dir1)
    beqz    $t0, 1f
    lddir   $t0, $t0, 1        # walk mid → leaf PT
    beqz    $t0, 1f
    ldpte   $t0, 0
    ldpte   $t0, 1
    b       2f
1:
    csrwr   $zero, 0x8c
    csrwr   $zero, 0x8d
2:
    tlbfill
    csrrd   $t0, 0x8b
    ertn

    .globl __alltraps
    .globl __restore
    .align 3
__alltraps:
    # Swap user sp with the per-task trap context pointer kept in CSR_SAVE.
    # This preserves every user GPR, including t0/r12.
    csrwr   $sp, CSR_SAVE
    st.d    $ra, $sp, 1*8
    st.d    $tp, $sp, 2*8
    .set    n, 4
    .rept   28
    STORE_X %n
    .set    n, n + 1
    .endr

    csrrd   $t0, 0x1
    csrrd   $t1, 0x6
    # CSR_SAVE now holds the interrupted user sp after the swap above.
    csrrd   $t2, CSR_SAVE
    st.d    $t0, $sp, 32*8
    st.d    $t1, $sp, 33*8
    st.d    $t2, $sp, 3*8

    # Keep the first user-trap path free of FP instructions.
    # LoongArch can trap on FP use when EUEN/FPU state is not fully prepared,
    # and nested traps here corrupt the user trap frame before Rust sees it.
    st.d    $zero, $sp, 70*8

    ld.d    $tp, $sp, 34*8
    ld.d    $t0, $sp, 35*8
    ld.d    $t1, $sp, 37*8
    ld.d    $sp, $sp, 36*8
    csrwr   $t0, 0x19
    invtlb  0x00, $zero, $zero
    jr      $t1

__restore:
    csrwr   $a0, CSR_SAVE
    csrrd   $a0, CSR_SAVE
    csrwr   $a1, 0x19
    invtlb  0x00, $zero, $zero
    move    $sp, $a0
    ld.d    $t0, $sp, 32*8
    ld.d    $t1, $sp, 33*8
    csrwr   $t0, 0x1
    csrwr   $t1, 0x6
    ld.d    $ra, $sp, 1*8
    ld.d    $tp, $sp, 2*8
    .set    n, 4
    .rept   28
    LOAD_X  %n
    .set    n, n + 1
    .endr
    ld.d    $sp, $sp, 3*8
    ertn

    .section .text
    .globl __trap_from_kernel
    .align 3
__trap_from_kernel:
    addi.d  $sp, $sp, -256
    st.d    $ra, $sp, 0
    st.d    $a0, $sp, 8
    st.d    $a1, $sp, 16
    st.d    $t0, $sp, 24
    st.d    $t1, $sp, 32
    la.local  $t0, trap_from_kernel
    jirl      $ra, $t0, 0
    ld.d    $ra, $sp, 0
    ld.d    $a0, $sp, 8
    ld.d    $a1, $sp, 16
    ld.d    $t0, $sp, 24
    ld.d    $t1, $sp, 32
    addi.d  $sp, $sp, 256
    ertn
