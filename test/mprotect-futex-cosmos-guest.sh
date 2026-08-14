#!/bin/sh

# This script is staged as final_auto_run by run-cargo-perf-bench.sh. In
# pivot-root mode the payload and its argument file remain on the bootstrap
# root at /.cosmos-old-root after the public image becomes /.

set -u

PAYLOAD=""
ARGS_FILE=""
for candidate in \
    /root/mprotect-futex-workload \
    /.cosmos-old-root/root/mprotect-futex-workload
do
    if [ -x "$candidate" ]; then
        PAYLOAD=$candidate
        break
    fi
done

for candidate in \
    /root/guest-payload-args \
    /.cosmos-old-root/root/guest-payload-args
do
    if [ -f "$candidate" ]; then
        ARGS_FILE=$candidate
        break
    fi
done

if [ -z "$PAYLOAD" ]; then
    echo "MPF_GUEST_ERROR missing workload payload" >&2
    echo "CARGO_PERF_DONE status=127"
    /bin/busybox poweroff -f 2>/dev/null || true
    exit 127
fi

# The write side of /proc/syscalls_count resets the optional CosmOS counter.
# If the kernel was built without SYSCALLS_COUNT, the workload still runs and
# its own deterministic call counters remain available for timing comparison.
if [ -w /proc/syscalls_count ]; then
    echo 1 > /proc/syscalls_count
fi

if [ -n "$ARGS_FILE" ] && [ -s "$ARGS_FILE" ]; then
    # The host runner writes only numeric workload options; word splitting is
    # intentional here and keeps the payload independent of a shell parser.
    set -- $(cat "$ARGS_FILE")
else
    set --
fi

"$PAYLOAD" "$@"
status=$?

if [ -r /proc/syscalls_count ]; then
    echo "COSMOS_SYSCALL_COUNTS_BEGIN"
    cat /proc/syscalls_count
    echo "COSMOS_SYSCALL_COUNTS_END"
fi
echo "COSMOS_MPF_DONE status=$status"
echo "CARGO_PERF_DONE status=$status"

sync 2>/dev/null || true
/bin/busybox poweroff -f 2>/dev/null || reboot -f 2>/dev/null || true
exit "$status"
