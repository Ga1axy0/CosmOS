#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOTFS_PATH="rootfs"
ROOTFS_TAR="rootfs.tar"

cd "$PROJECT_ROOT"

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "[ERROR] missing required tool: $1" >&2
        exit 1
    fi
}

pack_rootfs_submodule() {
    local submodule_path="$1"

    if [ ! -d "$submodule_path" ]; then
        echo "[ERROR] rootfs submodule path is not a directory: $submodule_path" >&2
        exit 1
    fi

    # rootfs 体积较大，评测快照中只保留 tar 包。
    rm -f "$ROOTFS_TAR"
    tar -cf "$ROOTFS_TAR" -C "$submodule_path" .
    git rm --cached -f -q "$submodule_path"
    rm -rf "$submodule_path"
    git add "$ROOTFS_TAR"
}

flatten_regular_submodule() {
    local submodule_path="$1"

    # 普通子模块直接展开成目录，避免评测环境依赖 submodule。
    git rm --cached -f -q "$submodule_path"
    rm -rf "$submodule_path/.git"
    git add -A "$submodule_path"
}

flatten_submodules() {
    if [ ! -f .gitmodules ]; then
        echo "[INFO] .gitmodules not found, skipping submodule flatten step"
        return
    fi

    require_tool tar

    echo "[INFO] syncing and updating submodules"
    git submodule sync --recursive
    git submodule update --init --recursive

    while IFS= read -r submodule_path; do
        [ -n "$submodule_path" ] || continue

        if [ ! -e "$submodule_path" ]; then
            echo "[ERROR] submodule path missing: $submodule_path" >&2
            exit 1
        fi

        echo "[INFO] flatten submodule: $submodule_path"
        if [ "$submodule_path" = "$ROOTFS_PATH" ]; then
            pack_rootfs_submodule "$submodule_path"
        else
            flatten_regular_submodule "$submodule_path"
        fi
    done < <(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '{print $2}')

    git rm --cached -f -q .gitmodules || true
    rm -f .gitmodules
    git add -A
}

flatten_submodules
