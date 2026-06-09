    # TLB refill handler — must be 4K-aligned, placed first in trampoline section.
    # Uses hardware page-table walker: PGD → lddir × N → ldpte × 2 → tlbfill.
    .section .text.trampoline
    .equ      CSR_SAVE, 0x30
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
    move    $t0, $sp
    csrrd   $sp, CSR_SAVE
    st.d    $ra, $sp, 1*8
    st.d    $tp, $sp, 2*8
    st.d    $t0, $sp, 3*8
    st.d    $a0, $sp, 4*8
    st.d    $a1, $sp, 5*8
    st.d    $a2, $sp, 6*8
    st.d    $a3, $sp, 7*8
    st.d    $a4, $sp, 8*8
    st.d    $a5, $sp, 9*8
    st.d    $a6, $sp, 10*8
    st.d    $a7, $sp, 11*8
    st.d    $t1, $sp, 12*8
    st.d    $t2, $sp, 13*8
    st.d    $t3, $sp, 14*8
    st.d    $t4, $sp, 15*8
    st.d    $t5, $sp, 16*8
    st.d    $t6, $sp, 17*8
    st.d    $t7, $sp, 18*8
    st.d    $t8, $sp, 19*8
    st.d    $r21, $sp, 20*8
    st.d    $fp, $sp, 22*8
    st.d    $s0, $sp, 23*8
    st.d    $s1, $sp, 24*8
    st.d    $s2, $sp, 25*8
    st.d    $s3, $sp, 26*8
    st.d    $s4, $sp, 27*8
    st.d    $s5, $sp, 28*8
    st.d    $s6, $sp, 29*8
    st.d    $s7, $sp, 30*8
    st.d    $s8, $sp, 31*8

    csrrd   $t0, 0x1
    csrrd   $t1, 0x6
    st.d    $t0, $sp, 32*8
    st.d    $t1, $sp, 33*8

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
    ld.d    $a0, $sp, 4*8
    ld.d    $a1, $sp, 5*8
    ld.d    $a2, $sp, 6*8
    ld.d    $a3, $sp, 7*8
    ld.d    $a4, $sp, 8*8
    ld.d    $a5, $sp, 9*8
    ld.d    $a6, $sp, 10*8
    ld.d    $a7, $sp, 11*8
    ld.d    $t1, $sp, 12*8
    ld.d    $t2, $sp, 13*8
    ld.d    $t3, $sp, 14*8
    ld.d    $t4, $sp, 15*8
    ld.d    $t5, $sp, 16*8
    ld.d    $t6, $sp, 17*8
    ld.d    $t7, $sp, 18*8
    ld.d    $t8, $sp, 19*8
    ld.d    $r21, $sp, 20*8
    ld.d    $fp, $sp, 22*8
    ld.d    $s0, $sp, 23*8
    ld.d    $s1, $sp, 24*8
    ld.d    $s2, $sp, 25*8
    ld.d    $s3, $sp, 26*8
    ld.d    $s4, $sp, 27*8
    ld.d    $s5, $sp, 28*8
    ld.d    $s6, $sp, 29*8
    ld.d    $s7, $sp, 30*8
    ld.d    $s8, $sp, 31*8
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
