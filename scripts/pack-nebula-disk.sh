#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROOTFS_DIR="${1:-$PROJECT_ROOT/CosmOS-rootfs/rootfs-la-strict}"
TEST_IMAGE="${2:-$PROJECT_ROOT/sdcard-la.img}"
OUTPUT_IMAGE="${3:-$PROJECT_ROOT/disk-la-nebula.img}"
COMPRESSED_IMAGE="${4:-$PROJECT_ROOT/rootfs-la-nebula.img}"
USER_BIN_DIR="${USER_BIN_DIR:-$PROJECT_ROOT/user/target/loongarch64-unknown-none/release}"

for tool in debugfs mktemp cp mkdir rm stat gzip cmp e2fsck; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing required tool: $tool" >&2
        exit 1
    }
done

[ -d "$ROOTFS_DIR" ] || {
    echo "rootfs directory not found: $ROOTFS_DIR" >&2
    exit 1
}
[ -d "$ROOTFS_DIR/root" ] || {
    echo "rootfs must contain /root: $ROOTFS_DIR" >&2
    exit 1
}
[ -d "$USER_BIN_DIR" ] || {
    echo "LoongArch user binary directory not found: $USER_BIN_DIR" >&2
    exit 1
}
[ -f "$TEST_IMAGE" ] || {
    echo "test filesystem image not found: $TEST_IMAGE" >&2
    exit 1
}

for output in "$OUTPUT_IMAGE" "$COMPRESSED_IMAGE"; do
    if [ "$output" = "$TEST_IMAGE" ]; then
        echo "output image must differ from the test image: $output" >&2
        exit 1
    fi
done
if [ "$COMPRESSED_IMAGE" = "$OUTPUT_IMAGE" ]; then
    echo "raw and compressed output images must differ" >&2
    exit 1
fi

STAGE_DIR="$(mktemp -d /tmp/pack-nebula-rootfs.XXXXXX)"
VERIFY_DIR="$(mktemp -d /tmp/verify-nebula-rootfs.XXXXXX)"
cleanup() {
    rm -rf "$STAGE_DIR" "$VERIFY_DIR"
}
trap cleanup EXIT

# Keep the runtime rootfs unchanged and add the two test suites required on the
# board below /mnt.  The Nebula U-Boot rootfs updater cannot safely write raw
# filesystem images whose uncompressed size is 4 GiB or larger.
cp -a "$ROOTFS_DIR"/. "$STAGE_DIR"/
mkdir -p "$STAGE_DIR/mnt"

for test_dir in glibc musl; do
    extract_log="$VERIFY_DIR/extract-$test_dir.log"
    if ! debugfs -R "rdump /$test_dir $STAGE_DIR/mnt" "$TEST_IMAGE" \
        >"$extract_log" 2>&1; then
        echo "debugfs failed to extract /$test_dir from $TEST_IMAGE" >&2
        while IFS= read -r line; do
            echo "$line" >&2
        done < "$extract_log"
        exit 1
    fi
    [ -d "$STAGE_DIR/mnt/$test_dir" ] || {
        echo "failed to extract /$test_dir from $TEST_IMAGE" >&2
        exit 1
    }
done

for test_script in \
    mnt/glibc/cagent_testcode.sh \
    mnt/glibc/buildstorm_testcode.sh \
    mnt/musl/cagent.sh; do
    [ -x "$STAGE_DIR/$test_script" ] || {
        echo "missing or non-executable test script: /$test_script" >&2
        exit 1
    }
done

MUSL_ARCH=loongarch64 \
MUSL_LOADER_ALIASES="ld-musl-loongarch64.so.1" \
    "$PROJECT_ROOT/scripts/pack-disk-img.sh" \
        "$STAGE_DIR" \
        "$USER_BIN_DIR" \
        "$OUTPUT_IMAGE"

# U-Boot's gzip metadata path only carries a 32-bit uncompressed size.  Its
# Update rootfs menu does not provide gzwrite's explicit outsize argument, so a
# larger image would be silently truncated modulo 4 GiB on the SSD.
raw_bytes="$(stat -c %s "$OUTPUT_IMAGE")"
if [ "$raw_bytes" -ge 4294967296 ]; then
    echo "raw rootfs is too large for Nebula Update rootfs: $raw_bytes bytes" >&2
    echo "the uncompressed image must be smaller than 4 GiB" >&2
    exit 1
fi

# Check that the generated filesystem really contains executable scripts at
# the paths used on the board. Compare their bytes with the staged sources so
# an incomplete extraction or pack cannot be reported as successful.
for test_script in \
    mnt/glibc/cagent_testcode.sh \
    mnt/glibc/buildstorm_testcode.sh \
    mnt/musl/cagent.sh; do
    verify_path="$VERIFY_DIR/$(basename "$test_script")"
    debugfs -R "dump -p /$test_script $verify_path" "$OUTPUT_IMAGE"
    cmp "$STAGE_DIR/$test_script" "$verify_path"
    [ -x "$verify_path" ] || {
        echo "packed test script is not executable: /$test_script" >&2
        exit 1
    }
done

e2fsck -fn "$OUTPUT_IMAGE"

# Boot Menu's rootfs update consumes a gzip stream even though the artifact
# conventionally has an .img suffix.
gzip -1 -c "$OUTPUT_IMAGE" > "$COMPRESSED_IMAGE"
gzip -dc "$COMPRESSED_IMAGE" | cmp - "$OUTPUT_IMAGE"

echo "packed Nebula rootfs image: $OUTPUT_IMAGE"
echo "compressed burn image: $COMPRESSED_IMAGE"
echo "  root filesystem: $ROOTFS_DIR -> /"
echo "  test payload: $TEST_IMAGE:/glibc -> /mnt/glibc"
echo "  test payload: $TEST_IMAGE:/musl  -> /mnt/musl"
echo "  verification: ext4, test scripts, permissions, and gzip stream are valid"
