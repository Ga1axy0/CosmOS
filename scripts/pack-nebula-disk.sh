#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROOT_IMAGE="${1:-$PROJECT_ROOT/disk-la-strict.img}"
MNT_IMAGE="${2:-$PROJECT_ROOT/sdcard-la.img}"
OUTPUT_IMAGE="${3:-$PROJECT_ROOT/disk-la-nebula.img}"

SECTOR_SIZE=512
ALIGN_SECTORS=2048
ROOT_START_SECTOR=$ALIGN_SECTORS

for tool in sfdisk dd truncate stat; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing required tool: $tool" >&2
        exit 1
    }
done

for image in "$ROOT_IMAGE" "$MNT_IMAGE"; do
    [ -f "$image" ] || {
        echo "filesystem image not found: $image" >&2
        exit 1
    }
done

if [ "$OUTPUT_IMAGE" = "$ROOT_IMAGE" ] || [ "$OUTPUT_IMAGE" = "$MNT_IMAGE" ]; then
    echo "output image must differ from both input images" >&2
    exit 1
fi

ceil_div() {
    echo $(( ($1 + $2 - 1) / $2 ))
}

align_up() {
    echo $(( (($1 + $2 - 1) / $2) * $2 ))
}

root_bytes="$(stat -c %s "$ROOT_IMAGE")"
mnt_bytes="$(stat -c %s "$MNT_IMAGE")"
root_data_sectors="$(ceil_div "$root_bytes" "$SECTOR_SIZE")"
mnt_data_sectors="$(ceil_div "$mnt_bytes" "$SECTOR_SIZE")"
root_partition_sectors="$(align_up "$root_data_sectors" "$ALIGN_SECTORS")"
mnt_partition_sectors="$(align_up "$mnt_data_sectors" "$ALIGN_SECTORS")"
mnt_start_sector=$((ROOT_START_SECTOR + root_partition_sectors))
disk_sectors=$((mnt_start_sector + mnt_partition_sectors + ALIGN_SECTORS))

truncate -s "$((disk_sectors * SECTOR_SIZE))" "$OUTPUT_IMAGE"
printf '%s\n' \
    'label: dos' \
    'unit: sectors' \
    "start=$ROOT_START_SECTOR, size=$root_partition_sectors, type=83" \
    "start=$mnt_start_sector, size=$mnt_partition_sectors, type=83" \
    | sfdisk "$OUTPUT_IMAGE"

dd if="$ROOT_IMAGE" of="$OUTPUT_IMAGE" bs=1M \
    seek="$((ROOT_START_SECTOR / ALIGN_SECTORS))" conv=notrunc,sparse status=progress
dd if="$MNT_IMAGE" of="$OUTPUT_IMAGE" bs=1M \
    seek="$((mnt_start_sector / ALIGN_SECTORS))" conv=notrunc,sparse status=progress

echo "packed Nebula SATA image: $OUTPUT_IMAGE"
echo "  partition 1: $ROOT_IMAGE -> /dev/vda1 -> /"
echo "  partition 2: $MNT_IMAGE -> /dev/vda2 -> /mnt"
echo "  virtual size: $((disk_sectors * SECTOR_SIZE / 1024 / 1024)) MiB"
