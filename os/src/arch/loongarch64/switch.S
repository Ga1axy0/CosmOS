    .section .text
    .globl __switch
__switch:
    st.d $sp, $a0, 8
    st.d $ra, $a0, 0
    st.d $fp, $a0, 16
    st.d $s0, $a0, 24
    st.d $s1, $a0, 32
    st.d $s2, $a0, 40
    st.d $s3, $a0, 48
    st.d $s4, $a0, 56
    st.d $s5, $a0, 64
    st.d $s6, $a0, 72
    st.d $s7, $a0, 80
    st.d $s8, $a0, 88

    # Save callee-saved FP regs $f24-$f31 into TaskContext.fs[0..7]
    # (offsets 14*8..21*8). The struct already reserves these slots; without
    # saving them, kernel-side FP state would leak across context switches.
    # FPEN is always on (see hart.rs), so fst.d does not trap here.
    fst.d $f24, $a0, 14*8
    fst.d $f25, $a0, 15*8
    fst.d $f26, $a0, 16*8
    fst.d $f27, $a0, 17*8
    fst.d $f28, $a0, 18*8
    fst.d $f29, $a0, 19*8
    fst.d $f30, $a0, 20*8
    fst.d $f31, $a0, 21*8

    ld.d $ra, $a1, 0
    ld.d $sp, $a1, 8
    ld.d $fp, $a1, 16
    ld.d $s0, $a1, 24
    ld.d $s1, $a1, 32
    ld.d $s2, $a1, 40
    ld.d $s3, $a1, 48
    ld.d $s4, $a1, 56
    ld.d $s5, $a1, 64
    ld.d $s6, $a1, 72
    ld.d $s7, $a1, 80
    ld.d $s8, $a1, 88
    # Restore callee-saved FP regs $f24-$f31 from TaskContext.fs[0..7].
    fld.d $f24, $a1, 14*8
    fld.d $f25, $a1, 15*8
    fld.d $f26, $a1, 16*8
    fld.d $f27, $a1, 17*8
    fld.d $f28, $a1, 18*8
    fld.d $f29, $a1, 19*8
    fld.d $f30, $a1, 20*8
    fld.d $f31, $a1, 21*8
    jr    $ra
