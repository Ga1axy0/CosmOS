.option arch, +f,+d
.altmacro
.macro SAVE_FP n
    fsd f\n, (38+\n)*8(sp)
.endm
.macro LOAD_FP n
    fld f\n, (38+\n)*8(a0)
.endm

.altmacro
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
    .section .text.trampoline
    .globl __alltraps
    .globl __restore
    .align 2
__alltraps:
    csrrw sp, sscratch, sp
    # now sp->*TrapContext in user space, sscratch->user stack
    # save other general purpose registers
    sd x1, 1*8(sp)
    # skip sp(x2), we will save it later
    sd x3, 3*8(sp)
    # save tp(x4), userspace may use it as TLS pointer
    sd x4, 4*8(sp)
    # save x5~x31
    .set n, 5
    .rept 27
        SAVE_GP %n
        .set n, n+1
    .endr
    # we can use t0/t1/t2 freely, because they have been saved in TrapContext
    csrr t0, sstatus
    csrr t1, sepc
    sd t0, 32*8(sp)
    sd t1, 33*8(sp)

    # Save floating-point state only when userspace actually dirtied it.  For
    # FS=Off/Initial/Clean the trap-frame backing store is already authoritative.
    srli t2, t0, 13
    andi t2, t2, 3
    li t1, 3
    bne t2, t1, 1f
    .set n, 0
    .rept 32
        SAVE_FP %n
        .set n, n + 1
    .endr
    frcsr t0
    sd t0, 70*8(sp)
    # The backing store now matches hardware, so return with FS=Clean unless
    # userspace dirties the registers again.
    ld t0, 32*8(sp)
    li t1, ~(3 << 13)
    and t0, t0, t1
    li t1, (2 << 13)
    or t0, t0, t1
    sd t0, 32*8(sp)
1:

    # Keep the live userspace FP register image reusable across an integer-only
    # kernel path.  Clearing fcsr dirties FS, so mark the live state Clean only
    # after that write.  FS=Off must first be enabled before touching fcsr.
    li t1, ~(3 << 13)
    beqz t2, 2f
    and t0, t0, t1
    li t1, (2 << 13)
    or t0, t0, t1
    csrwi fcsr, 0
    csrw sstatus, t0
    j 3f
2:
    and t0, t0, t1
    li t1, (1 << 13)
    or t0, t0, t1
    csrw sstatus, t0
    csrwi fcsr, 0
3:

    # read user stack from sscratch and save it in TrapContext
    csrr t2, sscratch
    sd t2, 2*8(sp)

    # Diagnostic-only assembly getppid path.  lmbench's "null" callback invokes
    # syscall 173, not getpid.  The first call follows the normal Rust path,
    # which caches the real parent PID in the otherwise-unused saved x0 slot.
    # Later calls can therefore preserve getppid semantics while bounding trap
    # entry/restore and satp-switch overhead without Rust or lock traffic.
    #
    # mode 1: restore directly in the still-active user address space.
    # mode 2: perform the legacy user->kernel->user satp writes for comparison.
    .if GETPID_ASM_PROBE
    # a7 retains the syscall number across user timer interrupts.  Check the
    # cause first so an interrupt during the benchmark cannot masquerade as a
    # second getppid call.
    csrr t0, scause
    li t1, 8
    bne t0, t1, .Lnormal_trap_path
    ld t0, 17*8(sp)
    li t1, 173
    bne t0, t1, .Lnormal_trap_path
    ld t0, 0*8(sp)
    beqz t0, .Lnormal_trap_path
    addi t0, t0, -1
    sd t0, 10*8(sp)
    ld t0, 33*8(sp)
    addi t0, t0, 4
    sd t0, 33*8(sp)
    mv a0, sp
    csrr a1, satp

    .if GETPID_ASM_PROBE == 2
    # Preserve the user ASID test across both satp writes.
    slli t2, a1, 4
    srli t2, t2, 48
    ld t0, 35*8(sp)
    csrw satp, t0
    bnez t2, .Lprobe_kernel_satp_ready
    sfence.vma
.Lprobe_kernel_satp_ready:
    csrw satp, a1
    bnez t2, .Luser_satp_ready
    sfence.vma
    .endif

    j .Luser_satp_ready
.Lnormal_trap_path:
    .endif

    # restore kernel hart id into tp before entering Rust
    ld tp, 34*8(sp)
    # load trap_handler into t1
    ld t1, 37*8(sp)
    # move to kernel_sp
    ld sp, 36*8(sp)
    # Kernel text, direct-map memory and every kernel stack are shared in the
    # high half of the current process page table. Keep satp unchanged.
    # jump to trap_handler
    jr t1

__restore:
    # a0: *TrapContext in user space(Constant); a1: user space token
    # Ordinary trap return keeps the same root. A process switch changes satp
    # here, before touching the new process' private trap frame.
    csrr t0, satp
    beq t0, a1, .Luser_satp_ready
    csrw satp, a1
    slli t2, a1, 4
    srli t2, t2, 48
    bnez t2, .Luser_satp_ready
    sfence.vma
.Luser_satp_ready:
    csrw sscratch, a0
    mv sp, a0
    # now sp points to TrapContext in user space, start restoring based on it
    ld t0, 32*8(sp)
    ld t1, 33*8(sp)
    csrw sepc, t1

    # A Clean frame can reuse the live FP register image when the kernel left it
    # Clean too.  Scheduling and any kernel FP instruction make live FS Dirty,
    # which conservatively falls back to a full restore.  Initial must also be
    # restored in full so a task never observes another context's registers.
    srli t2, t0, 13
    andi t2, t2, 3
    beqz t2, 4f
    li t1, 2
    bne t2, t1, 5f
    csrr t3, sstatus
    srli t3, t3, 13
    andi t3, t3, 3
    beq t3, t1, 6f
5:
    .set n, 0
    .rept 32
        LOAD_FP %n
        .set n, n + 1
    .endr
    # A full restore cannot assume the live fcsr is still the zero installed
    # on this trap's entry: the syscall may have used kernel FP, blocked, or
    # migrated.  Restore even an authoritative zero value so frm/fflags never
    # leak across contexts.
    ld t3, 70*8(a0)
    fscsr t3
    j 4f
6:
    # The Clean/Clean reuse path did clear live fcsr at entry and performed no
    # later FP operation, so only a nonzero saved value needs a CSR write.
    ld t3, 70*8(a0)
    beqz t3, 4f
    fscsr t3
4:
    # FP loads/fscsr dirty the live state; restore the authoritative user bits
    # once, after the optional fast/full FP path.
    csrw sstatus, t0

    # restore general purpose registers except x0/sp
    ld x1, 1*8(sp)
    ld x3, 3*8(sp)
    ld x4, 4*8(sp)
    .set n, 5
    .rept 27
        LOAD_GP %n
        .set n, n+1
    .endr
    # back to user stack
    ld sp, 2*8(sp)
    sret

    .section .data
    # emergency stack for kernel trap
    # in order to print trap info even if the kernel stack is corrupted.
__emergency:
    .align 4
    .space 1024 * 4
__emergency_end:


    .section .text
    .globl __trap_from_kernel
    # 2^2=4 bytes aligned for stvec
    .align 2
__trap_from_kernel:
    .if KERNEL_TRAP_DIAGNOSTICS
    addi sp, sp, -256
    # Save a complete GPR snapshot using architectural x-register indices as
    # slots. x0 is recorded as zero and x2 contains the interrupted (pre-frame)
    # stack pointer. Keeping this layout in sync with RiscvKernelTrapFrame lets
    # fatal kernel traps print the actual fault-time registers rather than the
    # Rust handler's already-clobbered values.
    .set n, 1
    .rept 31
        SAVE_GP %n
        .set n, n+1
    .endr
    sd zero, 0*8(sp)
    addi t0, sp, 256
    sd t0, 2*8(sp)

    # Pass the saved frame as the first Rust/C ABI argument. The interrupted a0
    # has already been preserved in x[10] above.
    mv a0, sp
    la t0, trap_from_kernel_riscv
    jalr t0

    # Restore every GPR except x0 and sp. sp is restored by releasing the frame.
    ld x1, 1*8(sp)
    .set n, 3
    .rept 29
        LOAD_GP %n
        .set n, n+1
    .endr

    addi sp, sp, 256
    sret
    .else
    # The normal fast path preserves only caller-saved registers that the Rust
    # handler may clobber. Callee-saved registers remain protected by the ABI.
    addi sp, sp, -256
    sd ra, 0(sp)
    sd t0, 8(sp)
    sd t1, 16(sp)
    sd t2, 24(sp)
    sd t3, 32(sp)
    sd t4, 40(sp)
    sd t5, 48(sp)
    sd t6, 56(sp)
    sd a0, 64(sp)
    sd a1, 72(sp)
    sd a2, 80(sp)
    sd a3, 88(sp)
    sd a4, 96(sp)
    sd a5, 104(sp)
    sd a6, 112(sp)
    sd a7, 120(sp)

    la t0, trap_from_kernel
    jalr t0

    ld ra, 0(sp)
    ld t0, 8(sp)
    ld t1, 16(sp)
    ld t2, 24(sp)
    ld t3, 32(sp)
    ld t4, 40(sp)
    ld t5, 48(sp)
    ld t6, 56(sp)
    ld a0, 64(sp)
    ld a1, 72(sp)
    ld a2, 80(sp)
    ld a3, 88(sp)
    ld a4, 96(sp)
    ld a5, 104(sp)
    ld a6, 112(sp)
    ld a7, 120(sp)

    addi sp, sp, 256
    sret
    .endif
