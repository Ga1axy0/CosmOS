#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
image=${1:-/tmp/cosmos-ext4-extent-test.img}

truncate -s 64M "$image"
mkfs.ext4 -q -F -b 4096 \
    -E lazy_itable_init=0,lazy_journal_init=0 \
    "$image"

cargo run --quiet \
    --manifest-path "$repo_root/fs/src/ext4_rs/Cargo.toml" \
    --example extent_stress -- "$image"

e2fsck -fn "$image"
