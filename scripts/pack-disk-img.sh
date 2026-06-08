#!/usr/bin/env bash
set -euo pipefail

ROOTFS_DIR="${1:-rootfs}"
USER_BIN_DIR="${2:-user/target/riscv64gc-unknown-none-elf/release}"
OUT_IMG="${3:-disk.img}"
EXTRA_MIB="${EXTRA_MIB:-512}"
MIN_SIZE_MIB="${MIN_SIZE_MIB:-1024}"
LABEL="${LABEL:-COSMOSDISK}"

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required tool: $1" >&2
        exit 1
    fi
}

require_tool mkfs.ext4
require_tool du
require_tool awk
require_tool truncate
require_tool mktemp
require_tool cp
require_tool rm

if [ ! -d "$ROOTFS_DIR" ]; then
    echo "rootfs directory not found: $ROOTFS_DIR" >&2
    exit 1
fi

if [ ! -d "$USER_BIN_DIR" ]; then
    echo "user binary directory not found: $USER_BIN_DIR" >&2
    exit 1
fi

STAGE_DIR="$(mktemp -d /tmp/pack-disk-img.XXXXXX)"
cleanup() {
    rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

cp -a "$ROOTFS_DIR"/. "$STAGE_DIR"/

if [ ! -d "$STAGE_DIR/root" ]; then
    echo "rootfs must contain /root before packing" >&2
    exit 1
fi

for host_path in "$USER_BIN_DIR"/*; do
    [ -f "$host_path" ] || continue
    [ -x "$host_path" ] || continue

    name="$(basename "$host_path")"
    case "$name" in
        *.bin|*.elf)
            continue
            ;;
    esac

    cp -f "$host_path" "$STAGE_DIR/root/$name"
done

if [ -f lib/musl/ar ] && [ -d "$STAGE_DIR/musl/lib" ]; then
    cp -f lib/musl/ar "$STAGE_DIR/musl/lib/ar"
fi

if [ -f lib/glibc/ar ] && [ -d "$STAGE_DIR/glibc/lib" ]; then
    cp -f lib/glibc/ar "$STAGE_DIR/glibc/lib/ar"
fi

rootfs_mib="$(du -sm "$STAGE_DIR" | awk '{print $1}')"
size_mib=$((rootfs_mib + EXTRA_MIB))
if [ "$size_mib" -lt "$MIN_SIZE_MIB" ]; then
    size_mib="$MIN_SIZE_MIB"
fi

rm -f "$OUT_IMG"
truncate -s "${size_mib}M" "$OUT_IMG"
mkfs.ext4 -q -F -d "$STAGE_DIR" -L "$LABEL" "$OUT_IMG"

echo "packed $OUT_IMG from staged $ROOTFS_DIR with user binaries in /root (${size_mib} MiB)"
