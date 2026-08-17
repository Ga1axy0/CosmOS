#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$PROJECT_ROOT/.." && pwd)"
ROOTFS_REPO="$PROJECT_ROOT/CosmOS-rootfs"
THIRD_PARTY="$ROOTFS_REPO/third-party"

STRICT_ROOT="${STRICT_ROOT:-$WORKSPACE_ROOT/loongarch64-strict-runtime}"
ROOTFS_OUT="${ROOTFS_OUT:-$ROOTFS_REPO/rootfs-la-strict}"
ROOTFS_BUILD="${ROOTFS_BUILD:-$ROOTFS_REPO/build/la-strict}"
ROOTFS_STAMPS="${ROOTFS_STAMPS:-$ROOTFS_REPO/build/.stamps-la-strict}"
DISK_OUT="${DISK_OUT:-$PROJECT_ROOT/disk-la-strict.img}"
COMPRESSED_OUT="${COMPRESSED_OUT:-$PROJECT_ROOT/rootfs-la-strict.img}"
JOBS="${JOBS:-$(nproc)}"

MUSL_STAGE="$STRICT_ROOT/musl-stage"
GLIBC_STAGE="$STRICT_ROOT/glibc-stage"
GCC_SOURCE="$STRICT_ROOT/gcc-13.2.0-src"
GCC_BUILD="$STRICT_ROOT/gcc-13.2.0-build"
GCC_STAGE="$STRICT_ROOT/gcc-13.2.0-stage"
MUSL_BUILD_SYSROOT="$STRICT_ROOT/musl-build-sysroot"

die() {
    echo "[ERROR] $*" >&2
    exit 1
}

require_file() {
    [ -s "$1" ] || die "missing required file: $1"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "missing command: $1"
}

if [ "$(id -u)" -ne 0 ]; then
    die "run this target as root inside the os-contest Docker container"
fi

for command_name in \
    cargo file gzip loongarch64-linux-gnu-gcc loongarch64-linux-musl-gcc \
    make mkfs.ext4 rustup tar
do
    require_command "$command_name"
done

# The base image exports an LD_LIBRARY_PATH ending in ':'.  glibc/GCC treat
# that as including the current directory and reject or contaminate builds.
unset LD_LIBRARY_PATH

MUSL_SYSROOT="$(readlink -f "$(loongarch64-linux-musl-gcc -print-sysroot)")"
GLIBC_SYSROOT="$(readlink -f "$(loongarch64-linux-gnu-gcc -print-sysroot)")"
MUSL_TOOLCHAIN_ROOT="$(cd "$(dirname "$(command -v loongarch64-linux-musl-gcc)")/.." && pwd)"
GLIBC_TOOLCHAIN_ROOT="$(cd "$(dirname "$(command -v loongarch64-linux-gnu-gcc)")/.." && pwd)"
GCC_STAGE_PREFIX="$GCC_STAGE$MUSL_TOOLCHAIN_ROOT"

require_file "$MUSL_STAGE/lib/libc.a"
require_file "$MUSL_STAGE/lib/libc.so"
require_file "$GLIBC_STAGE/usr/lib64/libc.a"
require_file "$GLIBC_STAGE/lib64/libc.so.6"
require_file "$GLIBC_STAGE/lib64/ld-linux-loongarch-lp64d.so.1"
require_file "$THIRD_PARTY/gcc-13.2.0.tar.xz"
require_file "$THIRD_PARTY/gmp-6.2.1.tar.bz2"
require_file "$THIRD_PARTY/mpfr-4.1.0.tar.bz2"
require_file "$THIRD_PARTY/mpc-1.2.1.tar.gz"

echo "[STRICT] project           : $PROJECT_ROOT"
echo "[STRICT] staging           : $STRICT_ROOT"
echo "[STRICT] musl sysroot      : $MUSL_SYSROOT"
echo "[STRICT] glibc sysroot     : $GLIBC_SYSROOT"
echo "[STRICT] rootfs output     : $ROOTFS_OUT"
echo "[STRICT] parallel jobs     : $JOBS"

echo "[STRICT] preparing an isolated musl build sysroot..."
rm -rf "$MUSL_BUILD_SYSROOT"
mkdir -p "$MUSL_BUILD_SYSROOT"
cp -a "$MUSL_SYSROOT/." "$MUSL_BUILD_SYSROOT/"
cp -a "$MUSL_STAGE/." "$MUSL_BUILD_SYSROOT/"

if [ -s "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libstdc++.a" ] \
    && [ -s "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libatomic.a" ] \
    && [ -s "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libgomp.a" ]; then
    echo "[STRICT] reusing completed strict-align GCC target runtimes..."
else
    echo "[STRICT] rebuilding GCC 13.2 target runtimes..."
    rm -rf "$GCC_SOURCE" "$GCC_BUILD" "$GCC_STAGE"
    mkdir -p "$GCC_SOURCE" "$GCC_BUILD" "$GCC_STAGE"

tar xf "$THIRD_PARTY/gcc-13.2.0.tar.xz" \
    -C "$GCC_SOURCE" --strip-components=1
mkdir -p "$GCC_SOURCE/gmp" "$GCC_SOURCE/mpfr" "$GCC_SOURCE/mpc"
tar xf "$THIRD_PARTY/gmp-6.2.1.tar.bz2" \
    -C "$GCC_SOURCE/gmp" --strip-components=1
tar xf "$THIRD_PARTY/mpfr-4.1.0.tar.bz2" \
    -C "$GCC_SOURCE/mpfr" --strip-components=1
tar xf "$THIRD_PARTY/mpc-1.2.1.tar.gz" \
    -C "$GCC_SOURCE/mpc" --strip-components=1

(
    cd "$GCC_BUILD"
    CFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    CXXFLAGS_FOR_TARGET="-O2 -mstrict-align" \
        "$GCC_SOURCE/configure" \
            --build="$(gcc -dumpmachine)" \
            --host="$(gcc -dumpmachine)" \
            --target=loongarch64-linux-musl \
            --prefix="$MUSL_TOOLCHAIN_ROOT" \
            --with-sysroot="$MUSL_SYSROOT" \
            --with-build-sysroot="$MUSL_BUILD_SYSROOT" \
            --with-native-system-header-dir=/include \
            --enable-languages=c,c++ \
            --disable-bootstrap \
            --disable-multilib \
            --disable-nls \
            --disable-werror \
            --disable-libsanitizer \
            --disable-libquadmath \
            --disable-libssp \
            --disable-libvtv \
            --disable-libitm \
            --without-isl
)

make -C "$GCC_BUILD" -j"$JOBS" \
    CFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    CXXFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    all-gcc
make -C "$GCC_BUILD" -j"$JOBS" \
    CFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    CXXFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    all-target-libgcc
make -C "$GCC_BUILD" -j"$JOBS" \
    CFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    CXXFLAGS_FOR_TARGET="-O2 -mstrict-align" \
    all-target-libstdc++-v3 \
    all-target-libatomic \
    all-target-libgomp
make -C "$GCC_BUILD" \
    DESTDIR="$GCC_STAGE" \
    install-target-libgcc \
    install-target-libstdc++-v3 \
    install-target-libatomic \
    install-target-libgomp
fi

require_file "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libstdc++.a"
require_file "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libatomic.a"
require_file "$GCC_STAGE_PREFIX/loongarch64-linux-musl/lib/libgomp.a"

echo "[STRICT] installing verified libc/runtime staging into container sysroots..."
cp -a "$MUSL_STAGE/." "$MUSL_SYSROOT/"
cp -a "$GLIBC_STAGE/usr/include/." "$GLIBC_SYSROOT/usr/include/"
cp -a "$GLIBC_STAGE/usr/lib64/." "$GLIBC_SYSROOT/usr/lib64/"
mkdir -p "$GLIBC_SYSROOT/lib64"
cp -a "$GLIBC_STAGE/lib64/." "$GLIBC_SYSROOT/lib64/"
cp -a "$GCC_STAGE_PREFIX/." "$MUSL_TOOLCHAIN_ROOT/"

TEST_SOURCE="$STRICT_ROOT/strict-runtime-test.c"
TEST_MUSL="$STRICT_ROOT/strict-runtime-test-musl"
TEST_GLIBC="$STRICT_ROOT/strict-runtime-test-glibc"
printf '%s\n' 'int main(void) { return 0; }' > "$TEST_SOURCE"
loongarch64-linux-musl-gcc -O2 -mstrict-align -static \
    "$TEST_SOURCE" -o "$TEST_MUSL"
loongarch64-linux-gnu-gcc -O2 -mstrict-align \
    "$TEST_SOURCE" -o "$TEST_GLIBC"
file "$TEST_MUSL" "$TEST_GLIBC"

echo "[STRICT] preparing the rootfs-la-strict tree..."
if [ "${STRICT_CLEAN:-0}" = "1" ]; then
    echo "[STRICT] STRICT_CLEAN=1; discarding the previous partial rootfs build..."
    rm -rf "$ROOTFS_OUT" "$ROOTFS_BUILD" "$ROOTFS_STAMPS"
fi
if [ ! -d "$ROOTFS_OUT" ]; then
    mkdir -p "$ROOTFS_OUT"
    cp -a "$ROOTFS_REPO/rootfs/." "$ROOTFS_OUT/"
    rm -rf "$ROOTFS_OUT/root/tgoskits" "$ROOTFS_OUT/root/.cargo"
else
    echo "[STRICT] resuming the existing rootfs-la-strict build..."
fi
mkdir -p "$ROOTFS_BUILD" "$ROOTFS_STAMPS"

export COMMON_CFLAGS="-Os -mstrict-align"
export COMMON_CXXFLAGS="-Os -mstrict-align"
export KCFLAGS="-mstrict-align"
export CFLAGS="-Os -mstrict-align"
export CXXFLAGS="-Os -mstrict-align"
export CROSS_PREFIX="loongarch64-linux-musl-"

# Each package script gets a fresh stamp directory and build directory, so no
# artifact from the ordinary rootfs-la build can be reused accidentally.
make -C "$ROOTFS_REPO" rootfs-init \
    ROOTFS_DIR="$ROOTFS_OUT" \
    BUILD_ROOT="$ROOTFS_BUILD" \
    STAMP_DIR="$ROOTFS_STAMPS" \
    TARGET=loongarch64-linux-musl \
    TOOLCHAIN_BIN="$MUSL_TOOLCHAIN_ROOT/bin" \
    BUSYBOX_ARCH=loongarch \
    GLIBC_TOOLCHAIN="$GLIBC_TOOLCHAIN_ROOT" \
    MUSL_LIB="$MUSL_SYSROOT/lib" \
    MUSL_ARCH=loongarch64 \
    WITH_RUST=0 \
    WITH_LIBCLANG=0 \
    WITH_BUILD_ESSENTIAL=1 \
    WITH_NATIVE_GCC=1 \
    WITH_GLIBC_HOST_SYSROOT=1 \
    MUSL_LOADER_ALIASES="ld-musl-loongarch64.so.1"

echo "[STRICT] rebuilding freestanding LoongArch user programs..."
RUST_TOOLCHAIN="$(cd "$PROJECT_ROOT/user" && rustup show active-toolchain)"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN%% *}"
if ! rustup component list --toolchain "$RUST_TOOLCHAIN" \
    | grep -q '^rust-src.*(installed)'; then
    echo "[STRICT] installing rust-src for $RUST_TOOLCHAIN..."
    rustup component add rust-src --toolchain "$RUST_TOOLCHAIN"
fi
(
    cd "$PROJECT_ROOT/user"
    CARGO_TARGET_LOONGARCH64_UNKNOWN_NONE_RUSTFLAGS="-Ctarget-feature=-ual" \
        cargo build \
            -Z build-std=core,alloc \
            --release \
            --target loongarch64-unknown-none
)

echo "[STRICT] packing the strict root filesystem image..."
MUSL_ARCH=loongarch64 \
MUSL_LOADER_ALIASES="ld-musl-loongarch64.so.1" \
    "$PROJECT_ROOT/scripts/pack-disk-img.sh" \
        "$ROOTFS_OUT" \
        "$PROJECT_ROOT/user/target/loongarch64-unknown-none/release" \
        "$DISK_OUT"
gzip -1 -c "$DISK_OUT" > "$COMPRESSED_OUT"

echo "[STRICT] build complete"
file "$ROOTFS_OUT/usr/bin/bash" "$ROOTFS_OUT/bin/busybox" \
    "$DISK_OUT" "$COMPRESSED_OUT"
sha256sum "$DISK_OUT" "$COMPRESSED_OUT"
echo "[STRICT] rootfs directory : $ROOTFS_OUT"
echo "[STRICT] raw ext4 image   : $DISK_OUT"
echo "[STRICT] gzip burn image  : $COMPRESSED_OUT"
