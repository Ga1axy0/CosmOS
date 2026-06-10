#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

flatten_submodules() {
    if [ ! -f .gitmodules ]; then
        echo "[INFO] .gitmodules not found, skipping submodule flatten step"
        return
    fi

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
        git rm --cached -f -q "$submodule_path"
        rm -rf "$submodule_path/.git"
        git add -A "$submodule_path"
    done < <(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '{print $2}')

    git rm --cached -f -q .gitmodules || true
    rm -f .gitmodules
}

strip_hidden_files() {
    echo "[INFO] removing hidden files to match evaluation environment"

    while IFS= read -r relpath; do
        [ -n "$relpath" ] || continue
        rm -rf -- "$relpath"
    done < <(
        find . \
            \( -path './.git' -o -path './.git/*' \) -prune -o \
            -name '.*' -printf '%P\n' | sort
    )

    git add -A
}

flatten_submodules
strip_hidden_files
