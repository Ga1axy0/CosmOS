#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

#[cfg(target_arch = "riscv64")]
mod riscv64 {
    use core::arch::global_asm;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use user_lib::{
        getcpu, sched_getaffinity, sched_setaffinity, sigaction, SignalAction, SIGUSR1,
    };

    const SYS_SCHED_SETAFFINITY: usize = 122;
    const SYS_SCHED_YIELD: usize = 124;
    const SYS_KILL: usize = 129;
    const SYS_GETCPU: usize = 168;
    const SYS_GETPPID: usize = 173;

    // RDN rounding mode plus the NV and NX accrued exception flags.  Every bit
    // is architecturally legal, and the value is deliberately nonzero.
    const TEST_FCSR: usize = (2 << 5) | 0x11;
    // Used by the signal handler to make a missing rt_sigreturn restore visible.
    const SIGNAL_FCSR: usize = (3 << 5) | 0x0c;
    const SNAPSHOT_WORDS: usize = 36;

    #[used]
    #[no_mangle]
    static FP_EXPECTED: [u64; 32] = [
        0x0123_4567_89ab_cdef,
        0x1021_3243_5465_7687,
        0x89ab_cdef_0123_4567,
        0xfedc_ba98_7654_3210,
        0x0011_2233_4455_6677,
        0x8899_aabb_ccdd_eeff,
        0x1357_9bdf_2468_ace0,
        0x0eca_8642_fdb9_7531,
        0x3ff0_0000_0000_0001,
        0xbff0_0000_0000_0002,
        0x4008_0000_0000_0003,
        0xc008_0000_0000_0004,
        0x7ff8_0000_0000_0005,
        0xfff8_0000_0000_0006,
        0x0000_0000_0000_0007,
        0x8000_0000_0000_0008,
        0x1111_2222_3333_4444,
        0x5555_6666_7777_8888,
        0x9999_aaaa_bbbb_cccc,
        0xdddd_eeee_ffff_0000,
        0x3141_5926_5358_9793,
        0x2384_6264_3383_2795,
        0xa5a5_5a5a_c3c3_3c3c,
        0x5a5a_a5a5_3c3c_c3c3,
        0x000f_00f0_0f00_f000,
        0xfff0_ff0f_f0ff_0fff,
        0x6a09_e667_f3bc_c908,
        0xbb67_ae85_84ca_a73b,
        0x3c6e_f372_fe94_f82b,
        0xa54f_f53a_5f1d_36f1,
        0x510e_527f_ade6_82d1,
        0x9b05_688c_2b3e_6c1f,
    ];

    #[used]
    #[no_mangle]
    static FP_SIGNAL_CLOBBER: [u64; 32] = [0xdead_beef_cafe_f00d; 32];

    #[used]
    #[no_mangle]
    static FP_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "C" {
        fn fp_probe_roundtrip(
            expected: *const u64,
            actual: *mut u64,
            fcsr: usize,
            syscall_or_migrate: usize,
            arg0: usize,
            arg1: usize,
            arg2: usize,
            repeat: usize,
        );
        fn fp_probe_signal_handler(signal: i32);
    }

    global_asm!(
        r#"
        .macro FP_LOAD_ALL base
        fld f0,    0(\base)
        fld f1,    8(\base)
        fld f2,   16(\base)
        fld f3,   24(\base)
        fld f4,   32(\base)
        fld f5,   40(\base)
        fld f6,   48(\base)
        fld f7,   56(\base)
        fld f8,   64(\base)
        fld f9,   72(\base)
        fld f10,  80(\base)
        fld f11,  88(\base)
        fld f12,  96(\base)
        fld f13, 104(\base)
        fld f14, 112(\base)
        fld f15, 120(\base)
        fld f16, 128(\base)
        fld f17, 136(\base)
        fld f18, 144(\base)
        fld f19, 152(\base)
        fld f20, 160(\base)
        fld f21, 168(\base)
        fld f22, 176(\base)
        fld f23, 184(\base)
        fld f24, 192(\base)
        fld f25, 200(\base)
        fld f26, 208(\base)
        fld f27, 216(\base)
        fld f28, 224(\base)
        fld f29, 232(\base)
        fld f30, 240(\base)
        fld f31, 248(\base)
        .endm

        .macro FP_STORE_ALL base
        fsd f0,    0(\base)
        fsd f1,    8(\base)
        fsd f2,   16(\base)
        fsd f3,   24(\base)
        fsd f4,   32(\base)
        fsd f5,   40(\base)
        fsd f6,   48(\base)
        fsd f7,   56(\base)
        fsd f8,   64(\base)
        fsd f9,   72(\base)
        fsd f10,  80(\base)
        fsd f11,  88(\base)
        fsd f12,  96(\base)
        fsd f13, 104(\base)
        fsd f14, 112(\base)
        fsd f15, 120(\base)
        fsd f16, 128(\base)
        fsd f17, 136(\base)
        fsd f18, 144(\base)
        fsd f19, 152(\base)
        fsd f20, 160(\base)
        fsd f21, 168(\base)
        fsd f22, 176(\base)
        fsd f23, 184(\base)
        fsd f24, 192(\base)
        fsd f25, 200(\base)
        fsd f26, 208(\base)
        fsd f27, 216(\base)
        fsd f28, 224(\base)
        fsd f29, 232(\base)
        fsd f30, 240(\base)
        fsd f31, 248(\base)
        .endm

        .section .text
        .align 2
        .global fp_probe_roundtrip
        .type fp_probe_roundtrip, @function
    fp_probe_roundtrip:
        addi sp, sp, -176
        sd s0,   0(sp)
        sd s1,   8(sp)
        sd s2,  16(sp)
        sd s3,  24(sp)
        sd s4,  32(sp)
        sd s5,  40(sp)
        sd s6,  48(sp)
        sd s8,  56(sp)

        # Preserve the ABI callee-saved FP state used by the Rust caller.
        # Reading fcsr also exercises the kernel's first-use (FS=Off) path.
        csrr t0, fcsr
        sd t0, 64(sp)
        fsd fs0,  72(sp)
        fsd fs1,  80(sp)
        fsd fs2,  88(sp)
        fsd fs3,  96(sp)
        fsd fs4, 104(sp)
        fsd fs5, 112(sp)
        fsd fs6, 120(sp)
        fsd fs7, 128(sp)
        fsd fs8, 136(sp)
        fsd fs9, 144(sp)
        fsd fs10, 152(sp)
        fsd fs11, 160(sp)

        mv s0, a0
        mv s1, a1
        mv s2, a2
        mv s3, a3
        mv s4, a4
        mv s5, a5
        mv s6, a6
        mv s8, a7

        FP_LOAD_ALL s0
        csrw fcsr, s2

        # syscall number zero is an internal sentinel for the migration
        # sequence: setaffinity -> sched_yield -> getcpu.
        beqz s3, .Lfp_probe_migrate
        beqz s8, .Lfp_probe_snapshot
    .Lfp_probe_syscall_loop:
        mv a0, s4
        mv a1, s5
        mv a2, s6
        mv a7, s3
        ecall
        addi s8, s8, -1
        bnez s8, .Lfp_probe_syscall_loop
        sd a0, 264(s1)
        j .Lfp_probe_snapshot

    .Lfp_probe_migrate:
        li a0, 0
        li a1, 8
        mv a2, s4
        li a7, {sys_sched_setaffinity}
        ecall
        sd a0, 264(s1)

        li a0, 0
        li a1, 0
        li a2, 0
        li a7, {sys_sched_yield}
        ecall
        sd a0, 272(s1)

        mv a0, s5
        li a1, 0
        li a2, 0
        li a7, {sys_getcpu}
        ecall
        sd a0, 280(s1)

    .Lfp_probe_snapshot:
        FP_STORE_ALL s1
        csrr t0, fcsr
        sd t0, 256(s1)

        fld fs0,  72(sp)
        fld fs1,  80(sp)
        fld fs2,  88(sp)
        fld fs3,  96(sp)
        fld fs4, 104(sp)
        fld fs5, 112(sp)
        fld fs6, 120(sp)
        fld fs7, 128(sp)
        fld fs8, 136(sp)
        fld fs9, 144(sp)
        fld fs10, 152(sp)
        fld fs11, 160(sp)
        ld t0, 64(sp)
        csrw fcsr, t0

        ld s0,   0(sp)
        ld s1,   8(sp)
        ld s2,  16(sp)
        ld s3,  24(sp)
        ld s4,  32(sp)
        ld s5,  40(sp)
        ld s6,  48(sp)
        ld s8,  56(sp)
        addi sp, sp, 176
        ret
        .size fp_probe_roundtrip, .-fp_probe_roundtrip

        # This is intentionally an ABI-hostile signal handler: it leaves all
        # 32 FPRs and fcsr changed.  rt_sigreturn must restore the interrupted
        # context from the signal frame, including the nominally callee-saved
        # FPRs.
        .align 2
        .global fp_probe_signal_handler
        .type fp_probe_signal_handler, @function
    fp_probe_signal_handler:
        lla t0, FP_SIGNAL_CLOBBER
        FP_LOAD_ALL t0
        li t0, {signal_fcsr}
        csrw fcsr, t0
        lla t0, FP_SIGNAL_COUNT
        li t1, 1
        amoadd.d zero, t1, (t0)
        ret
        .size fp_probe_signal_handler, .-fp_probe_signal_handler
        "#,
        signal_fcsr = const SIGNAL_FCSR,
        sys_sched_setaffinity = const SYS_SCHED_SETAFFINITY,
        sys_sched_yield = const SYS_SCHED_YIELD,
        sys_getcpu = const SYS_GETCPU,
    );

    fn run_roundtrip(
        label: &str,
        syscall_number: usize,
        args: [usize; 3],
        repeat: usize,
    ) -> ([u64; SNAPSHOT_WORDS], usize) {
        run_roundtrip_with_fcsr(label, TEST_FCSR, syscall_number, args, repeat)
    }

    fn run_roundtrip_with_fcsr(
        label: &str,
        expected_fcsr: usize,
        syscall_number: usize,
        args: [usize; 3],
        repeat: usize,
    ) -> ([u64; SNAPSHOT_WORDS], usize) {
        let mut actual = [0xfeed_face_dead_beef; SNAPSHOT_WORDS];
        unsafe {
            fp_probe_roundtrip(
                FP_EXPECTED.as_ptr(),
                actual.as_mut_ptr(),
                expected_fcsr,
                syscall_number,
                args[0],
                args[1],
                args[2],
                repeat,
            );
        }
        let failures = check_snapshot(label, expected_fcsr, &actual);
        (actual, failures)
    }

    fn check_snapshot(
        label: &str,
        expected_fcsr: usize,
        actual: &[u64; SNAPSHOT_WORDS],
    ) -> usize {
        let mut failures = 0;
        for (index, (&expected, &observed)) in
            FP_EXPECTED.iter().zip(actual[..32].iter()).enumerate()
        {
            if observed != expected {
                println!(
                    "fp_context_probe: {} f{} mismatch expected={:#018x} actual={:#018x}",
                    label, index, expected, observed
                );
                failures += 1;
            }
        }
        if actual[32] != expected_fcsr as u64 {
            println!(
                "fp_context_probe: {} fcsr mismatch expected={:#x} actual={:#x}",
                label, expected_fcsr, actual[32]
            );
            failures += 1;
        }
        failures
    }

    fn first_other_cpu(mask: usize, current: usize) -> Option<usize> {
        (0..usize::BITS as usize).find(|&cpu| cpu != current && mask & (1usize << cpu) != 0)
    }

    fn run_migration_probe() -> usize {
        let allowed = sched_getaffinity(0);
        let current = getcpu();
        if allowed < 0 || current < 0 {
            println!(
                "fp_context_probe: migration setup failed affinity={} cpu={}",
                allowed, current
            );
            return 1;
        }

        let allowed = allowed as usize;
        let current = current as usize;
        let Some(target) = first_other_cpu(allowed, current) else {
            println!(
                "fp_context_probe: migration SKIP (only cpu {} in affinity {:#x})",
                current, allowed
            );
            return 0;
        };

        let target_mask = 1usize << target;
        let mut observed_cpu = u32::MAX;
        let (actual, mut failures) = run_roundtrip(
            "migration",
            0,
            [
                &target_mask as *const usize as usize,
                &mut observed_cpu as *mut u32 as usize,
                0,
            ],
            0,
        );

        let restore = sched_setaffinity(0, allowed);
        let setaffinity_ret = actual[33] as isize;
        let yield_ret = actual[34] as isize;
        let getcpu_ret = actual[35] as isize;
        if setaffinity_ret != 0 || yield_ret != 0 || getcpu_ret != 0 || restore != 0 {
            println!(
                "fp_context_probe: migration syscalls failed set={} yield={} getcpu={} restore={}",
                setaffinity_ret, yield_ret, getcpu_ret, restore
            );
            failures += 1;
        }
        if observed_cpu as usize != target {
            println!(
                "fp_context_probe: migration did not reach target cpu={} observed={}",
                target, observed_cpu
            );
            failures += 1;
        } else {
            println!(
                "fp_context_probe: migration cpu {} -> {} preserved FP state",
                current, target
            );
        }
        failures
    }

    pub fn run() -> i32 {
        let mut failures = 0;

        let (getppid, mismatch) = run_roundtrip("getppid x1024", SYS_GETPPID, [0; 3], 1024);
        failures += mismatch;
        if (getppid[33] as isize) < 0 {
            println!(
                "fp_context_probe: getppid returned invalid value {}",
                getppid[33] as isize
            );
            failures += 1;
        }

        let (yielded, mismatch) = run_roundtrip("sched_yield x64", SYS_SCHED_YIELD, [0; 3], 64);
        failures += mismatch;
        if yielded[33] as isize != 0 {
            println!(
                "fp_context_probe: sched_yield returned {}",
                yielded[33] as isize
            );
            failures += 1;
        }

        // In the full-restore path a zero fcsr must be written explicitly: a
        // context switch or kernel FP use may otherwise leave nonzero live
        // frm/fflags behind.  Keep this separate from the nonzero-fcsr case so
        // both sides of the restore optimization remain covered.
        let (zero_fcsr_yield, mismatch) = run_roundtrip_with_fcsr(
            "zero fcsr + sched_yield x64",
            0,
            SYS_SCHED_YIELD,
            [0; 3],
            64,
        );
        failures += mismatch;
        if zero_fcsr_yield[33] as isize != 0 {
            println!(
                "fp_context_probe: zero-fcsr sched_yield returned {}",
                zero_fcsr_yield[33] as isize
            );
            failures += 1;
        }

        FP_SIGNAL_COUNT.store(0, Ordering::SeqCst);
        let action = SignalAction {
            handler: fp_probe_signal_handler as usize,
            sa_flags: 0,
            sa_mask: 0,
        };
        if sigaction(SIGUSR1, Some(&action), None) != 0 {
            println!("fp_context_probe: sigaction(SIGUSR1) failed");
            failures += 1;
        } else {
            let pid = user_lib::getpid();
            let (signaled, mismatch) = run_roundtrip(
                "signal/rt_sigreturn",
                SYS_KILL,
                [pid as usize, SIGUSR1 as usize, 0],
                1,
            );
            failures += mismatch;
            if signaled[33] as isize != 0 {
                println!(
                    "fp_context_probe: kill(SIGUSR1) returned {}",
                    signaled[33] as isize
                );
                failures += 1;
            }
            let seen = FP_SIGNAL_COUNT.load(Ordering::SeqCst);
            if seen != 1 {
                println!(
                    "fp_context_probe: signal handler ran {} times instead of once",
                    seen
                );
                failures += 1;
            }
        }

        failures += run_migration_probe();

        if failures == 0 {
            println!("fp_context_probe: PASS");
            0
        } else {
            println!("fp_context_probe: FAIL failures={}", failures);
            1
        }
    }
}

#[cfg(target_arch = "riscv64")]
#[no_mangle]
fn main() -> i32 {
    riscv64::run()
}

#[cfg(not(target_arch = "riscv64"))]
#[no_mangle]
fn main() -> i32 {
    println!("fp_context_probe: SKIP (RISC-V only)");
    0
}
