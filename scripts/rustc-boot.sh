#!/usr/bin/env bash

set -euo pipefail

# Build before opening the QEMU input pipe.  Otherwise a cold incremental
# build can consume the fixed startup delay and send Ctrl-C before the guest
# has reached its shell.
make kernel-rv PERF_PROBE=1 >/dev/null
PROJECT_NAME="testbin$((RANDOM))"
echo $PROJECT_NAME

  {
      sleep 8
      printf '\003'                         # Ctrl-C，取消自动测试
      sleep 1

      printf '%s\n' 'echo 1 > /proc/io_perf'
      sleep 0.5
      printf '%s\n' 'echo 1 > /proc/perf_probe'
      sleep 0.5
      printf '%s\n' 'echo 1 > /proc/perf_probe_enable'
      sleep 0.5

      printf 'time cargo new %s\n' "$PROJECT_NAME"
      sleep 8

      printf '%s\n' 'cat /proc/io_perf'
      sleep 1
      printf '%s\n' 'cat /proc/perf_probe'
      sleep 1

      printf '%s\n' 'echo 1 > /proc/io_perf'
      sleep 0.5
      printf '%s\n' 'echo 1 > /proc/perf_probe'
      sleep 0.5
    
      printf 'cd %s\n' "$PROJECT_NAME"
      sleep 0.5
      
      printf '%s\n' 'time cargo build'
      sleep 100

      printf '%s\n' 'cat /proc/io_perf'
      sleep 1
      printf '%s\n' 'cat /proc/perf_probe'
      sleep 1
      
      printf '%s\n' 'exit'
      sleep 1
      # The guest may close the console immediately after `exit`.
      printf '\001x' 2>/dev/null || true     # Ctrl-A x，退出 QEMU
  } | timeout 180 env FAST_RUN_QEMU_NETDEV="${FAST_RUN_QEMU_NETDEV:-user,id=net}" make fast-run PERF_PROBE=1
