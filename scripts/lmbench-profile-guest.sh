#!/bin/sh

# Run a broad, self-contained lmbench subset in either CosmOS or Linux.
# The host harness supplies the same static RISC-V binary to both guests.

set -u

PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH

CASE_TIMEOUT=${LMBENCH_CASE_TIMEOUT:-45}
GROUPS=${LMBENCH_GROUPS:-}
CASES=${LMBENCH_CASES:-}
DATA_FILE=/tmp/lmbench-data
LOCAL_BINARY=/tmp/lmbench_all

group_enabled() {
    case ",${GROUPS}," in
        *,all,*|*,"$1",*) return 0 ;;
        *) return 1 ;;
    esac
}

case_enabled() {
    name=$1
    if [ -z "$CASES" ]; then
        return 0
    fi
    case ",${CASES}," in
        *,"$name",*) return 0 ;;
        *) return 1 ;;
    esac
}

find_binary() {
    if [ -x /.cosmos-old-root/root/lmbench_all ]; then
        echo /.cosmos-old-root/root/lmbench_all
    elif [ -x /host/.make/lmbench-old/lmbench_all ]; then
        echo /host/.make/lmbench-old/lmbench_all
    else
        echo ''
    fi
}

run_case() {
    name=$1
    shift
    case_enabled "$name" || return 0
    echo "LMBENCH_CASE_BEGIN name=$name"
    echo "LMBENCH_COMMAND $*"
    if command -v timeout >/dev/null 2>&1; then
        timeout "$CASE_TIMEOUT" "$LMBENCH" "$@"
        status=$?
    else
        echo 'LMBENCH_CASE_ERROR missing_timeout'
        status=127
    fi
    echo "LMBENCH_CASE_END name=$name status=$status"
}

run_loopback_case() {
    name=$1
    mode=$2
    shift 2
    case_enabled "$name" || return 0
    server_log=/tmp/lmbench-${mode}-server.log
    rm -f "$server_log"
    echo "LMBENCH_CASE_BEGIN name=$name"
    echo "LMBENCH_COMMAND $mode -s; $mode $*"
    "$LMBENCH" "$mode" -s >"$server_log" 2>&1 &
    server_pid=$!
    sleep 1
    if command -v timeout >/dev/null 2>&1; then
        timeout "$CASE_TIMEOUT" "$LMBENCH" "$mode" "$@"
        status=$?
    else
        echo 'LMBENCH_CASE_ERROR missing_timeout'
        status=127
    fi
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    echo "LMBENCH_SERVER_LOG_BEGIN name=$name"
    cat "$server_log" 2>/dev/null || true
    echo "LMBENCH_SERVER_LOG_END name=$name"
    echo "LMBENCH_CASE_END name=$name status=$status"
}

echo LMBENCH_PROFILE_BEGIN
source_binary=$(find_binary)
if [ -z "$source_binary" ]; then
    echo LMBENCH_PROFILE_ERROR missing_binary
    echo '[cargo-perf] CARGO_PERF_DONE status=1'
    exit 1
fi

# Execute from the guest filesystem in both systems.  Linux obtains the
# source through 9p, while CosmOS obtains it from the bootstrap root; neither
# source path is used by the measured commands below.
cp "$source_binary" "$LOCAL_BINARY"
chmod 755 "$LOCAL_BINARY"
LMBENCH=$LOCAL_BINARY
echo "LMBENCH_BINARY source=$source_binary local=$LMBENCH"
if [ -z "$GROUPS" ]; then
    if [ -r /.cosmos-old-root/root/lmbench-groups ]; then
        GROUPS=$(cat /.cosmos-old-root/root/lmbench-groups)
    elif [ -r /host/.make/lmbench-groups ]; then
        GROUPS=$(cat /host/.make/lmbench-groups)
    else
        GROUPS=syscall,process,signal_sync_ipc,memory,file,loopback_network
    fi
fi
if [ -z "$CASES" ]; then
    if [ -r /.cosmos-old-root/root/lmbench-cases ]; then
        CASES=$(cat /.cosmos-old-root/root/lmbench-cases)
    elif [ -r /host/.make/lmbench-cases ]; then
        CASES=$(cat /host/.make/lmbench-cases)
    fi
fi
echo "LMBENCH_GROUPS $GROUPS"
echo "LMBENCH_CASES ${CASES:-all}"

rm -f "$DATA_FILE" /tmp/lmbench-*.server.log /tmp/lmbench-*-server.log
if command -v dd >/dev/null 2>&1; then
    dd if=/dev/zero of="$DATA_FILE" bs=1M count=16 2>/dev/null || true
fi
if [ ! -f "$DATA_FILE" ]; then
    : > "$DATA_FILE"
fi
cp /bin/true /tmp/hello 2>/dev/null || cp /bin/busybox /tmp/hello 2>/dev/null || true
chmod 755 /tmp/hello 2>/dev/null || true

if group_enabled syscall; then
echo LMBENCH_GROUP_BEGIN name=syscall
run_case syscall_null lat_syscall -P 1 -W 1 -N 5 null
run_case syscall_read lat_syscall -P 1 -W 1 -N 5 read
run_case syscall_write lat_syscall -P 1 -W 1 -N 5 write
run_case syscall_stat lat_syscall -P 1 -W 1 -N 5 stat "$DATA_FILE"
run_case syscall_fstat lat_syscall -P 1 -W 1 -N 5 fstat "$DATA_FILE"
run_case syscall_open lat_syscall -P 1 -W 1 -N 5 open "$DATA_FILE"
echo LMBENCH_GROUP_END name=syscall
fi

if group_enabled process; then
echo LMBENCH_GROUP_BEGIN name=process
run_case proc_procedure lat_proc -P 1 -W 1 -N 5 procedure
run_case proc_fork lat_proc -P 1 -W 1 -N 5 fork
run_case proc_exec lat_proc -P 1 -W 1 -N 5 exec
run_case proc_shell lat_proc -P 1 -W 1 -N 5 shell
echo LMBENCH_GROUP_END name=process
fi

if group_enabled signal_sync_ipc; then
echo LMBENCH_GROUP_BEGIN name=signal_sync_ipc
run_case sig_install lat_sig -P 1 -W 1 -N 5 install
run_case sig_catch lat_sig -P 1 -W 1 -N 5 catch
run_case sig_prot lat_sig -P 1 -W 1 -N 5 prot "$DATA_FILE"
run_case pipe lat_pipe -P 1 -W 1 -N 5
run_case unix lat_unix -P 1 -W 1 -N 5
run_case fifo lat_fifo -P 1 -W 1 -N 5
run_case ctx lat_ctx -P 1 -W 1 -N 5 2
run_case select_file lat_select -n 4 -P 1 -W 1 -N 5 file
run_case fcntl lat_fcntl -P 1 -W 1 -N 5
run_case sem lat_sem -P 1 -W 1 -N 5
echo LMBENCH_GROUP_END name=signal_sync_ipc
fi

if group_enabled memory; then
echo LMBENCH_GROUP_BEGIN name=memory
run_case mem_rd lat_mem_rd -P 1 -W 1 -N 5 1M 64 128 256
run_case mem_rd_4M lat_mem_rd -P 1 -W 1 -N 5 4M 64 256
run_case bw_rd bw_mem -P 1 -W 1 -N 5 1M rd
run_case bw_wr bw_mem -P 1 -W 1 -N 5 1M wr
run_case bw_rdwr bw_mem -P 1 -W 1 -N 5 1M rdwr
run_case bw_cp bw_mem -P 1 -W 1 -N 5 1M cp
run_case bw_bzero bw_mem -P 1 -W 1 -N 5 1M bzero
run_case bw_bcopy bw_mem -P 1 -W 1 -N 5 1M bcopy
run_case rand lat_rand
run_case dram_page lat_dram_page -W 1 -N 5 -M 16M
run_case usleep lat_usleep -u nanosleep -P 1 -W 1 -N 5 0
echo LMBENCH_GROUP_END name=memory
fi

if group_enabled file; then
echo LMBENCH_GROUP_BEGIN name=file
run_case fs lat_fs -s 8k -n 4 -P 1 -W 1 -N 1 /tmp
run_case file_read bw_file_rd -P 1 -W 1 -N 1 64k io_only "$DATA_FILE"
run_case file_open_close bw_file_rd -P 1 -W 1 -N 1 64k open2close "$DATA_FILE"
echo LMBENCH_GROUP_END name=file
fi

if group_enabled loopback_network; then
echo LMBENCH_GROUP_BEGIN name=loopback_network
run_loopback_case tcp lat_tcp -P 1 -W 1 -N 5 127.0.0.1
run_loopback_case udp lat_udp -P 1 -W 1 -N 5 127.0.0.1
run_loopback_case connect lat_connect -N 5 127.0.0.1
echo LMBENCH_GROUP_END name=loopback_network
fi

echo LMBENCH_PROFILE_DONE
echo '[cargo-perf] CARGO_PERF_DONE status=0'
sync
poweroff -f 2>/dev/null \
    || /bin/busybox poweroff -f 2>/dev/null \
    || reboot -f 2>/dev/null \
    || exit 0
