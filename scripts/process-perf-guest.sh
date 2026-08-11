#!/bin/sh

# Warm process-lifecycle microbenchmark shared by CosmOS and Linux.  It keeps
# the executable pages hot so the result emphasizes vfork/execve/wait and
# address-space setup instead of first-read disk latency.

set -u

CARGO_HOME=${CARGO_HOME:-/root/.cargo}
RUSTUP_HOME=${RUSTUP_HOME:-/root/.rustup}
PATH="$CARGO_HOME/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
HOME=/root
export CARGO_HOME RUSTUP_HOME PATH HOME

say() {
    echo "[process-perf] $*"
}

uptime_now() {
    read uptime_value _ < /proc/uptime
    echo "$uptime_value"
}

reset_probes() {
    if [ -w /proc/perf_probe_enable ]; then
        echo 0 > /proc/perf_probe_enable
        echo 1 > /proc/perf_probe
        echo 1 > /proc/perf_probe_enable
    fi
}

dump_probes() {
    if [ -w /proc/perf_probe_enable ]; then
        echo 0 > /proc/perf_probe_enable
    fi
    echo "===== PROCESS_PERF_PROBE_BEGIN ====="
    if [ -r /proc/perf_probe ]; then
        cat /proc/perf_probe
    fi
    echo "===== PROCESS_PERF_PROBE_END ====="
}

run_true_loop() {
    count=$1
    i=0
    while [ "$i" -lt "$count" ]; do
        /bin/true
        i=$((i + 1))
    done
}

run_version_loop() {
    count=$1
    shift
    i=0
    while [ "$i" -lt "$count" ]; do
        "$@" >/dev/null
        i=$((i + 1))
    done
}

measure() {
    name=$1
    shift
    start=$(uptime_now)
    "$@"
    status=$?
    end=$(uptime_now)
    say "phase=$name start_uptime_s=$start end_uptime_s=$end status=$status"
    return "$status"
}

say "PROCESS_PERF_BEGIN"
say "cargo=$(command -v cargo 2>/dev/null || echo missing)"
say "rustc=$(command -v rustc 2>/dev/null || echo missing)"

# Warm the executable and dynamic-linker pages before enabling probes.
/bin/true
cargo --version >/dev/null || exit 127
rustc --version >/dev/null || exit 127

reset_probes
measure true_x300 run_true_loop 300 || exit $?
measure cargo_version_x20 run_version_loop 20 cargo --version || exit $?
measure rustc_version_x20 run_version_loop 20 rustc --version || exit $?
dump_probes
say "PROCESS_PERF_DONE status=0"
echo "[cargo-perf] CARGO_PERF_DONE status=0"

poweroff -f 2>/dev/null \
    || /bin/busybox poweroff -f 2>/dev/null \
    || reboot -f 2>/dev/null \
    || exit 0
