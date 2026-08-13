#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LABEL="${1:-run}"
SMP_COUNT="${SMP_COUNT:-8}"
RUN_TIMEOUT="${RUN_TIMEOUT:-180}"
PERF_PROBE_VALUE="${PERF_PROBE_VALUE:-1}"
SKIP_KERNEL_BUILD="${SKIP_KERNEL_BUILD:-0}"
OUT_DIR="${OUT_DIR:-$PROJECT_ROOT/.make/cargo-perf}"
LOG_PATH="$OUT_DIR/$LABEL.log"
BUILD_LOG_PATH="$OUT_DIR/$LABEL-build.log"
ROOTFS_RV="$PROJECT_ROOT/CosmOS-rootfs/rootfs-rv"
GUEST_RUNNER="${GUEST_RUNNER:-$SCRIPT_DIR/cargo-perf-guest.sh}"
GUEST_PAYLOAD="${GUEST_PAYLOAD:-}"
GUEST_PAYLOAD_DEST="${GUEST_PAYLOAD_DEST:-/root/guest-payload}"
GUEST_GROUPS="${GUEST_GROUPS:-}"
GUEST_CASES="${GUEST_CASES:-}"
LOG_PARSER="$SCRIPT_DIR/parse-cargo-perf-log.py"
OVERLAY_DIR=""
BENCH_ROOTFS=""
BOOTSTRAP_BASE=""
SDCARD_OVERLAY=""
BOOTSTRAP_OVERLAY=""

cleanup() {
    # Only remove the private directory shape created below.  The benchmark
    # rootfs and bootstrap image both live here, so the normal evaluator's
    # rootfs-rv and disk.img remain untouched.
    if [[ -n "$OVERLAY_DIR" && "$OVERLAY_DIR" == /tmp/cosmos-cargo-perf.* ]]; then
        rm -rf -- "$OVERLAY_DIR"
    fi
}
trap cleanup EXIT

mkdir -p "$OUT_DIR"

if [[ ! -f "$PROJECT_ROOT/sdcard-rv-pub.img" ]]; then
    echo "missing $PROJECT_ROOT/sdcard-rv-pub.img" >&2
    exit 2
fi
if [[ ! -x "$GUEST_RUNNER" ]]; then
    echo "guest runner is not executable: $GUEST_RUNNER" >&2
    exit 2
fi
if ! command -v qemu-img >/dev/null 2>&1; then
    echo "qemu-img is required to create disposable benchmark overlays" >&2
    exit 2
fi

OVERLAY_DIR="$(mktemp -d /tmp/cosmos-cargo-perf.XXXXXX)"
BENCH_ROOTFS="$OVERLAY_DIR/rootfs-rv"
BOOTSTRAP_BASE="$OVERLAY_DIR/bootstrap.img"
SDCARD_OVERLAY="$OVERLAY_DIR/sdcard.qcow2"
BOOTSTRAP_OVERLAY="$OVERLAY_DIR/bootstrap.qcow2"

if [[ "$SKIP_KERNEL_BUILD" == 1 ]]; then
    if [[ ! -f "$PROJECT_ROOT/kernel-rv" ]]; then
        echo "missing prebuilt $PROJECT_ROOT/kernel-rv" >&2
        exit 2
    fi
    echo "[cargo-perf-host] using prebuilt RISC-V kernel"
elif [[ "$PERF_PROBE_VALUE" == 1 ]]; then
    echo "[cargo-perf-host] building instrumented RISC-V kernel"
else
    echo "[cargo-perf-host] building production RISC-V kernel"
fi
if [[ "$SKIP_KERNEL_BUILD" != 1 ]] && ! make -C "$PROJECT_ROOT" kernel-rv PERF_PROBE="$PERF_PROBE_VALUE" >"$BUILD_LOG_PATH" 2>&1; then
    echo "[cargo-perf-host] kernel build failed: $BUILD_LOG_PATH" >&2
    tail -n 100 "$BUILD_LOG_PATH" >&2
    exit 1
fi
KERNEL_SHA256="$(sha256sum "$PROJECT_ROOT/kernel-rv" | awk '{print $1}')"
echo "[cargo-perf-host] kernel_sha256=$KERNEL_SHA256"

echo "[cargo-perf-host] cloning bootstrap rootfs for this run"
cp -a "$ROOTFS_RV" "$BENCH_ROOTFS"
echo "[cargo-perf-host] staging no-countdown runner in temporary rootfs"
install -m 0755 "$GUEST_RUNNER" "$BENCH_ROOTFS/root/final_auto_run"
install -m 0755 "$GUEST_RUNNER" "$BENCH_ROOTFS/root/final-auto-run"
if [[ -n "$GUEST_PAYLOAD" ]]; then
    if [[ ! -f "$GUEST_PAYLOAD" ]]; then
        echo "guest payload does not exist: $GUEST_PAYLOAD" >&2
        exit 2
    fi
    payload_path="$BENCH_ROOTFS$GUEST_PAYLOAD_DEST"
    mkdir -p "$(dirname "$payload_path")"
    install -m 0755 "$GUEST_PAYLOAD" "$payload_path"
    echo "[cargo-perf-host] staged guest payload at $GUEST_PAYLOAD_DEST"
fi
if [[ -n "$GUEST_GROUPS" ]]; then
    printf '%s\n' "$GUEST_GROUPS" > "$BENCH_ROOTFS/root/lmbench-groups"
    echo "[cargo-perf-host] staged guest groups=$GUEST_GROUPS"
fi
if [[ -n "$GUEST_CASES" ]]; then
    printf '%s\n' "$GUEST_CASES" > "$BENCH_ROOTFS/root/lmbench-cases"
    echo "[cargo-perf-host] staged guest cases=$GUEST_CASES"
fi

echo "[cargo-perf-host] repacking temporary bootstrap disk"
PACK_USER_APPS=0 LOOP_FAT32_ENABLE=0 EXTRA_MIB=16 MIN_SIZE_MIB=64 \
    "$PROJECT_ROOT/scripts/pack-disk-img.sh" \
    "$BENCH_ROOTFS" \
    "$PROJECT_ROOT/user/target/riscv64gc-unknown-none-elf/release" \
    "$BOOTSTRAP_BASE"

# Use explicit writable overlays instead of QEMU's implicit `-snapshot`
# temporary files.  This keeps both source images pristine and also works in
# restricted environments where QEMU cannot create files under /var/tmp.
qemu-img create -q -f qcow2 -F raw -b "$PROJECT_ROOT/sdcard-rv-pub.img" "$SDCARD_OVERLAY"
qemu-img create -q -f qcow2 -F raw -b "$BOOTSTRAP_BASE" "$BOOTSTRAP_OVERLAY"

sdcard_args="-drive file=$SDCARD_OVERLAY,if=none,format=qcow2,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0"
bootstrap_args="-drive file=$BOOTSTRAP_OVERLAY,if=none,format=qcow2,id=x1 -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1"

echo "[cargo-perf-host] running label=$LABEL smp=$SMP_COUNT perf_probe=$PERF_PROBE_VALUE timeout=${RUN_TIMEOUT}s"
run_prefix=()
if [[ -n "${QEMU_CPUSET:-}" ]]; then
    run_prefix=(taskset --cpu-list "$QEMU_CPUSET")
    echo "[cargo-perf-host] pinning QEMU process tree to cpus=$QEMU_CPUSET"
fi
set +e
# QEMU's stdio chardev needs to stay in the foreground process group.  Without
# --foreground, timeout can stop it on a terminal read before any guest output
# reaches the log.
make_args=(-C "$PROJECT_ROOT")
if [[ "$SKIP_KERNEL_BUILD" == 1 ]]; then
    # `fast-run` normally depends on `kernel-rv`.  Merely skipping the explicit
    # build above is insufficient when a source file is newer than a frozen
    # A/B kernel copied into place: make would silently rebuild it here.  Treat
    # this target as an old file so its recipe is suppressed for this run.
    make_args+=(-o kernel-rv)
fi
"${run_prefix[@]}" timeout --foreground "$RUN_TIMEOUT" \
    make "${make_args[@]}" fast-run FINAL=1 PERF_PROBE="$PERF_PROBE_VALUE" SMP="$SMP_COUNT" \
    FAST_RUN_MODE_ARGS= \
    FAST_RUN_QEMU_BLK_ARGS="$sdcard_args" \
    QEMU_COMP_EXTRA_BLK_ARGS="$bootstrap_args" \
    FAST_RUN_QEMU_NETDEV=user,id=net \
    >"$LOG_PATH" 2>&1
qemu_status=$?
set -e

if ! rg -q 'CARGO_PERF_DONE status=0' "$LOG_PATH"; then
    echo "[cargo-perf-host] benchmark did not complete successfully (qemu rc=$qemu_status)" >&2
    tail -n 100 "$LOG_PATH" >&2
    exit 1
fi

echo "[cargo-perf-host] completed (qemu rc=$qemu_status): $LOG_PATH"
if [[ -x "$LOG_PARSER" ]]; then
    if rg -q '\[cargo-perf\] cargo_(new|run_release)' "$LOG_PATH"; then
        "$LOG_PARSER" "$LOG_PATH"
    else
        rg '\[(process-perf|cargo-perf)\] (phase|cargo_|CARGO_|PROCESS_)' "$LOG_PATH" || true
    fi
else
    rg '\[cargo-perf\] cargo_(new|run_release)' "$LOG_PATH" || true
fi
