#!/usr/bin/env bash

set -euo pipefail

# Build and run the same RISC-V static workload in Linux and CosmOS QEMU,
# while also providing a quick native-Linux/strace baseline. The QEMU paths
# reuse the repository's disposable-overlay runners, so source images are not
# modified by this test.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE="$SCRIPT_DIR/mprotect_futex_workload.c"
WORK_DIR="${WORK_DIR:-$PROJECT_ROOT/.make/mprotect-futex}"
OUT_DIR="${OUT_DIR:-$WORK_DIR/runs}"
LINUX_CC="${LINUX_CC:-cc}"
RV_CC="${RV_CC:-/opt/riscv64-linux-musl-cross/bin/riscv64-linux-musl-gcc}"
LINUX_IMAGE="${LINUX_IMAGE:-$PROJECT_ROOT/.make/linux-rv/arch/riscv/boot/Image}"
PUBLIC_IMAGE="${PUBLIC_IMAGE:-$PROJECT_ROOT/sdcard-rv-pub.img}"
WORKERS="${WORKERS:-4}"
ROUNDS="${ROUNDS:-2048}"
PAGES="${PAGES:-32}"
SMP_COUNT="${SMP_COUNT:-4}"
RUN_TIMEOUT="${RUN_TIMEOUT:-180}"
PERF_PROBE_VALUE="${PERF_PROBE_VALUE:-0}"
SKIP_KERNEL_BUILD="${SKIP_KERNEL_BUILD:-0}"
MODE=all

usage() {
    cat <<EOF
Usage: ${0##*/} [options]

Build and run a repeated mprotect/futex workload.

Modes:
  --host-only       Run native Linux with strace (no QEMU prerequisites).
  --qemu-only       Run Linux and CosmOS RISC-V guests and compare them.
  --cosmos-only     Run only the CosmOS RISC-V guest.

Workload options:
  --workers N       Worker threads (default: $WORKERS)
  --rounds N        Permission/synchronization rounds (default: $ROUNDS)
  --pages N         Pages per worker mapping (default: $PAGES)

Environment overrides:
  RV_CC, LINUX_IMAGE, PUBLIC_IMAGE, SMP_COUNT, RUN_TIMEOUT,
  SKIP_KERNEL_BUILD=1, OUT_DIR, WORK_DIR

Examples:
  ${0##*/} --host-only
  ${0##*/} --qemu-only --workers 4 --rounds 4096
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host-only)
            MODE=host
            shift
            ;;
        --qemu-only)
            MODE=qemu
            shift
            ;;
        --cosmos-only)
            MODE=cosmos
            shift
            ;;
        --workers|--rounds|--pages)
            [[ $# -ge 2 ]] || { echo "missing value for $1" >&2; exit 2; }
            case "$1" in
                --workers) WORKERS=$2 ;;
                --rounds) ROUNDS=$2 ;;
                --pages) PAGES=$2 ;;
            esac
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

mkdir -p "$WORK_DIR" "$OUT_DIR"
RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
RUN_DIR="$OUT_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"
ARGS_FILE="$WORK_DIR/guest-payload-args"
printf '%s\n' --workers "$WORKERS" --rounds "$ROUNDS" --pages "$PAGES" >"$ARGS_FILE"

compile_host() {
    local output="$WORK_DIR/workload-linux"
    echo "[mprotect-futex] compiling native Linux workload: $output"
    "$LINUX_CC" -std=c11 -O2 -pthread -Wall -Wextra -Werror \
        "$SOURCE" -o "$output"
    echo "$output"
}

compile_riscv() {
    local output="$WORK_DIR/workload-rv"
    if [[ ! -x "$RV_CC" ]]; then
        echo "RISC-V compiler not found: $RV_CC" >&2
        echo "Set RV_CC to a static musl toolchain or use --host-only." >&2
        exit 2
    fi
    echo "[mprotect-futex] compiling static RISC-V workload: $output"
    "$RV_CC" -std=c11 -O2 -static -pthread -Wall -Wextra -Werror \
        "$SOURCE" -o "$output"
    echo "$output"
}

run_host() {
    local binary="$1"
    local output="$RUN_DIR/linux-host.out"
    local trace="$RUN_DIR/linux-host.strace"

    echo "[mprotect-futex] running native Linux baseline"
    if command -v strace >/dev/null 2>&1; then
        if strace -f -qq -c -e trace=mprotect,futex -o "$trace" \
            "$binary" --workers "$WORKERS" --rounds "$ROUNDS" --pages "$PAGES" \
            >"$output" && rg -q 'MPF_RESULT ' "$output"; then
            echo "[mprotect-futex] native strace summary: $trace"
            sed -n '/% time/,$p' "$trace" || true
        else
            echo "[mprotect-futex] strace could not attach; rerunning without strace" >&2
            "$binary" --workers "$WORKERS" --rounds "$ROUNDS" --pages "$PAGES" \
                >"$output"
        fi
    else
        "$binary" --workers "$WORKERS" --rounds "$ROUNDS" --pages "$PAGES" \
            >"$output"
        echo "[mprotect-futex] strace not found; native call counts come from the workload"
    fi
    cat "$output"
}

run_linux_guest() {
    if [[ ! -f "$LINUX_IMAGE" ]]; then
        echo "missing Linux RISC-V image: $LINUX_IMAGE" >&2
        return 2
    fi
    if [[ ! -f "$PUBLIC_IMAGE" ]]; then
        echo "missing public rootfs image: $PUBLIC_IMAGE" >&2
        return 2
    fi

    echo "[mprotect-futex] running Linux RISC-V guest"
    OUT_DIR="$RUN_DIR/linux-guest" \
    LINUX_IMAGE="$LINUX_IMAGE" PUBLIC_IMAGE="$PUBLIC_IMAGE" \
    GUEST_RUNNER="$SCRIPT_DIR/mprotect-futex-linux-guest.sh" \
    SMP_COUNT="$SMP_COUNT" RUN_TIMEOUT="$RUN_TIMEOUT" \
        "$PROJECT_ROOT/scripts/run-linux-cargo-perf-bench.sh" mprotect-futex-linux
}

run_cosmos_guest() {
    if [[ ! -f "$PUBLIC_IMAGE" ]]; then
        echo "missing public rootfs image: $PUBLIC_IMAGE" >&2
        return 2
    fi
    if [[ ! -d "$PROJECT_ROOT/CosmOS-rootfs/rootfs-rv" ]]; then
        echo "missing CosmOS bootstrap rootfs: $PROJECT_ROOT/CosmOS-rootfs/rootfs-rv" >&2
        return 2
    fi

    echo "[mprotect-futex] running CosmOS RISC-V guest"
    OUT_DIR="$RUN_DIR/cosmos" \
    GUEST_RUNNER="$SCRIPT_DIR/mprotect-futex-cosmos-guest.sh" \
    GUEST_PAYLOAD="$WORK_DIR/workload-rv" \
    GUEST_PAYLOAD_DEST=/root/mprotect-futex-workload \
    GUEST_PAYLOAD_ARGS="$(cat "$ARGS_FILE")" \
    PUBLIC_IMAGE="$PUBLIC_IMAGE" SMP_COUNT="$SMP_COUNT" \
    RUN_TIMEOUT="$RUN_TIMEOUT" PERF_PROBE_VALUE="$PERF_PROBE_VALUE" \
    SKIP_KERNEL_BUILD="$SKIP_KERNEL_BUILD" SYSCALLS_COUNT=1 \
        "$PROJECT_ROOT/scripts/run-cargo-perf-bench.sh" mprotect-futex-cosmos
}

metric() {
    local file="$1"
    local key="$2"
    awk -v key="$key" '
        /MPF_RESULT / {
            for (i = 1; i <= NF; ++i) {
                split($i, pair, "=")
                if (pair[1] == key) {
                    print pair[2]
                    exit
                }
            }
        }
    ' "$file"
}

proc_timing_metric() {
    local file="$1"
    local name="$2"
    local column="$3"
    awk -v name="$name" -v column="$column" '$2 == name { print $column; exit }' "$file"
}

print_comparison() {
    local linux_file="$1"
    local cosmos_file="$2"
    local linux_elapsed cosmos_elapsed linux_mprotect cosmos_mprotect

    linux_elapsed=$(metric "$linux_file" elapsed_ns)
    cosmos_elapsed=$(metric "$cosmos_file" elapsed_ns)
    linux_mprotect=$(metric "$linux_file" mprotect_calls)
    cosmos_mprotect=$(metric "$cosmos_file" mprotect_calls)

    if [[ -z "$linux_elapsed" || -z "$cosmos_elapsed" ]]; then
        echo "cannot find MPF_RESULT in guest logs" >&2
        return 1
    fi

    echo
    echo "==== mprotect/futex comparison (same static RISC-V binary) ===="
    printf '%-14s %16s %16s %16s %16s\n' \
        backend elapsed_ns mprotect_calls futex_wait_calls futex_wake_calls
    printf '%-14s %16s %16s %16s %16s\n' \
        linux-guest "$linux_elapsed" "$linux_mprotect" \
        "$(metric "$linux_file" futex_wait_calls)" \
        "$(metric "$linux_file" futex_wake_calls)"
    printf '%-14s %16s %16s %16s %16s\n' \
        cosmos "$cosmos_elapsed" "$cosmos_mprotect" \
        "$(metric "$cosmos_file" futex_wait_calls)" \
        "$(metric "$cosmos_file" futex_wake_calls)"

    awk -v linux="$linux_elapsed" -v cosmos="$cosmos_elapsed" '
        BEGIN {
            if (linux > 0) {
                printf "elapsed ratio Cosmos/Linux: %.3fx\n", cosmos / linux
            }
        }
    '

    echo
    echo "CosmOS kernel counter view (requires SYSCALLS_COUNT=1):"
    printf '  mprotect: calls=%s total_ns=%s avg_ns=%s\n' \
        "$(proc_timing_metric "$cosmos_file" mprotect 3)" \
        "$(proc_timing_metric "$cosmos_file" mprotect 4)" \
        "$(proc_timing_metric "$cosmos_file" mprotect 5)"
    printf '  futex:    calls=%s total_ns=%s avg_ns=%s\n' \
        "$(proc_timing_metric "$cosmos_file" futex 3)" \
        "$(proc_timing_metric "$cosmos_file" futex 4)" \
        "$(proc_timing_metric "$cosmos_file" futex 5)"
}

host_binary=""
if [[ "$MODE" == host || "$MODE" == all ]]; then
    host_binary=$(compile_host | tail -1)
    run_host "$host_binary"
fi

if [[ "$MODE" == host ]]; then
    echo "[mprotect-futex] logs: $RUN_DIR"
    exit 0
fi

if [[ "$MODE" == qemu || "$MODE" == cosmos || "$MODE" == all ]]; then
    compile_riscv >/dev/null
fi

if [[ "$MODE" == cosmos ]]; then
    run_cosmos_guest
    cosmos_log="$RUN_DIR/cosmos/mprotect-futex-cosmos.log"
    echo "[mprotect-futex] CosmOS log: $cosmos_log"
    rg 'MPF_RESULT|COSMOS_SYSCALL_COUNTS|^[0-9]+ (mprotect|futex) ' "$cosmos_log" || true
    exit 0
fi

if [[ "$MODE" == qemu || "$MODE" == all ]]; then
    run_linux_guest
    run_cosmos_guest
    linux_log="$RUN_DIR/linux-guest/mprotect-futex-linux.log"
    cosmos_log="$RUN_DIR/cosmos/mprotect-futex-cosmos.log"
    print_comparison "$linux_log" "$cosmos_log"
fi

echo "[mprotect-futex] logs: $RUN_DIR"
