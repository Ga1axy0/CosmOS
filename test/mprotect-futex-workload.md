# mprotect/futex workload

`mprotect_futex_workload.c` is a repeatable workload for comparing the Linux
and CosmOS implementations of `mprotect(2)` and `futex(2)`.

Each worker owns an anonymous mapping. In every round it:

1. publishes a parked token and waits on a private futex;
2. changes one page `RW -> R`, reads it, then changes it `R -> RW` and writes;
3. publishes a completion token and wakes the main thread.

The test therefore exercises real wait/wake handshakes together with the page
permission changes used by JITs and language runtimes. `MPF_RESULT` contains
the workload's direct syscall counts and elapsed time. The expected number of
`mprotect` calls is exactly `workers * rounds * 2`; futex wait counts can vary
slightly because an expected-value race returns `EAGAIN` instead of sleeping.

## Run

The native-only smoke test needs only the host C compiler:

```sh
./test/run-mprotect-futex-compare.sh --host-only \
  --workers 4 --rounds 2048 --pages 32
```

With the existing RISC-V Linux image, public rootfs image, and CosmOS kernel:

```sh
SYSCALLS_COUNT=1 ./test/run-mprotect-futex-compare.sh --qemu-only \
  --workers 4 --rounds 2048 --pages 32
```

The runner builds one static RISC-V ELF, runs it in both Linux and CosmOS
QEMU, and prints an elapsed-time ratio. Use `--cosmos-only` when only the
CosmOS path is available. The default `SKIP_KERNEL_BUILD=0` rebuilds the
kernel; use `SKIP_KERNEL_BUILD=1` only when `kernel-rv` already contains the
desired configuration. `SYSCALLS_COUNT=1` includes `/proc/syscalls_count` in
the CosmOS log during a rebuild.

The `/proc/syscalls_count` values are a kernel-side cross-check. Each row now
contains `count total_ns avg_ns`; the timing covers syscall dispatch and the
syscall implementation, in nanoseconds, but not architecture trap entry or
return. They include small process-startup and counter-dump overhead around
the measured program, so the exact per-workload call count is the field in
`MPF_RESULT`.
