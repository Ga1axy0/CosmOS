#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOTFS="${ROOTFS:-$PROJECT_ROOT/CosmOS-rootfs/rootfs-rv}"
SOURCE="${SOURCE:-$PROJECT_ROOT/benchmarks/llama2c/run.c}"
OUT="${OUT:-$PROJECT_ROOT/benchmarks/llama2c/build/llama2c-bais-rv}"
QEMU_USER="${QEMU_USER:-qemu-riscv64}"

for tool in "$QEMU_USER" find sort head tail mkdir readelf; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done

if [ ! -f "$SOURCE" ] || [ ! -d "$ROOTFS" ]; then
    echo "missing source or RISC-V musl rootfs" >&2
    exit 1
fi

CC1="$(find "$ROOTFS/usr/libexec/gcc/riscv64-linux-musl" -type f -name cc1 | sort | head -n 1)"
GCC_LIB_DIR="$(find "$ROOTFS/usr/lib/gcc/riscv64-linux-musl" -type f -name crtbegin.o -printf '%h\n' | sort | tail -n 1)"
AS="$ROOTFS/usr/riscv64-linux-musl/bin/as"
LD="$ROOTFS/usr/riscv64-linux-musl/bin/ld"

if [ -z "$CC1" ] || [ -z "$GCC_LIB_DIR" ] || [ ! -x "$AS" ] || [ ! -x "$LD" ]; then
    echo "incomplete self-hosted RISC-V musl toolchain under $ROOTFS" >&2
    exit 1
fi

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"
ASM="$OUT_DIR/run.s"
OBJ="$OUT_DIR/run.o"

"$QEMU_USER" -L "$ROOTFS" "$CC1" \
    -quiet \
    -isystem "$ROOTFS/usr/include" \
    -isystem "$GCC_LIB_DIR/include" \
    -D_REENTRANT -D_GNU_SOURCE \
    "$SOURCE" \
    -march=rv64imafdc -mabi=lp64d -O3 -fopenmp \
    -o "$ASM"

"$QEMU_USER" -L "$ROOTFS" "$AS" \
    --traditional-format -march=rv64imafdc -mabi=lp64d \
    -o "$OBJ" "$ASM"

"$QEMU_USER" -L "$ROOTFS" "$LD" \
    --sysroot="$ROOTFS" --eh-frame-hdr -melf64lriscv \
    -dynamic-linker /lib/ld-musl-riscv64.so.1 \
    -o "$OUT" \
    "$ROOTFS/lib/crt1.o" \
    "$ROOTFS/lib/crti.o" \
    "$GCC_LIB_DIR/crtbegin.o" \
    -L"$GCC_LIB_DIR" \
    -L"$ROOTFS/usr/riscv64-linux-musl/lib" \
    -L"$ROOTFS/usr/lib" \
    -L"$ROOTFS/lib" \
    "$OBJ" \
    -lgomp -lm -lgcc \
    --push-state --as-needed -lgcc_s --pop-state \
    -lc -lgcc \
    --push-state --as-needed -lgcc_s --pop-state \
    "$GCC_LIB_DIR/crtend.o" \
    "$ROOTFS/lib/crtn.o"

chmod +x "$OUT"
readelf -h "$OUT" | grep -E 'Class:|Machine:'
readelf -d "$OUT" | grep NEEDED
echo "built $OUT"
