#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CACHE_DIR="${CACHE_DIR:-$PROJECT_ROOT/benchmarks/llama2c/cache}"
MODEL_URL="${MODEL_URL:-https://huggingface.co/karpathy/tinyllamas/resolve/main/stories15M.bin}"
TOKENIZER_URL="${TOKENIZER_URL:-https://raw.githubusercontent.com/karpathy/llama2.c/350e04fe35433e6d2941dce5a1f53308f87058eb/tokenizer.bin}"
MODEL_SHA256="cd590644d963867a2b6e5a1107f51fad663c41d79c149fbecbbb1f95fa81f49a"
TOKENIZER_SHA256="50a52ef822ee9e83de5ce9d0be0a025a773d019437f58b5ff9dcafb063ece361"

for tool in curl sha256sum mkdir mv rm; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done

mkdir -p "$CACHE_DIR"

fetch_checked() {
    local url="$1"
    local output="$2"
    local expected="$3"
    if [ -f "$output" ] && printf '%s  %s\n' "$expected" "$output" | sha256sum -c - >/dev/null 2>&1; then
        echo "verified $output"
        return
    fi
    local temporary="${output}.download"
    rm -f "$temporary"
    curl --fail --location --retry 3 --output "$temporary" "$url"
    printf '%s  %s\n' "$expected" "$temporary" | sha256sum -c -
    mv "$temporary" "$output"
}

fetch_checked "$MODEL_URL" "$CACHE_DIR/stories15M.bin" "$MODEL_SHA256"
fetch_checked "$TOKENIZER_URL" "$CACHE_DIR/tokenizer.bin" "$TOKENIZER_SHA256"
