#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

flatten_regular_submodule() {
    local submodule_path="$1"

    # 普通子模块直接展开成目录，避免评测环境依赖 submodule。
    git rm --cached -f -q "$submodule_path"
    rm -rf "$submodule_path/.git"
    git add -A "$submodule_path"
    echo "[INFO] done: flatten submodule $submodule_path"
}

flatten_submodules() {
    if [ ! -f .gitmodules ]; then
        echo "[INFO] .gitmodules not found, skipping submodule flatten step"
        return
    fi

    echo "[INFO] syncing and updating submodules"
    git submodule sync --recursive
    git submodule update --init --recursive
    rm -f rootfs.tar

    while IFS= read -r submodule_path; do
        [ -n "$submodule_path" ] || continue

        if [ ! -e "$submodule_path" ]; then
            echo "[ERROR] submodule path missing: $submodule_path" >&2
            exit 1
        fi

        echo "[INFO] flatten submodule: $submodule_path"
        flatten_regular_submodule "$submodule_path"
    done < <(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '{print $2}')

    git rm --cached -f -q .gitmodules || true
    rm -f .gitmodules
    git add -A
    echo "[INFO] done: remove .gitmodules from evaluation snapshot"
}

flatten_submodules
echo "[INFO] done: export evaluation tree"
