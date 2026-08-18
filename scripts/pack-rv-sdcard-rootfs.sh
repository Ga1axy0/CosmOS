#!/usr/bin/env bash
# Pack the existing RISC-V rootfs together with an expanded sdcard image at
# /mnt.  The source rootfs is never changed; all work happens in a temporary
# staging directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOTFS_DIR="${1:-$PROJECT_ROOT/CosmOS-rootfs/rootfs-rv}"
SDCARD_IMAGE="${2:-$PROJECT_ROOT/sdcard-rv.img}"
OUTPUT_IMAGE="${3:-$PROJECT_ROOT/disk-rv-with-sdcard.img}"
USER_BIN_DIR="${4:-$PROJECT_ROOT/user/target/riscv64gc-unknown-none-elf/release}"
PACK_USER_APPS="${PACK_USER_APPS:-0}"
LOOP_FAT32_ENABLE="${LOOP_FAT32_ENABLE:-0}"
MUSL_ARCH="${MUSL_ARCH:-riscv64}"
TARGET_DESCRIPTION="${TARGET_DESCRIPTION:-RISC-V}"
EXTRA_MIB="${EXTRA_MIB:-16}"
MIN_SIZE_MIB="${MIN_SIZE_MIB:-64}"

usage() {
    cat <<EOF
Usage: $(basename "$0") [ROOTFS_DIR [SDCARD_IMAGE [OUTPUT_IMAGE [USER_BIN_DIR]]]]

Create an ext4 image from ROOTFS_DIR after expanding SDCARD_IMAGE below /mnt.
Defaults:
  ROOTFS_DIR     $PROJECT_ROOT/CosmOS-rootfs/rootfs-rv
  SDCARD_IMAGE   $PROJECT_ROOT/sdcard-rv.img
  OUTPUT_IMAGE   $PROJECT_ROOT/disk-rv-with-sdcard.img

The source rootfs and sdcard image are read only.  Set PACK_USER_APPS=1 to
also copy compiled user applications via pack-disk-img.sh.  Set
LOOP_FAT32_ENABLE=1 to add the optional loop-mount test image.
EOF
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required tool: $1" >&2
        exit 1
    }
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi

for tool in debugfs mktemp cp mkdir rm; do
    require_tool "$tool"
done

[ -d "$ROOTFS_DIR" ] || {
    echo "rootfs directory not found: $ROOTFS_DIR" >&2
    exit 1
}
[ -f "$SDCARD_IMAGE" ] || {
    echo "sdcard image not found: $SDCARD_IMAGE" >&2
    exit 1
}
[ -x "$SCRIPT_DIR/pack-disk-img.sh" ] || {
    echo "pack script is not executable: $SCRIPT_DIR/pack-disk-img.sh" >&2
    exit 1
}

input_image="$(realpath -e "$SDCARD_IMAGE")"
output_image="$(realpath -m "$OUTPUT_IMAGE")"
if [ "$input_image" = "$output_image" ]; then
    echo "output image must differ from the sdcard image: $OUTPUT_IMAGE" >&2
    exit 1
fi

STAGE_DIR="$(mktemp -d /tmp/pack-rv-sdcard-rootfs.XXXXXX)"
cleanup() {
    rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

cp -a "$ROOTFS_DIR"/. "$STAGE_DIR"/
# Replace any pre-existing staging /mnt so the packed payload is exactly the
# expanded sdcard filesystem rather than a merge with stale rootfs files.
rm -rf "$STAGE_DIR/mnt"
mkdir -p "$STAGE_DIR/mnt"

echo "extracting $SDCARD_IMAGE:/ -> /mnt"
# rdump restores source ownership as well as file contents and modes.  It is
# normal for an unprivileged packer to be unable to chown files to the UID/GID
# stored in the evaluation image; the dump itself still succeeds.  Keep real
# debugfs diagnostics visible, while omitting only those expected chown lines.
EXTRACT_LOG="$STAGE_DIR/rdump.log"
if ! debugfs -R "rdump / $STAGE_DIR/mnt" "$SDCARD_IMAGE" >"$EXTRACT_LOG" 2>&1; then
    cat "$EXTRACT_LOG" >&2
    exit 1
fi
grep -Ev \
    -e '^debugfs [0-9]' \
    -e '^(dump_file|rdump): (Operation not permitted|Invalid argument) while changing ownership of ' \
    "$EXTRACT_LOG" >&2 || true
rm -f "$EXTRACT_LOG"

for required_path in root usr; do
    [ -e "$STAGE_DIR/mnt/$required_path" ] || {
        echo "sdcard extraction is incomplete: missing /mnt/$required_path" >&2
        exit 1
    }
done

PACK_USER_APPS="$PACK_USER_APPS" \
LOOP_FAT32_ENABLE="$LOOP_FAT32_ENABLE" \
EXTRA_MIB="$EXTRA_MIB" \
MIN_SIZE_MIB="$MIN_SIZE_MIB" \
MUSL_ARCH="$MUSL_ARCH" \
    "$SCRIPT_DIR/pack-disk-img.sh" \
        "$STAGE_DIR" \
        "$USER_BIN_DIR" \
        "$OUTPUT_IMAGE"

echo "packed $TARGET_DESCRIPTION rootfs image: $OUTPUT_IMAGE"
echo "  root filesystem: $ROOTFS_DIR -> /"
echo "  sdcard payload: $SDCARD_IMAGE:/ -> /mnt"
