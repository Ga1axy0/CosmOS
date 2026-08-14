#!/usr/bin/env bash

set -euo pipefail

# Build one static raw-statx workload and run it in Linux and CosmOS RISC-V
# guests.  CosmOS is run once with normal latency settings and, optionally, a
# second time with the dynamic statx phase timers enabled.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK_DIR="${WORK_DIR:-$PROJECT_ROOT/.make/statx-bench}"
OUT_DIR="${OUT_DIR:-$PROJECT_ROOT/.make/cargo-perf}"
RV_CC="${RV_CC:-/opt/riscv64-linux-musl-cross/bin/riscv64-linux-musl-gcc}"
LINUX_IMAGE="${LINUX_IMAGE:-$PROJECT_ROOT/.make/linux-rv/arch/riscv/boot/Image}"
PUBLIC_IMAGE="${PUBLIC_IMAGE:-$PROJECT_ROOT/sdcard-rv-pub.img}"
ITERATIONS="${ITERATIONS:-100000}"
FILES="${FILES:-128}"
PASSES="${PASSES:-16}"
SMP_COUNT="${SMP_COUNT:-1}"
RUN_TIMEOUT="${RUN_TIMEOUT:-240}"
SKIP_KERNEL_BUILD="${SKIP_KERNEL_BUILD:-0}"
SKIP_COMPILE="${SKIP_COMPILE:-0}"
PROFILE="${PROFILE:-1}"
MODE=all
LABEL=""

usage() {
    cat <<EOF
Usage: ${0##*/} [--linux-only|--cosmos-only|--profile-only] [label]

The same static RISC-V raw SYS_statx workload is used in both guests.
Environment overrides:
  RV_CC LINUX_IMAGE PUBLIC_IMAGE ITERATIONS FILES PASSES SMP_COUNT
  RUN_TIMEOUT SKIP_KERNEL_BUILD PROFILE OUT_DIR WORK_DIR
  SKIP_COMPILE=1 reuses an existing WORK_DIR/statx_bench ELF.

PROFILE=1 (default) performs an additional CosmOS run with /proc/io_perf and
/proc/statx_perf_enable enabled.  Set PROFILE=0 for only the fair latency run.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --linux-only)
            MODE=linux
            shift
            ;;
        --cosmos-only)
            MODE=cosmos
            shift
            ;;
        --profile-only)
            MODE=profile
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --*)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            [[ -z "$LABEL" ]] || {
                echo "only one label is allowed" >&2
                exit 2
            }
            LABEL=$1
            shift
            ;;
    esac
done

if [[ -z "$LABEL" ]]; then
    LABEL="statx-compare-$(date +%Y%m%d-%H%M%S)"
fi

mkdir -p "$WORK_DIR" "$OUT_DIR"
BENCHMARK="$WORK_DIR/statx_bench"
ARGS_FILE="$WORK_DIR/guest-args"

compile_benchmark() {
    if [[ "$SKIP_COMPILE" == 1 ]]; then
        [[ -x "$BENCHMARK" ]] || {
            echo "SKIP_COMPILE=1 but benchmark is missing: $BENCHMARK" >&2
            exit 2
        }
        echo "[statx] reusing static RISC-V benchmark: $BENCHMARK"
    else
        [[ -x "$RV_CC" ]] || {
            echo "RISC-V compiler not found: $RV_CC" >&2
            exit 2
        }
        echo "[statx] compiling static RISC-V benchmark: $BENCHMARK"
        "$RV_CC" -std=c11 -O2 -static -Wall -Wextra -Werror \
            "$SCRIPT_DIR/statx_bench.c" -o "$BENCHMARK"
    fi
    printf '%s\n' \
        --iterations "$ITERATIONS" \
        --files "$FILES" \
        --passes "$PASSES" >"$ARGS_FILE"
}

run_linux() {
    local run_label="$LABEL-linux"

    [[ -f "$LINUX_IMAGE" ]] || {
        echo "missing Linux RISC-V image: $LINUX_IMAGE" >&2
        exit 2
    }
    [[ -f "$PUBLIC_IMAGE" ]] || {
        echo "missing public image: $PUBLIC_IMAGE" >&2
        exit 2
    }
    echo "[statx] running Linux guest: $run_label"
    OUT_DIR="$OUT_DIR" \
    LINUX_IMAGE="$LINUX_IMAGE" \
    PUBLIC_IMAGE="$PUBLIC_IMAGE" \
    GUEST_RUNNER="$SCRIPT_DIR/statx-linux-guest.sh" \
    SMP_COUNT="$SMP_COUNT" \
    RUN_TIMEOUT="$RUN_TIMEOUT" \
        "$PROJECT_ROOT/scripts/run-linux-cargo-perf-bench.sh" "$run_label"
}

run_cosmos() {
    local run_label="$1"
    local io_perf="$2"

    echo "[statx] running CosmOS guest: $run_label (io_perf=$io_perf)"
    OUT_DIR="$OUT_DIR" \
    GUEST_RUNNER="$SCRIPT_DIR/statx-cosmos-guest.sh" \
    GUEST_PAYLOAD="$BENCHMARK" \
    GUEST_PAYLOAD_DEST=/root/statx_bench \
    GUEST_PAYLOAD_ARGS="$(<"$ARGS_FILE")" \
    PUBLIC_IMAGE="$PUBLIC_IMAGE" \
    SMP_COUNT="$SMP_COUNT" \
    RUN_TIMEOUT="$RUN_TIMEOUT" \
    PERF_PROBE_VALUE=0 \
    SYSCALLS_COUNT=0 \
    IO_PERF_COUNTERS="$io_perf" \
    SKIP_KERNEL_BUILD="$SKIP_KERNEL_BUILD" \
        "$PROJECT_ROOT/scripts/run-cargo-perf-bench.sh" "$run_label"
}

metric() {
    local log_path="$1"
    local case_name="$2"
    awk -v wanted="case=$case_name" '
        $1 == "STATX_RESULT" && $2 == wanted {
            for (i = 1; i <= NF; ++i) {
                split($i, pair, "=")
                if (pair[1] == "ns_per_call") {
                    print pair[2]
                    exit
                }
            }
        }
    ' "$log_path"
}

print_comparison() {
    local linux_log="$OUT_DIR/$LABEL-linux.log"
    local cosmos_log="$OUT_DIR/$LABEL-cosmos.log"

    [[ -f "$linux_log" && -f "$cosmos_log" ]] || return 0
    echo
    echo "==== raw statx latency (same static RISC-V binary) ===="
    printf '%-20s %16s %16s %12s\n' case linux_ns cosmos_ns ratio
    for case_name in path_fixed empty_path relative_dirfd path_rotate_first path_rotate_repeat; do
        local linux_value cosmos_value
        linux_value=$(metric "$linux_log" "$case_name")
        cosmos_value=$(metric "$cosmos_log" "$case_name")
        [[ -n "$linux_value" && -n "$cosmos_value" ]] || continue
        awk -v name="$case_name" -v linux="$linux_value" -v cosmos="$cosmos_value" \
            'BEGIN { printf "%-20s %16.3f %16.3f %11.3fx\n", name, linux, cosmos, cosmos / linux }'
    done
    echo
    echo "logs:"
    echo "  Linux:  $linux_log"
    echo "  CosmOS: $cosmos_log"
}

compile_benchmark
case "$MODE" in
    linux)
        run_linux
        ;;
    cosmos)
        run_cosmos "$LABEL-cosmos" 0
        ;;
    profile)
        run_cosmos "$LABEL-profile" 1
        ;;
    all)
        run_linux
        run_cosmos "$LABEL-cosmos" 0
        if [[ "$PROFILE" == 1 ]]; then
            run_cosmos "$LABEL-profile" 1
        fi
        print_comparison
        ;;
esac

echo "[statx] completed: $OUT_DIR"
