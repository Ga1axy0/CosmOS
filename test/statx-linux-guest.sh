#!/bin/sh

# Linux guest side of test/run-statx-compare.sh.  The benchmark binary is
# exported read-only through 9p at /host so this uses the same static RISC-V
# executable that the CosmOS guest receives in its temporary bootstrap root.

set -u

PAYLOAD=/host/.make/statx-bench/statx_bench
ARGS_FILE=/host/.make/statx-bench/guest-args

if [ ! -x "$PAYLOAD" ]; then
    echo "STATX_GUEST_ERROR missing $PAYLOAD" >&2
    echo "CARGO_PERF_DONE status=127"
    /bin/busybox poweroff -f 2>/dev/null || true
    exit 127
fi

if [ -s "$ARGS_FILE" ]; then
    set -- $(cat "$ARGS_FILE")
else
    set --
fi

echo STATX_LINUX_BEGIN
"$PAYLOAD" "$@"
status=$?
echo "STATX_LINUX_DONE status=$status"
echo "CARGO_PERF_DONE status=$status"

/bin/busybox poweroff -f 2>/dev/null || reboot -f 2>/dev/null || true
exit "$status"
