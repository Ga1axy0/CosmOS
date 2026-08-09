#!/bin/sh

# Runs as the temporary final_auto_run inside the pivoted public RISC-V image.
# Keep this script POSIX-sh compatible: pivot-eval-root invokes it via /bin/sh.

set -u

PROJECT_DIR=/root/cosmos-cargo-perf
CARGO_HOME=${CARGO_HOME:-/root/.cargo}
RUSTUP_HOME=${RUSTUP_HOME:-/root/.rustup}
PATH="$CARGO_HOME/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
HOME=/root
export CARGO_HOME RUSTUP_HOME PATH HOME

say() {
    echo "[cargo-perf] $*"
}

snapshot_meminfo() {
    label=$1
    echo "===== CARGO_PERF_${label}_MEMINFO_BEGIN ====="
    cat /proc/cosmos_meminfo
    echo "===== CARGO_PERF_${label}_MEMINFO_END ====="
}

reset_phase_counters() {
    # Disable first so shell setup and counter reset do not leak into the
    # measured interval. /proc/io_perf resets all FS and block-I/O counters.
    echo 0 > /proc/perf_probe_enable
    echo 1 > /proc/perf_probe
    echo 1 > /proc/io_perf
    echo 1 > /proc/perf_probe_enable
}

uptime_now() {
    # /proc/uptime is available before the public root has any optional timing
    # utilities installed.  Emit the raw seconds value and let the host-side
    # harness calculate the difference without perturbing the measured phase.
    read uptime_value _ < /proc/uptime
    echo "$uptime_value"
}

dump_phase_counters() {
    phase=$1
    # Stop timing before spawning cat. io_perf may include the tiny cost of
    # opening procfs, but no later workload can perturb the timing probes.
    echo 0 > /proc/perf_probe_enable
    echo "===== CARGO_PERF_${phase}_IO_BEGIN ====="
    cat /proc/io_perf
    echo "===== CARGO_PERF_${phase}_IO_END ====="
    echo "===== CARGO_PERF_${phase}_PROBE_BEGIN ====="
    cat /proc/perf_probe
    echo "===== CARGO_PERF_${phase}_PROBE_END ====="
}

finish() {
    status=$1
    say "CARGO_PERF_DONE status=$status"
    sync
    # The benchmark runner is PID 1. Prefer an explicit shutdown, but exiting
    # still lets the kernel terminate the guest if the userspace command is
    # unavailable in a future public image.
    poweroff -f 2>/dev/null || reboot -f 2>/dev/null || exit "$status"
}

say "CARGO_PERF_BEGIN"
say "cargo=$(command -v cargo 2>/dev/null || echo missing)"
cargo --version || finish 127
rustc --version || finish 127

# The host uses disposable disk overlays, and the explicit path prevents an
# old interrupted benchmark from contaminating cargo-new behavior.
rm -rf "$PROJECT_DIR"

snapshot_meminfo NEW_BEFORE
reset_phase_counters
new_start=$(uptime_now)
echo "===== CARGO_PERF_NEW_COMMAND_BEGIN ====="
cargo new --vcs none "$PROJECT_DIR"
new_status=$?
new_end=$(uptime_now)
say "cargo_new start_uptime_s=$new_start end_uptime_s=$new_end status=$new_status"
echo "===== CARGO_PERF_NEW_COMMAND_END status=$new_status ====="
dump_phase_counters NEW
snapshot_meminfo NEW_AFTER
if [ "$new_status" -ne 0 ]; then
    finish "$new_status"
fi

cd "$PROJECT_DIR" || finish 1
snapshot_meminfo RUN_BEFORE
reset_phase_counters
run_start=$(uptime_now)
echo "===== CARGO_PERF_RUN_COMMAND_BEGIN ====="
cargo run --release --offline
run_status=$?
run_end=$(uptime_now)
say "cargo_run_release start_uptime_s=$run_start end_uptime_s=$run_end status=$run_status"
echo "===== CARGO_PERF_RUN_COMMAND_END status=$run_status ====="
dump_phase_counters RUN
snapshot_meminfo RUN_AFTER

finish "$run_status"
