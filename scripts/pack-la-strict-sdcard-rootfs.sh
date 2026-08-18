#!/usr/bin/env bash
# Pack rootfs-la-strict with an expanded LoongArch evaluation sdcard at /mnt.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOTFS_DIR="${1:-$PROJECT_ROOT/CosmOS-rootfs/rootfs-la-strict}"
SDCARD_IMAGE="${2:-$PROJECT_ROOT/sdcard-la.img}"
OUTPUT_IMAGE="${3:-$PROJECT_ROOT/disk-la-strict-with-sdcard.img}"
USER_BIN_DIR="${4:-$PROJECT_ROOT/user/target/loongarch64-unknown-none/release}"

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    cat <<EOF
Usage: $(basename "$0") [ROOTFS_DIR [SDCARD_IMAGE [OUTPUT_IMAGE [USER_BIN_DIR]]]]

Expand SDCARD_IMAGE below /mnt in a temporary copy of ROOTFS_DIR, then pack
the combined LoongArch strict rootfs as an ext4 image.
Defaults:
  ROOTFS_DIR     $PROJECT_ROOT/CosmOS-rootfs/rootfs-la-strict
  SDCARD_IMAGE   $PROJECT_ROOT/sdcard-la.img
  OUTPUT_IMAGE   $PROJECT_ROOT/disk-la-strict-with-sdcard.img
EOF
    exit 0
fi

MUSL_ARCH=loongarch64 \
TARGET_DESCRIPTION='strict LoongArch' \
    exec "$SCRIPT_DIR/pack-rv-sdcard-rootfs.sh" \
        "$ROOTFS_DIR" "$SDCARD_IMAGE" "$OUTPUT_IMAGE" "$USER_BIN_DIR"
