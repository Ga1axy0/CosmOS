#!/bin/sh

# Guest runner for scripts/run-linux-cargo-perf-bench.sh. The Linux QEMU
# runner exports the repository read-only at /host, so it can execute the same
# static RISC-V payload that the CosmOS runner stages into its bootstrap image.

set -u

PAYLOAD=/host/.make/mprotect-futex/workload-rv
ARGS_FILE=/host/.make/mprotect-futex/guest-payload-args

if [ ! -x "$PAYLOAD" ]; then
    echo "MPF_GUEST_ERROR missing $PAYLOAD" >&2
    echo "CARGO_PERF_DONE status=127"
    /bin/busybox poweroff -f 2>/dev/null || true
    exit 127
fi

if [ -s "$ARGS_FILE" ]; then
    set -- $(cat "$ARGS_FILE")
else
    set --
fi

"$PAYLOAD" "$@"
status=$?
echo "LINUX_MPF_DONE status=$status"
echo "CARGO_PERF_DONE status=$status"

/bin/busybox poweroff -f 2>/dev/null || reboot -f 2>/dev/null || true
exit "$status"
