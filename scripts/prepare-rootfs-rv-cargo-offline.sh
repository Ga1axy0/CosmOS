#!/usr/bin/env bash
set -euo pipefail

# Prepare Cargo's dependency cache for the RISC-V guest rootfs.
#
# This script runs on the host while rootfs-rv is being assembled.  Cargo
# registry/git contents are architecture-independent and can be downloaded on
# the host, but compiled Cargo binaries must not be copied: they would have the
# host architecture and cannot run inside the RISC-V guest.

ROOTFS_DIR="${ROOTFS_DIR:?ROOTFS_DIR is required}"
ROOTFS_DIR="$(cd "$ROOTFS_DIR" && pwd)"
WORKSPACE_DIR="$ROOTFS_DIR/root/tgoskits"
GUEST_CARGO_HOME="$ROOTFS_DIR/root/.cargo"

HOST_CARGO="${HOST_CARGO:-cargo}"
HOST_CARGO_HOME="${HOST_CARGO_HOME:-$HOME/.cargo}"
# ksym 0.6 uses the unstable `let_chains` feature.  The host's default
# nightly may be older than the guest toolchain, so select the matching
# nightly explicitly.  Set HOST_RUST_TOOLCHAIN= to use HOST_CARGO directly.
HOST_RUST_TOOLCHAIN="${HOST_RUST_TOOLCHAIN:-nightly-2026-05-28}"
PREFETCH_STARRY_TOOLS="${PREFETCH_STARRY_TOOLS:-1}"
GLIBC_HOST_TARGET="${GLIBC_HOST_TARGET:-riscv64gc-unknown-linux-gnu}"
GLIBC_HOST_LINKER="${GLIBC_HOST_LINKER:-/usr/bin/riscv64gc-unknown-linux-gnu-gcc}"

HOST_CARGO_ARGS=()
if [[ -n "$HOST_RUST_TOOLCHAIN" ]]; then
    HOST_CARGO_ARGS+=("+$HOST_RUST_TOOLCHAIN")
fi

die() {
    echo "[ERROR] $*" >&2
    exit 1
}

command -v "$HOST_CARGO" >/dev/null 2>&1 || die "host cargo not found: $HOST_CARGO"
[ -f "$WORKSPACE_DIR/Cargo.toml" ] || die "TGOSKits workspace not found: $WORKSPACE_DIR"
[ -f "$WORKSPACE_DIR/Cargo.lock" ] || die "Cargo.lock not found: $WORKSPACE_DIR/Cargo.lock"
[ -x "$ROOTFS_DIR$GLIBC_HOST_LINKER" ] || die "guest glibc host linker not found: $ROOTFS_DIR$GLIBC_HOST_LINKER"

host_cargo_version="$($HOST_CARGO "${HOST_CARGO_ARGS[@]}" --version)" || \
    die "cannot run host Cargo with toolchain ${HOST_RUST_TOOLCHAIN:-<direct>}; set HOST_RUST_TOOLCHAIN= or install the requested toolchain"

mkdir -p "$HOST_CARGO_HOME" "$GUEST_CARGO_HOME"

echo "[INFO] host Cargo: $host_cargo_version"
echo "[INFO] host CARGO_HOME: $HOST_CARGO_HOME"
echo "[INFO] guest workspace: $WORKSPACE_DIR"

# Fetch the complete locked workspace.  Do not pass --target here: the
# workspace lockfile contains architecture-specific packages which must also
# be available to an offline Cargo resolver.
echo "[INFO] fetching TGOSKits dependencies on the host..."
CARGO_HOME="$HOST_CARGO_HOME" "$HOST_CARGO" "${HOST_CARGO_ARGS[@]}" fetch \
    --locked \
    --manifest-path "$WORKSPACE_DIR/Cargo.toml"

# axbuild's Starry post-processing may invoke these through `cargo install` if
# rust-nm/rust-objcopy/gen_ksym are not already present in the guest.  Install
# into a disposable host directory only to populate the host Cargo cache; the
# host binaries themselves are intentionally not copied into rootfs-rv.
if [[ "$PREFETCH_STARRY_TOOLS" != "0" ]]; then
    tool_stage="$(mktemp -d "${TMPDIR:-/tmp}/starry-cargo-tools.XXXXXX")"
    cleanup() {
        rm -rf "$tool_stage"
    }
    trap cleanup EXIT

    echo "[INFO] fetching cargo-binutils dependencies on the host..."
    CARGO_HOME="$HOST_CARGO_HOME" "$HOST_CARGO" "${HOST_CARGO_ARGS[@]}" install \
        --root "$tool_stage" cargo-binutils

    echo "[INFO] fetching ksym dependencies on the host..."
    CARGO_HOME="$HOST_CARGO_HOME" "$HOST_CARGO" "${HOST_CARGO_ARGS[@]}" install \
        --root "$tool_stage" ksym
fi

# Copy only Cargo's data directories.  In particular, do not copy
# $HOST_CARGO_HOME/bin: those are host executables, not RISC-V guest tools.
for cache_dir in registry git; do
    if [[ -d "$HOST_CARGO_HOME/$cache_dir" ]]; then
        echo "[INFO] copying Cargo $cache_dir cache"
        # Git pack files are commonly installed read-only.  A subsequent
        # rootfs refresh must still be able to replace them when the host
        # cache has advanced, so make the generated destination writable
        # before merging the refreshed cache contents.
        if [[ -d "$GUEST_CARGO_HOME/$cache_dir" ]]; then
            chmod -R u+w "$GUEST_CARGO_HOME/$cache_dir"
        fi
        mkdir -p "$GUEST_CARGO_HOME/$cache_dir"
        cp -a "$HOST_CARGO_HOME/$cache_dir/." "$GUEST_CARGO_HOME/$cache_dir/"
    fi
done

# This is a generated rootfs tree, so the guest Cargo configuration can be
# owned by the image build.  Keep a backup if a previous local configuration
# exists, then make offline mode effective for both cargo and nested cargo
# invocations started by `cargo xtask`.
guest_config="$GUEST_CARGO_HOME/config.toml"
if [[ -f "$guest_config" ]]; then
    cp -f "$guest_config" "$guest_config.before-offline"
fi
cat > "$guest_config" <<'CONFIG_EOF'
[net]
offline = true
git-fetch-with-cli = true
CONFIG_EOF

# Build scripts and proc-macros for Starry's none-elf target execute on the
# RISC-V Linux host.  Select the isolated glibc linker for that host target;
# the target-specific Starry linker configuration remains owned by tgoskits.
cat >> "$guest_config" <<CONFIG_EOF

[env]
# cc-rs treats HOST == TARGET as a native build and otherwise falls back to
# /usr/bin/cc, which is the guest's musl compiler.  Select the matching glibc
# wrapper for C dependencies of GNU-host build scripts such as libz-sys.
CC_${GLIBC_HOST_TARGET//-/_} = "${GLIBC_HOST_LINKER}"

[target.${GLIBC_HOST_TARGET}]
linker = "${GLIBC_HOST_LINKER}"
CONFIG_EOF

echo "[INFO] guest Cargo cache size:"
du -sh "$GUEST_CARGO_HOME" 2>/dev/null || true
echo "[OK] rootfs-rv Cargo offline preparation complete"
