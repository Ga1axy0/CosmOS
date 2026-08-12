#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LABEL="${1:-linux-run}"
SMP_COUNT="${SMP_COUNT:-8}"
RUN_TIMEOUT="${RUN_TIMEOUT:-90}"
BOOT_DELAY="${BOOT_DELAY:-3}"
OUT_DIR="${OUT_DIR:-$PROJECT_ROOT/.make/cargo-perf}"
LOG_PATH="$OUT_DIR/$LABEL.log"
LINUX_IMAGE="${LINUX_IMAGE:-$PROJECT_ROOT/.make/linux-rv/arch/riscv/boot/Image}"
PUBLIC_IMAGE="${PUBLIC_IMAGE:-$PROJECT_ROOT/sdcard-rv-pub.img}"
GUEST_RUNNER="${GUEST_RUNNER:-$SCRIPT_DIR/cargo-perf-guest.sh}"
OVERLAY_DIR=""
SDCARD_OVERLAY=""
INPUT_FIFO=""

cleanup() {
    if [[ -n "$OVERLAY_DIR" && "$OVERLAY_DIR" == /tmp/linux-cargo-perf.* ]]; then
        rm -rf -- "$OVERLAY_DIR"
    fi
}
trap cleanup EXIT

mkdir -p "$OUT_DIR"

if [[ ! -f "$LINUX_IMAGE" ]]; then
    echo "missing Linux image: $LINUX_IMAGE" >&2
    exit 2
fi
if [[ ! -f "$PUBLIC_IMAGE" ]]; then
    echo "missing public rootfs image: $PUBLIC_IMAGE" >&2
    exit 2
fi
if ! command -v qemu-img >/dev/null 2>&1; then
    echo "qemu-img is required to create a disposable benchmark overlay" >&2
    exit 2
fi
case "$GUEST_RUNNER" in
    "$PROJECT_ROOT"/*) guest_runner_relative="${GUEST_RUNNER#"$PROJECT_ROOT"/}" ;;
    *) echo "guest runner must be inside $PROJECT_ROOT: $GUEST_RUNNER" >&2; exit 2 ;;
esac

OVERLAY_DIR="$(mktemp -d /tmp/linux-cargo-perf.XXXXXX)"
SDCARD_OVERLAY="$OVERLAY_DIR/sdcard.qcow2"
INPUT_FIFO="$OVERLAY_DIR/qemu-input"
qemu-img create -q -f qcow2 -F raw -b "$PUBLIC_IMAGE" "$SDCARD_OVERLAY"
mkfifo "$INPUT_FIFO"

# Open the FIFO read/write before starting QEMU so neither endpoint blocks
# while the other one is being set up.
exec 3<>"$INPUT_FIFO"

echo "[linux-cargo-perf-host] running label=$LABEL smp=$SMP_COUNT timeout=${RUN_TIMEOUT}s"
run_prefix=()
if [[ -n "${QEMU_CPUSET:-}" ]]; then
    run_prefix=(taskset --cpu-list "$QEMU_CPUSET")
    echo "[linux-cargo-perf-host] pinning QEMU process tree to cpus=$QEMU_CPUSET"
fi
set +e
"${run_prefix[@]}" timeout --foreground "$RUN_TIMEOUT" \
    qemu-system-riscv64 \
    -machine virt \
    -kernel "$LINUX_IMAGE" \
    -m 4G \
    -nographic \
    -smp "$SMP_COUNT" \
    -bios default \
    -drive "file=$SDCARD_OVERLAY,if=none,format=qcow2,id=x0" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -fsdev "local,id=host0,path=$PROJECT_ROOT,security_model=none,readonly=on" \
    -device virtio-9p-device,fsdev=host0,mount_tag=host0 \
    -append 'console=ttyS0 root=/dev/vda rw rootwait init=/bin/sh' \
    -no-reboot \
    -rtc base=utc \
    <&3 >"$LOG_PATH" 2>&1 &
qemu_pid=$!

sleep "$BOOT_DELAY"
printf '%s\n' \
    'mount -t proc proc /proc' \
    'mkdir -p /host' \
    'mount -t 9p -o trans=virtio,version=9p2000.L host0 /host' \
    "LMBENCH_GROUPS=${LMBENCH_GROUPS:-all} LMBENCH_CASES=${LMBENCH_CASES:-} LMBENCH_CASE_TIMEOUT=${LMBENCH_CASE_TIMEOUT:-45} /bin/sh /host/$guest_runner_relative; /bin/busybox poweroff -f" \
    >&3

wait "$qemu_pid"
qemu_status=$?
set -e
exec 3>&-

if ! rg -q 'CARGO_PERF_DONE status=0' "$LOG_PATH"; then
    echo "[linux-cargo-perf-host] benchmark did not complete successfully (qemu rc=$qemu_status)" >&2
    tail -n 100 "$LOG_PATH" >&2
    exit 1
fi

echo "[linux-cargo-perf-host] completed (qemu rc=$qemu_status): $LOG_PATH"
if rg -q '\[cargo-perf\] cargo_(new|run_release)' "$LOG_PATH"; then
    "$SCRIPT_DIR/parse-cargo-perf-log.py" "$LOG_PATH"
else
    rg '\[(process-perf|cargo-perf)\] (phase|cargo_|CARGO_|PROCESS_)' "$LOG_PATH" || true
fi
