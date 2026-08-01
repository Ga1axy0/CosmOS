# RISC-V syscall 延迟调查摘要

更新日期：2026-07-29

## 测试对象与环境

- `lmbench lat_syscall null` 在当前版本中执行的是 RISC-V syscall 173：`getppid`，不是 syscall 172：`getpid`。
- 固定微基准执行 100,000 次 raw `getppid`，避免 libc/vDSO 路径干扰。
- 主要受控环境：QEMU `virt`、RV64GC、TCG、4 GiB、SMP=1，并将 QEMU 固定到同一宿主 CPU。
- 这里的绝对时间主要代表 QEMU TCG；电源状态、宿主频率和调度会造成明显噪声，比较时应使用交错启动和中位数。

用户最初观测：

| 系统 | `lat_syscall null` |
|---|---:|
| Linux | 1.09–1.25 µs |
| CosmOS 原始版本 | 21.97–29.00 µs |
| CosmOS 去掉无条件 `fence.i` 后 | 曾测得约 16.7–17.0 µs，后续稳定环境约 19 µs |

同一宿主、同一 QEMU 上，Linux raw `getppid` 中位数约为 0.827 µs。

## CosmOS 软件路径分层

一次分层 sweep 的中位数如下。墙钟数据包含宿主噪声，`icount` 更适合观察 guest 路径长度，但不能反映 `satp` 等 QEMU helper 的真实成本。

| 路径 | 墙钟时间 | guest 指令/调用 |
|---|---:|---:|
| 无 `fence.i` 基线 | 20.390 µs | 2465 |
| `current_task_cache` | 16.894 µs | 1617 |
| `process_identity_cache` | 16.907 µs | 1504 |
| `return_work_cache` | 15.985 µs | 1429 |
| `trap_context_cache` | 14.844 µs | 1029 |
| Rust 最小 getppid 路径 | 10.303 µs | 479 |
| 汇编保存/恢复，包含两次 `satp` | 4.638 µs | 117 |
| 汇编保存/恢复，不写 `satp` | 0.533 µs | 111 |

更严格的相邻层交错测试表明：

- `current_task_cache` 是最大的单项软件收益，约降低 3.5 µs。
- `process_identity_cache` 减少约 113 条 guest 指令，但没有测到稳定的独立墙钟收益。
- `return_work_cache` 相对 `process_identity_cache` 快约 10.6%–12.4%。
- `trap_context_cache` 相对 `return_work_cache` 快约 4.8%–12.2%。
- 完整 cache 链相对基线的严格配对改善约为 24%–29%。
- 修复 `return_work_cache` 竞态后，完整 cache 链三轮中位数为 14.094、14.071、13.947 µs，没有观察到性能回退。

其他已测开销：

- 进出内核的 CPU accounting 与 active-user-hart 维护合计约 2.2 µs。
- 其中 active-user-hart 位图更新约 0.5–0.9 µs。
- syscall 内核中断 enable/restore guard 约 0.55 µs。
- 每次 trap 轮询 TLB shootdown mailbox 只约 14 条 guest 指令，不是主要瓶颈。

## Cache 结构与状态

Feature 依赖关系：

```text
trap_context_cache
└── return_work_cache
    └── process_identity_cache
        └── current_task_cache
```

- `current_task_cache`
  - 每个 hart 使用一个 `AtomicPtr<TaskControlBlock>` 快速定位当前任务。
  - `Processor.current: Arc<TCB>` 仍是所有权来源。
  - 正确性依赖当前内核不可抢占、同一任务不会同时运行在两个 hart。
- `process_identity_cache`
  - PCB 中缓存 `parent_pid: AtomicUsize`，避免 `getppid` 获取 PCB 大锁和升级父进程 `Weak`。
  - 初始化、fork/`CLONE_PARENT`、spawn 和 reparent 写入点均已同步。
- `return_work_cache`
  - TCB 中缓存 `resched_work_pending`，PCB 中缓存单调的 `zombie_work_pending`。
  - 已修复锁外清除 reschedule hint 覆盖并发生产者 `true` 的竞态。
  - 所有 `resched_reason` 与 hint 更新现在统一通过
    `TaskControlBlock::set_resched_reason_locked()`，并由同一把 `task_inner` 锁排序。
- `trap_context_cache`
  - TCB 中缓存 trap context PPN、用户 VA 和 address-space token。
  - 当前单任务单 hart 模型下工作正常；后续仍应加固 exec 时 PPN/token 的成组发布和旧映射生命周期。

`return_work_cache` 修复后的 SMP=4 测试包括：

- 40 轮八线程 `getppid + sched_yield + exit_group`；
- 4 路并发、每路 100,000 次 `getppid`；
- 100 次并发 fork/exec/wait；
- SIGTERM/wait；
- 一个忙任务在 4 个 hart 间迁移 80 次；
- 带 `sched_invariant_checks` 的构建未发现 `Some(resched_reason) + false hint`。

## 页表切换是剩余的架构级问题

汇编探针显示，两次 `satp` 写入在当前 QEMU 上增加约：

```text
4.638 - 0.533 = 4.105 µs
```

`icount` 只增加约 6 条 guest 指令，说明主要成本来自 QEMU 的 CSR/TLB/address-space helper，而不是普通指令数量。

CosmOS 当前行为：

```text
用户页表只包含用户区域、trampoline 和少量特殊映射
    ↓ syscall
trampoline 写 satp，切换到内核页表
    ↓ return
再次写 satp，切回用户页表
```

Linux RISC-V 的普通 syscall 入口和返回不切换 `satp`：

- 用户 PGD 中共享 supervisor-only 的内核高半区映射；
- syscall 在同一地址空间中进入内核；
- `satp` 通常只在调度器切换到不同 `mm` 时更新。

参考代码：

- `../linux/arch/riscv/include/asm/pgalloc.h` 中的 `sync_kernel_mappings()`；
- `../linux/arch/riscv/mm/context.c` 中的 `switch_mm()`；
- `../linux/arch/riscv/kernel/entry.S` 中的 syscall 入口/返回。

CosmOS 不能直接把现有内核页表项复制到用户页表：当前内核/物理 direct map 使用低地址区域，可能与用户 VA 规划冲突。

建议的页表优化路线：

1. 将 kernel text/data 和物理 direct map 移到 Sv39 高半区。
2. 为每个用户页表共享内核高半区顶层页表项，所有内核 PTE 清除 `U` 位。
3. trampoline 只切换内核栈、`tp` 和 trap 状态，不再在每次 syscall 写 `satp`。
4. 仅在 `switch_mm()` 切换不同进程地址空间时写 `satp`。
5. 明确定义 ASID 分配、`sfence.vma`、TLB shootdown 和页表更新同步规则。
6. 保持用户指针校验、`SUM`/PTE 权限隔离，并根据安全目标评估是否需要 KPTI。

无条件 `fence.i` 不应出现在普通 syscall 返回路径；它只应在本 hart 可能执行新写入或新映射的指令之后使用。但目前数据也表明，单独删除 `fence.i` 不能解决主要差距。

## 构建完整 cache 测试内核

```sh
make -C os kernel ARCH=riscv64 \
  EXTRA_FEATURES='--features legacy-vdb-names --features trap_context_cache'
```

需要检查提示位不变量时额外启用：

```text
--features sched_invariant_checks
```
