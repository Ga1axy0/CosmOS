# Perf Probe Suite

## TLDR

命名耗时探针已经可用：用 `crate::probe!({ 一段代码 }, "注册名")` 包住热点代码；用 `PERF_PROBE=1` 构建；guest 内 `echo 1 > /proc/perf_probe_enable` 开启，`cat /proc/perf_probe` 读取 `calls/total_ns/avg_ns/max_ns`。默认构建不启用统计，不用于正式性能对比。

## 接口

代码侧使用块式宏：

```rust
crate::probe!({
    add_timer_inner();
}, "timer.add");
```

宏的行为：

- `perf_probe` feature 关闭时，只保留原始代码块，不读时钟，不注册名字。
- `perf_probe` feature 开启但 `/proc/perf_probe_enable` 为 `0` 时，只检查全局开关，不注册名字。
- 第一次启用并命中调用点时，按字符串注册槽位；之后同一调用点走静态原子缓存。
- 代码块里的 `return`、`?`、panic unwind/drop 路径仍会触发 guard drop，记录已耗时间。
- 目前最多注册 64 个名字，适合短期热点诊断；正式提交优化时应删掉临时探针。

## 构建

默认构建不启用探针：

```bash
make all BUILD_ARCH=rv SMP=1 KEEP_SDCARD=1
```

启用探针构建：

```bash
make all BUILD_ARCH=rv SMP=1 KEEP_SDCARD=1 PERF_PROBE=1
```

顶层 Makefile 现在把 `LOG` 和 `PERF_PROBE` 写入同一个 kernel config stamp；在 `PERF_PROBE=1` 和普通构建之间切换会触发内核重编，避免误用带探针的 `kernel-rv`。

## Guest 控制

```bash
echo reset > /proc/perf_probe
echo 1 > /proc/perf_probe_enable
# run workload
cat /proc/perf_probe
echo 0 > /proc/perf_probe_enable
```

`/proc/perf_probe` 输出格式：

```text
enabled 1
name calls total_ns avg_ns max_ns
timer.interrupt 108 7108000 65814 407200
timer.check_expired 108 968800 8970 339200
```

`echo reset > /proc/perf_probe` 或 truncate `/proc/perf_probe` 会清零统计；已注册名字保留，便于多轮读表对齐。

## Cyclictest 热点采样

示例命令：

```bash
SMP=1 PERF_PROBE=1 RUN_TIMEOUT=600 \
  bash .codex/skills/perf-opt/scripts/drive_cosmos_qemu.sh \
  'echo reset > /proc/perf_probe; echo 1 > /proc/perf_probe_enable; cd /mnt/glibc && ./cyclictest_testcode.sh; echo 0 > /proc/perf_probe_enable; cat /proc/perf_probe; echo RC_$?'
```

执行完整 `cyclictest_testcode.sh` 的探针采样只用于热点定位，不进入性能对比表，也不更新 `docs/cyclictest_perf_notes.md` 的 TLDR baseline 差距表。原始输出可放入 `test.md` 追溯，探针耗时汇总追加到 `docs/cyclictest_probe_summary.csv`。

## 建议热点名

当前框架本身不要求固定探针点。针对 cyclictest，可以优先在后续热点提交里使用这些名字：

- `timer.interrupt`
- `timer.check_expired`
- `timer.add`
- `timer.remove`
- `trap.user_timer_periodic`
- `trap.kernel_timer_periodic`
- `net.poll_timer_tick`
- `sys.clock_nanosleep_arm`
- `sys.nanosleep_arm`

## 添加临时探针

优先把探针放在明确边界上，例如锁内扫描、timer arm、唤醒、调度决策、网络 poll gate。不要跨越主动阻塞或上下文切换的大范围路径，否则 `max_ns` 会把等待时间也算进去。

命名建议用 `模块.动作`，例如：

```rust
crate::probe!({
    self.pick_next_task()
}, "sched.pick_next")
```

如果要测一个函数的全体耗时，可以把函数体整体放进宏块；如果函数返回值不是 `()`，让宏块最后一行返回原值即可。
