#!/bin/sh

# CosmOS guest side of test/run-statx-compare.sh.  The host runner stages the
# static benchmark at /root/statx_bench in a disposable bootstrap image.

set -u

PAYLOAD=""
ARGS_FILE=""
for candidate in \
    /root/statx_bench \
    /.cosmos-old-root/root/statx_bench
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
    echo "STATX_GUEST_ERROR missing benchmark payload" >&2
    echo "CARGO_PERF_DONE status=127"
    /bin/busybox poweroff -f 2>/dev/null || true
    exit 127
fi

if [ -n "$ARGS_FILE" ] && [ -s "$ARGS_FILE" ]; then
    set -- $(cat "$ARGS_FILE")
else
    set --
fi

# The first run is the latency comparison.  The optional io_perf counters and
# statx phase timers stay disabled, so their timer reads do not affect it.
if [ -w /proc/statx_perf_enable ]; then
    echo 0 > /proc/statx_perf_enable
fi
if [ -w /proc/io_perf ]; then
    echo 1 > /proc/io_perf
fi
echo STATX_COSMOS_BASELINE_BEGIN
"$PAYLOAD" "$@"
baseline_status=$?
echo "STATX_COSMOS_BASELINE_DONE status=$baseline_status"

profile_status=0
if [ -w /proc/statx_perf_enable ]; then
    # Reset after the baseline, then collect only the profiled benchmark.
    if [ -w /proc/io_perf ]; then
        echo 1 > /proc/io_perf
    fi
    echo 1 > /proc/statx_perf_enable
    echo STATX_COSMOS_PROFILE_BEGIN
    "$PAYLOAD" "$@"
    profile_status=$?
    echo STATX_COSMOS_PROFILE_DONE status=$profile_status
    echo STATX_COSMOS_IO_PERF_BEGIN
    cat /proc/io_perf
    echo STATX_COSMOS_IO_PERF_END
    echo 0 > /proc/statx_perf_enable
else
    echo STATX_COSMOS_PROFILE_SKIPPED
fi

status=$baseline_status
if [ "$status" -eq 0 ] && [ "$profile_status" -ne 0 ]; then
    status=$profile_status
fi
echo "STATX_COSMOS_DONE status=$status"
echo "CARGO_PERF_DONE status=$status"

sync 2>/dev/null || true
/bin/busybox poweroff -f 2>/dev/null || reboot -f 2>/dev/null || true
exit "$status"
