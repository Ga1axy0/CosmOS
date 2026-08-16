#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

TRIALS="${TRIALS:-3}"
WORKERS="${WORKERS:-8}"
STEPS="${STEPS:-32}"
WARMUP="${WARMUP:-1}"
IO_MIB="${IO_MIB:-128}"
POLICIES="${POLICIES:-off bais}"
BUILD="${BUILD:-1}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-90}"
RUN_TIMEOUT="${RUN_TIMEOUT:-300}"
QEMU_TIMEOUT="${QEMU_TIMEOUT:-420}"
RESULT_DIR="${RESULT_DIR:-$PROJECT_ROOT/results/llama2c-bais}"

case "$TRIALS:$WORKERS:$STEPS:$WARMUP:$IO_MIB" in
    *[!0-9:]*|0:*) echo "numeric arguments are invalid" >&2; exit 2 ;;
esac

read -r -a policy_list <<< "$POLICIES"
if [ "${#policy_list[@]}" -eq 0 ]; then
    echo "POLICIES must not be empty" >&2
    exit 2
fi
for policy in "${policy_list[@]}"; do
    case "$policy" in
        off|bais) ;;
        *) echo "unsupported policy: $policy" >&2; exit 2 ;;
    esac
done

for tool in awk cp date mkfifo mktemp rg sed seq sort timeout wc sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 2
    fi
done

mkdir -p "$RESULT_DIR"
timestamp="$(date +%Y%m%d-%H%M%S)"
log_path="$RESULT_DIR/llama2c-bais-$timestamp.log"
csv_path="$RESULT_DIR/llama2c-bais-$timestamp.csv"
stage_dir=""
runner_pid=""
input_fifo=""
run_dir=""
trial_disk=""

cleanup_all() {
    exec 3>&- 2>/dev/null || true
    if [ -n "$runner_pid" ] && kill -0 "$runner_pid" 2>/dev/null; then
        kill "$runner_pid" 2>/dev/null || true
        wait "$runner_pid" 2>/dev/null || true
    fi
    if [ -n "$input_fifo" ]; then
        rm -f "$input_fifo"
    fi
    if [ -n "$trial_disk" ]; then
        rm -f "$trial_disk"
    fi
    if [ -n "$run_dir" ]; then
        rmdir "$run_dir" 2>/dev/null || true
    fi
    if [ -n "$stage_dir" ] && [ -d "$stage_dir" ]; then
        rm -rf "$stage_dir"
    fi
}
trap cleanup_all EXIT INT TERM

if [ "$BUILD" = "1" ]; then
    "$PROJECT_ROOT/scripts/prepare-llama2c-model.sh"
    "$PROJECT_ROOT/scripts/build-llama2c-bais-rv.sh"
    user_sysroot="$(cd "$PROJECT_ROOT/user" && rustc --print sysroot)"
    user_host="$(cd "$PROJECT_ROOT/user" && rustc -vV | awk '/^host:/ {print $2}')"
    toolchain_bin="$user_sysroot/lib/rustlib/$user_host/bin"
    export PATH="$toolchain_bin:$PATH"
    make -C "$PROJECT_ROOT" user-apps kernel-rv

    stage_dir="$(mktemp -d /tmp/cosmos-llama2c-rootfs.XXXXXX)"
    mkdir -p "$stage_dir/root/llama2c"
    cp "$PROJECT_ROOT/benchmarks/llama2c/build/llama2c-bais-rv" \
        "$stage_dir/root/llama2c/llama2c-bais-rv"
    cp "$PROJECT_ROOT/benchmarks/llama2c/cache/stories15M.bin" \
        "$stage_dir/root/llama2c/stories15M.bin"
    cp "$PROJECT_ROOT/benchmarks/llama2c/cache/tokenizer.bin" \
        "$stage_dir/root/llama2c/tokenizer.bin"
    EXTRA_ROOTFS_DIR="$stage_dir" \
    LOOP_FAT32_ENABLE=0 \
    MUSL_ARCH=riscv64 \
        "$PROJECT_ROOT/scripts/pack-disk-img.sh" \
        "$PROJECT_ROOT/CosmOS-rootfs/rootfs-rv" \
        "$PROJECT_ROOT/user/target/riscv64gc-unknown-none-elf/release" \
        "$PROJECT_ROOT/disk.img"
    rm -rf "$stage_dir"
    stage_dir=""
fi

: > "$log_path"
printf 'run,trial,policy,workers,steps,warmup,measured,ttft_ns,first_forward_ns,token_avg_ns,token_p50_ns,token_p95_ns,token_p99_ns,token_max_ns,tokens_per_second,e2e_ns,checksum,io_mib,io_requested_bytes,io_completed_bytes,io_elapsed_ns,io_status\n' > "$csv_path"

wait_for_log() {
    local pattern="$1"
    local timeout_seconds="$2"
    local runner_pid="$3"
    local first_line="$4"
    local deadline=$((SECONDS + timeout_seconds))
    while ! tail -n +"$first_line" "$log_path" | rg -q "$pattern"; do
        if ! kill -0 "$runner_pid" 2>/dev/null; then
            echo "QEMU exited while waiting for: $pattern" >&2
            return 1
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "timeout waiting for QEMU log pattern: $pattern" >&2
            return 1
        fi
        sleep 0.2
    done
}

run_index=0
policy_count="${#policy_list[@]}"
for trial in $(seq 1 "$TRIALS"); do
    if [ $((trial % 2)) -eq 1 ]; then
        ordered_policies=("${policy_list[@]}")
    else
        ordered_policies=()
        for ((index = policy_count - 1; index >= 0; index--)); do
            ordered_policies+=("${policy_list[index]}")
        done
    fi
    for policy in "${ordered_policies[@]}"; do
        run_index=$((run_index + 1))
        marker="__LLAMA_BAIS_DONE_${run_index}__"
        run_dir="$(mktemp -d "$RESULT_DIR/.llama-qemu.XXXXXX")"
        input_fifo="$run_dir/input"
        trial_disk="$run_dir/disk.img"
        mkfifo "$input_fifo"
        cp --reflink=auto "$PROJECT_ROOT/disk.img" "$trial_disk"
        first_line=$(( $(wc -l < "$log_path") + 1 ))
        (
            cd "$PROJECT_ROOT"
            timeout "$QEMU_TIMEOUT" make run RUN_ARCH=rv SMP="$WORKERS" DISK_RV_IMG="$trial_disk" \
                < "$input_fifo" >> "$log_path" 2>&1
        ) &
        runner_pid=$!
        exec 3>"$input_fifo"

        wait_for_log '\[final_auto_run\] default mode' "$BOOT_TIMEOUT" "$runner_pid" "$first_line"
        printf '\003' >&3
        wait_for_log 'root@CosmOS:.*#' 15 "$runner_pid" "$first_line"
        printf 'cd /root/llama2c; ./llama2c-bais-rv stories15M.bin -z tokenizer.bin -t 0 -n %s -i "Once upon a time" -c %s -b %s -w %s -q 1 -d %s; cat /proc/bais; echo %s\n' \
            "$STEPS" "$WORKERS" "$policy" "$WARMUP" "$IO_MIB" "$marker" >&3
        wait_for_log "^${marker}\r?$" "$RUN_TIMEOUT" "$runner_pid" "$first_line"
        printf '\001x' >&3 || true
        exec 3>&-
        wait "$runner_pid" 2>/dev/null || true
        runner_pid=""
        rm -f "$input_fifo"
        input_fifo=""
        rm -f "$trial_disk"
        trial_disk=""
        rmdir "$run_dir" 2>/dev/null || true
        run_dir=""
    done
done

mapfile -t result_lines < <(sed -n 's/.*LLAMA_BAIS_RESULT /LLAMA_BAIS_RESULT /p' "$log_path" | tr -d '\r')
mapfile -t io_lines < <(sed -n 's/.*LLAMA_BAIS_IO /LLAMA_BAIS_IO /p' "$log_path" | tr -d '\r')
expected=$((TRIALS * policy_count))
if [ "${#result_lines[@]}" -ne "$expected" ] || [ "${#io_lines[@]}" -ne "$expected" ]; then
    echo "expected $expected result and I/O rows; got ${#result_lines[@]} and ${#io_lines[@]}" >&2
    exit 1
fi

reference_checksum=""
for ((index = 0; index < expected; index++)); do
    declare -A result=()
    declare -A io=()
    for field in ${result_lines[index]#LLAMA_BAIS_RESULT }; do
        result["${field%%=*}"]="${field#*=}"
    done
    for field in ${io_lines[index]#LLAMA_BAIS_IO }; do
        io["${field%%=*}"]="${field#*=}"
    done
    policy="${result[policy]}"
    trial=$((index / policy_count + 1))
    if [ "${io[policy]}" != "$policy" ] || [ "${io[status]}" != "0" ] \
        || [ "${io[requested_bytes]}" != "${io[completed_bytes]}" ]; then
        echo "invalid I/O result at run $((index + 1))" >&2
        exit 1
    fi
    if [ -z "$reference_checksum" ]; then
        reference_checksum="${result[checksum]}"
    elif [ "$reference_checksum" != "${result[checksum]}" ]; then
        echo "token checksum differs across runs/policies at run $((index + 1))" >&2
        exit 1
    fi
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$((index + 1))" "$trial" "$policy" "${result[workers]}" \
        "${result[steps]}" "${result[warmup]}" "${result[measured]}" \
        "${result[ttft_ns]}" "${result[first_forward_ns]}" "${result[token_avg_ns]}" \
        "${result[token_p50_ns]}" "${result[token_p95_ns]}" \
        "${result[token_p99_ns]}" "${result[token_max_ns]}" \
        "${result[tokens_per_second]}" "${result[e2e_ns]}" \
        "${result[checksum]}" "${result[io_mib]}" \
        "${io[requested_bytes]}" "${io[completed_bytes]}" \
        "${io[elapsed_ns]}" "${io[status]}" >> "$csv_path"
    unset result io
done

echo "raw log: $log_path"
echo "csv:     $csv_path"
awk -F, '
    NR > 1 {
        n[$3]++
        ttft[$3] += $8
        first_forward[$3] += $9
        avg[$3] += $10
        p95[$3] += $12
        p99[$3] += $13
        tps[$3] += $15
        e2e[$3] += $16
    }
    END {
        print "policy,count,ttft_avg_ns,first_forward_avg_ns,token_avg_ns,token_p95_avg_ns,token_p99_avg_ns,tokens_per_second_avg,e2e_avg_ns"
        for (p in n) {
            printf "%s,%d,%.0f,%.0f,%.0f,%.0f,%.0f,%.6f,%.0f\n",
                p,n[p],ttft[p]/n[p],first_forward[p]/n[p],avg[p]/n[p],p95[p]/n[p],
                p99[p]/n[p],tps[p]/n[p],e2e[p]/n[p]
        }
    }
' "$csv_path"
