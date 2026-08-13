# 原子操作、锁、并发与内存序速查

SMP 内核 bug 常常不是“少加一把锁”，而是发布顺序、唤醒时机、关中断范围、TLB 一致性和对象生命周期没有一起设计。CosmOS 的并发代码分布在 `os/src/sync`、`os/src/sched`、`os/src/task`、`os/src/mm/tlb_shootdown.rs` 和进程/文件系统路径中。

## 1. 先区分四种同步需求

| 问题 | 适合工具 |
| --- | --- |
| 单个整数/指针状态的无锁发布 | `AtomicBool`/`AtomicUsize`/指针原子 + 正确内存序 |
| 短临界区、不能睡眠 | 自旋锁；必要时关本地中断 |
| 临界区可能阻塞/等待 I/O | 睡眠锁/互斥锁，不能持自旋锁睡眠 |
| 等待条件变化 | 条件变量、wait queue、信号量；必须用“检查条件→睡眠”的原子协议 |

原子变量不是“加锁的整数”，锁也不是“所有内存序都自动正确”。先写状态机：谁发布、谁观察、什么时候允许回收。

## 2. 原子操作模板

```rust
// 适用环境：[no_std,K/U][RV/LA]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static READY: AtomicBool = AtomicBool::new(false);
static OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);

READY.store(true, Ordering::Release);
if READY.load(Ordering::Acquire) {
    // 现在可以观察发布前写入的普通内存
}
```

基本原则：

- `Relaxed` 只保证该原子自身的读写原子性和修改顺序，不发布周围普通内存。
- `Release` store/publish 与 `Acquire` load/observe 配对，建立 happens-before。
- `AcqRel` 用于既读取又发布的 RMW（如 CAS、`fetch_add`）。
- `SeqCst` 额外提供全局顺序，简单但可能隐藏设计问题；不要机械替换所有顺序。
- `Ordering` 只约束同一套原子同步关系，不能替代锁、设备 barrier、TLB flush 或 cache 操作。

### 2.1 CAS 选型

```rust
// 适用环境：[no_std,K][RV/LA]
if OWNER
    .compare_exchange(
        usize::MAX,
        hart_id,
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .is_ok()
{
    initialize_global_state();
}
```

失败顺序不能是 `Release`/`AcqRel`；失败只读取旧值，常用 `Acquire` 或 `Relaxed`。CosmOS 的 `try_claim_bootstrap_hart` 正是一次性选举 bootstrap hart 的例子。

`compare_exchange_weak` 允许伪失败，适合循环；单次选举使用 strong 更直观。

### 2.2 计数器与引用

```rust
// 适用环境：[no_std,K/U][RV/LA]；手写引用计数仅为内存序示意，优先使用 Arc。
let old = refs.fetch_add(1, Ordering::Relaxed);
let old = refs.fetch_sub(1, Ordering::Release);
if old == 1 {
    core::sync::atomic::fence(Ordering::Acquire);
    drop_object();
}
```

这是引用计数的典型模式，但对象发布、回收和 ABA 问题必须整体审查。优先使用 `Arc` 或项目已有封装，不要现场手写一个不完整的 `Arc`。

## 3. `Relaxed/Acquire/Release` 的直觉

```text
生产者：写 data → ready.store(true, Release)
消费者：ready.load(Acquire) == true → 读 data
```

消费者只有在读到由生产者发布的值（或其 release sequence）时，才能依赖 data 已经可见。下面是错误模式：

```rust
// 适用环境：[no_std,K/U][RV/LA]；错误示意。
data = value;
ready.store(true, Relaxed); // 不能保证其他 hart 先看到 data 再看到 ready
```

如果 `data` 之后还会被修改，Acquire/Release 也不够，需要保护 data 的锁或更复杂的无锁算法。

## 4. CosmOS 的启动同步

`os/src/main.rs` 中：

```text
bootstrap hart:
clear_bss()
→ BOOT_BSS_READY.store(0, Release)
→ 初始化 bootinfo/mm/fs/driver/task
→ BOOT_DONE.store(true, Release)

secondary hart:
BOOT_BSS_READY.load(Acquire)
→ 等待 BOOT_DONE.load(Acquire)
→ 激活本 hart kernel page table
→ 本地 trap/timer/platform 初始化
```

这里不能把 `Release` 改成普通写或随意改成 `Relaxed`：secondary hart 可能在看到 flag 后立刻访问 `.bss` 和全局对象。

## 5. 锁的分类与规则

先阅读 `os/src/sync/mod.rs`、`spin.rs`、`mutex.rs`、`sleep_mutex.rs`、`fs_sleep_mutex.rs` 的实际 API；不同锁的 guard、关中断行为和可否睡眠不一定相同。

### 5.1 自旋锁

适合很短的临界区，通常不能：

- 执行阻塞 I/O；
- 调度或等待条件变量；
- 长时间分配/回收大量页；
- 在中断可能再次获取同一锁的情况下保持中断开启。

```rust
// 适用环境：[no_std,K][RV/LA]；lock 是当前 CosmOS/内核锁的占位名。
let mut guard = lock.lock();
guard.push(item);
// guard 离开作用域后释放
```

如果锁保护的是可能在中断上下文访问的对象，要确认使用的是 `SpinNoIrqLock` 或项目对应的关中断版本。关中断只影响当前 hart，不防止其他 hart 并发访问。

### 5.2 睡眠锁

临界区可能等待 I/O、页 fault 或其他任务时使用；但睡眠锁不能在硬中断或不允许阻塞的路径使用。常见死锁：

```text
持有进程锁 → 访问用户内存 → 触发 page fault/等待 I/O
```

先确认翻译函数是否可能 fault、分配、睡眠，再决定锁范围。`os/src/syscall/utils.rs` 的用户 buffer 路径尤其需要注意。

## 6. 锁顺序和死锁排查

给锁编号并建立全局顺序，例如：

```text
全局表锁 < 进程锁 < 地址空间锁 < 文件描述符锁 < inode/page-cache 锁
```

具体顺序必须以当前 CosmOS 代码为准，以上只是记录格式。每次新增嵌套锁，检查：

1. 所有调用路径是否遵循相同顺序；
2. 是否在 guard 活着时调用外部函数；
3. 外部函数是否会回调当前对象；
4. 是否可能睡眠、中断、调度或触发 drop；
5. 锁被 panic/提前返回时是否自动释放。

死锁现象通常是一个 hart 停在自旋循环，另一个 hart 停在等待它；打印“获取锁前/后、hart、pid、锁名”比只打印函数入口有效。

## 7. Wait queue、条件变量与丢失唤醒

正确协议必须是：

```text
获取保护条件的锁
while !condition:
    将当前任务放入等待队列
    原子地释放锁并睡眠
被唤醒后重新获取锁
重新检查 condition
执行操作
释放锁
```

不能只等待一次：

```rust
// 适用环境：[no_std,K/U][RV/LA]；错误示意，sleep/use_data 为占位。
if !ready {
    sleep();
}
use_data(); // 可能是虚假唤醒或被其他消费者抢走
```

唤醒方通常应先改变条件，再唤醒等待者；并明确条件修改和入队/睡眠之间的原子性。`wake_all` 可能带来 thundering herd，但通常比丢唤醒更容易先保证正确。

## 8. 信号量与计数资源

信号量初值表示资源数量：

```text
down/P：count > 0 时减一，否则睡眠
up/V：count 加一，唤醒等待者
```

不要把 mutex 当作 count=0/1 的信号量来替代，除非 API/所有权语义明确。检查：

- `up` 是否可能溢出；
- `down` 被取消/进程退出时是否归还资源；
- 持其他锁时是否允许阻塞；
- ID 表项释放后是否可能 ABA 复用。

## 9. TLB shootdown 是另一种同步

`os/src/mm/tlb_shootdown.rs` 的目标不是普通数据锁，而是保证：

```text
页表更新
→ 记录受影响的 hart/ASID/地址范围
→ 发 IPI 或让目标 hart 轮询 mailbox
→ 目标 hart 执行本地 TLB flush
→ 必要时确认完成
→ 延迟回收旧页表/物理页
```

即使 PTE 的 Rust 写入被锁保护，其他 hart 的 TLB 仍可能缓存旧翻译。反过来，过早释放物理页会让旧 TLB 翻译指向已复用内存。审查页表题时把“锁、TLB、页生命周期”放在同一张图上。

## 10. 原子与普通字段的混用

错误模式：

```rust
// 适用环境：[no_std,K/U][RV/LA]；错误示意。
state.value = value;
state.ready.store(true, Ordering::Relaxed);
```

如果 `value` 由其他 hart 读取，应使用 Release/Acquire 发布，或把 `value` 放在同一把锁内。如果原子只用于统计计数，普通字段与它没有发布关系，可以使用 `Relaxed`。

不要让同一逻辑状态一部分通过原子读取、一部分无锁读取；这会产生 data race 或不一致快照。

## 11. 常见症状→调查路径

| 症状 | 优先检查 |
| --- | --- |
| 只在 `SMP > 1` 失败 | 共享对象生命周期、发布顺序、未关本地中断、per-hart 状态 |
| 任务偶尔丢失 | 入队/条件修改/唤醒的线性化点，是否先睡后入队 |
| 100% CPU 自旋 | 锁持有者 hart、锁顺序、是否在中断中重入 |
| 退出后 use-after-free | 仍在 run queue/TLB/等待队列/Arc 中的引用 |
| 数据看起来旧 | Release/Acquire 缺失，或需要硬件/设备 barrier |
| QEMU 卡住但无 panic | deadlock、等待条件永远不成立、IPI/定时器中断未启用 |
| 页表更新后偶发执行旧代码 | 本地/远端 TLB 或 instruction cache 同步遗漏 |

## 12. 现场 checklist

- [ ] 能画出生产者、观察者、发布点、回收点和线性化点。
- [ ] 每个原子操作的 `Ordering` 有具体理由。
- [ ] 不在自旋锁/中断路径睡眠、I/O 或执行长时间分配。
- [ ] 等待条件使用 `while`，入队、释放锁和睡眠没有窗口。
- [ ] 多架构 TLB flush、IPI 和 barrier 都通过 `hal`/arch 抽象而不是硬编码。
- [ ] 结束锁 guard 前没有调用可能重入当前对象的外部函数。
- [ ] 用 `SMP=1` 先复现功能，再用 `SMP=2/4` 验证竞态。

## 参考

- Rust 原子：<https://doc.rust-lang.org/core/sync/atomic/>
- CosmOS：`os/src/main.rs`、`os/src/sync`、`os/src/sched`、`os/src/task`、`os/src/mm/tlb_shootdown.rs`。

## 13. 按当前仓库实现的函数地图

本节是对前文速查的仓库级补充。这里的“能睡眠”是指可能进入调度器等待，不是“函数签名里出现了 sleep”。行号会随提交变化，现场用函数名和 `rg` 定位。

### 13.1 同步模块

| 路径 | 当前实现/函数 | 现场要点 |
|---|---|---|
| `os/src/sync/mod.rs` | 导出 `Condvar`、`DeadlockDetector`、futex API、`Mutex`/`MutexSpin`/`MutexBlocking`、`Semaphore`、`SleepMutex`、`SpinLock`/`SpinNoIrqLock`、`UP*Cell` | 新代码先从这里确认公开类型，不要根据旧文档猜模块名 |
| `os/src/sync/spin.rs` | `SpinLock::lock`/`try_lock`；`SpinNoIrqLock::lock`；guard `Drop` | CAS 成功 `Acquire`、失败 `Relaxed`；释放 `Release`。前者不关 IRQ，后者关本地 IRQ并 poll TLB |
| `os/src/sync/up.rs` | `UPSafeCell::exclusive_access`、`UPIntrFreeCell::exclusive_access` | 名字保留兼容性；当前实现同样用 atomic lock，不能当作单核专用无锁容器 |
| `os/src/sync/mutex.rs` | `MutexSpin::lock`、`MutexBlocking::lock`/`unlock` | `MutexSpin` 竞争时让出任务；blocking mutex 竞争时进 `WaitQueue`，不在内部 spin guard 下睡眠 |
| `os/src/sync/sleep_mutex.rs` | `SleepMutex::lock`、`SleepMutexGuard::drop` | `AtomicBool + UnsafeCell + WaitQueue`；可跨 I/O，但硬 IRQ/noirq lock 不可用 |
| `os/src/sync/fs_sleep_mutex.rs` | `fs_sleep_mutex_try_lock`、`fs_sleep_mutex_wait`、`fs_sleep_mutex_unlock` | 由 FS C ABI hook 按 lock 地址找到状态，带 waiter handoff；key 生命周期必须覆盖 FS mutex |
| `os/src/sync/condvar.rs` | `Condvar::wait`、`wait_simple`、`signal` | deprecated；`wait` 是 unlock 后再入队，不提供完整 predicate/原子转换 |
| `os/src/sync/semaphore.rs` | `Semaphore::down`、`up` | count 在 `SpinNoIrqLock` 内；`up` 后 `wake_one`，`down` 醒来必须再查 count |
| `os/src/sync/futex.rs` | `futex_wait_addr`、`futex_wake_addr`、`futex_requeue_addr`、`handle_futex_wait_timeout` | 期望值检查、入队后 recheck、timeout/signal/registry generation；用户侧 payload 仍需原子发布 |
| `os/src/sync/deadlock.rs` | `begin_request`、`finish_request`、`release`、`is_safe_state` | 诊断/准入辅助，不是 Rust 内存序，也不能证明所有内核路径无死锁 |

`SpinNoIrqLock` 自旋时会调用 `crate::mm::poll_pending_shootdown()`，因此它比普通自旋锁多了一个很强的约束：poll 路径不能分配、睡眠或递归获取会阻塞 shootdown 的锁。不要把这个行为复制到自定义 spinlock 中。

### 13.2 等待队列、调度与唤醒

| 路径 | 函数/字段 | 读法 |
|---|---|---|
| `os/src/task/wait_queue.rs` | `prepare_to_wait` → `wait_with_reason_or_skip` → `block_prepared` → `finish_wait` | 入队、重查、阻塞和清理是一套协议，不要只复用 `wake_one` |
| 同上 | `wake_up_to_with`、`wake_waiter_by_ptr` | 先从队列取出/校验 waiter，再进入 `wakeup_task`；raw handle 可能需要重定向到当前 queue |
| 同上 | `wake_and_requeue_with` | 源/目标 queue 按地址顺序拿锁，移动 waiter 时更新 `current_wq_handle` |
| `os/src/sched/api.rs` | `block_current_and_run_next` | `LocalIrqSave` 覆盖取 current、提交阻塞状态、切换的窗口，防止同 hart IRQ 看到半完成状态 |
| 同上 | `suspend_current_and_run_next` | 仅让出 CPU 的路径；不能把所有“等待”都换成它 |
| `os/src/sched/runqueue.rs` | `wakeup_task`、`resolve_on_cpu_wake`、`enqueue_task_on` | 结合 `task_status`、`on_cpu`、`sched.on_rq`，避免丢 Runnable 或重复入队 |
| `os/src/sched/processor.rs` | `current_task`、`take_current_task`、`finish_pending_task_release`、`schedule` | 可选 current cache 用 pointer Release/Acquire；切换结束才 `on_cpu.store(false, Release)` |
| `os/src/task/task.rs` | `TaskControlBlock::inner`、`on_cpu`、`return_work`、`cpu_accounting_stamp` | task-inner 保护多数普通字段；`on_cpu` 是远端唤醒等待切换的同步点 |
| `os/src/task/mod.rs` | `wake_signal_waiters`、`notify_parent_child_exit`、teardown/quiesce 路径 | signal/exit 要清理 wait handle/futex/timer，并按 PCB→task 反向锁序拆分作用域 |

一个 waiter 是否“已经被唤醒”不能只看 `task_status`。至少同时观察：

```text
task_status       = Interruptible / Uninterruptible / Runnable / Running / Zombie
task.sched.on_rq  = 是否在某个 runqueue
task.on_cpu       = 是否仍被某个 hart 持有；远端 wake 要 Acquire 等待 false
current_wq_handle = 是否仍挂在本队列，或已被 requeue/清理
```

### 13.3 中断与 TLB

| 路径 | 关键 API | 约束 |
|---|---|---|
| `os/src/trap/irq.rs` | `HardIrqGuard`、`can_sleep`、`enter_noirq_lock`/`exit_noirq_lock` | hardirq 或 noirq depth 非零时不能进入会阻塞的 lock |
| `os/src/hal/mod.rs` | `LocalIrqSave`、`local_irqs_enabled`、`disable_local_irqs`/`enable_local_irqs` | 只管理当前 hart 的 IRQ，不等价于跨 hart fence |
| `os/src/trap/mod.rs` | trap 入口 `poll_pending_shootdown`、timer/software/external IRQ | IPI/IRQ 慢路径在 hardirq 上下文，不能睡眠 |
| `os/src/mm/tlb_shootdown.rs` | `shootdown_inner`、`service_pending_shootdown_quiet`、`poll_pending_shootdown` | mailbox Release/Acquire + per-hart CAS + ack；架构 TLB flush 另行执行 |
| `os/src/mm/memory_set.rs` | `mark_user_active`、`advance_tlb_generation`、`shootdown_user_harts`、`DeferredUserReclaim::flush_then_release` | generation/active hart 与延迟页帧回收必须连起来 |

## 14. 内存序和 fence 的严谨速查

### 14.1 每种操作允许的 ordering

```text
load:       Relaxed / Acquire / SeqCst
store:      Relaxed / Release / SeqCst
RMW:        Relaxed / Acquire / Release / AcqRel / SeqCst
CAS success:以上 RMW 序均可
CAS failure:Relaxed / Acquire / SeqCst（不可 Release/AcqRel）
fence:      Acquire / Release / AcqRel / SeqCst
```

`Acquire` 不是“读到最新值”，`Release` 也不是“立刻刷新所有 cache”。它们只有在同一原子对象上形成对应读写关系时，才把此前/此后的普通内存访问串起来。读到旧值时，Acquire 可能仍然是合法的；算法必须允许重试或使用明确的状态/代数。

| 场景 | 最小常见选择 | 不要这样解释 |
|---|---|---|
| 统计计数、只作诊断的最大值 | `Relaxed`（CAS 也可 Relaxed） | “Relaxed 一定全局一致” |
| 初始化后发布不可变对象 | 写对象后 `store(Release)`，读 flag `load(Acquire)` | “flag 原子，所以对象字段也原子” |
| 解锁/加锁 | unlock `Release`，lock 成功 `Acquire` | “只要 lock 位 CAS 原子，guard 内普通字段就自动正确” |
| 抢 owner 并读 owner 发布的数据 | CAS 成功 `Acquire`/`AcqRel` | 成功 CAS 用 `Relaxed` 却读取未受锁保护的数据 |
| 清除并取得一个 pending bit | `fetch_and(AcqRel)` 或按协议选择 | 把多个 bit 当成独立事件，却没有代数/优先级规则 |
| 单一全局顺序的算法 | `SeqCst` | 用 SeqCst 修复 data race、死锁或 TLB 旧翻译 |

### 14.2 fence 的两个容易混淆的层次

```rust
// [no_std,K/U][RV/LA]：普通内存发布的示意，直接 Release/Acquire 通常更好读。
use core::sync::atomic::{fence, AtomicBool, Ordering};

static PUBLISHED: AtomicBool = AtomicBool::new(false);

fn publish(payload: &mut usize) {
    *payload = 42;
    fence(Ordering::Release);
    PUBLISHED.store(true, Ordering::Relaxed);
}

fn observe(payload: &usize) -> Option<usize> {
    if !PUBLISHED.load(Ordering::Relaxed) {
        return None;
    }
    fence(Ordering::Acquire);
    Some(*payload)
}
```

此代码的完整正确性仍取决于 `payload` 只初始化一次或有额外生命周期/互斥。实际项目优先写成：

```rust
// [no_std,K/U][RV/LA]
payload_write();
PUBLISHED.store(true, Ordering::Release);

if PUBLISHED.load(Ordering::Acquire) {
    payload_read();
}
```

`compiler_fence` 只限制编译器重排，适合极特殊的同 hart 中断/信号协议；它不是跨 hart 硬件同步。RISC-V 的 `fence` 指令、`sfence.vma`、`fence.i` 和 LoongArch 的 `dbar`、`ibar`、`invtlb` 分属普通内存、指令可见性和地址翻译语义，不能互换。

### 14.3 CAS 的失败路径、ABA 与所有权

```rust
// [no_std,K/U][RV/LA]：weak CAS 必须循环；失败 ordering 只描述失败读取。
use core::sync::atomic::{AtomicUsize, Ordering};

fn increment_below(value: &AtomicUsize, limit: usize) -> bool {
    let mut old = value.load(Ordering::Relaxed);
    loop {
        if old >= limit {
            return false;
        }
        match value.compare_exchange_weak(
            old,
            old + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => old = observed,
        }
    }
}
```

检查点：

- `compare_exchange_weak` 即使没有其他线程修改也可能失败；不循环是错误。
- `compare_exchange` strong 也只是不允许伪失败，不代表公平或无饥饿。
- 指针从 A 变 B 再变回 A 是 ABA；加 generation/tag，或采用锁、Arc、hazard/epoch 等生命周期方案。
- `AtomicPtr` 只原子地保存地址；它不延长对象寿命。CosmOS current-task cache 手动增加 strong count 后才 `Arc::from_raw`，不要照抄成一次裸 `from_raw`。
- `fetch_add/sub` 可能回绕；引用、hart mask、waiter 数量要有边界和错误路径。

## 15. CosmOS 等待协议的可复制模板

### 15.1 WaitQueue：条件而不是唤醒本身

```rust
// [no_std,K][RV/LA]：CosmOS 风格；实际模块需引入 WaitReason 和合适的数据锁。
loop {
    {
        let guard = data_lock.lock();
        if guard.condition_is_true() {
            // 在同一保护协议下消费/改变条件。
            drop(guard);
            break;
        }
        drop(guard); // 绝不能持 data/inner guard 进入 wait
    }

    wait_queue.wait_with_reason_or_skip(
        WaitReason::Mutex,
        || {
            let guard = data_lock.lock();
            guard.condition_is_true()
        },
    );
}
```

这是可套用的结构模板，但 `data_lock`、`condition_is_true` 是占位符；可直接使用的当前 reason 变体包括 `Mutex`、`Semaphore`、`Condvar`、`Futex(addr, expected)` 等，见 `os/src/task/task.rs::WaitReason`。调用者要确保 predicate 与生产者修改条件使用同一个锁或同一个原子协议。

生产者的顺序：

```text
[K] 持数据锁/使用原子协议修改 condition
[K] 完成 Release 或释放数据锁
[K] 调用 wake_one/wake_all（不持会造成反向调用的 queue guard）
```

`WaitQueue` 内部已经实现“prepare_to_wait → recheck → block”；如果新代码自己在 `push_back` 后直接 schedule，会重新引入丢失唤醒。

### 15.2 blocking mutex 与 semaphore 的共同形状

```text
// [no_std,K][RV/LA]：伪代码，展示锁作用域而非可直接编译的类型。
loop {
    lock(inner)
    if resource_available {
        consume_or_mark_locked()
        unlock(inner)
        return
    }
    unlock(inner)
    wait_with_reason_or_skip(predicate)
}
```

CosmOS 的 `MutexBlocking` 以 `locked: bool` 表示资源；`Semaphore` 以 `count > 0` 表示资源。两者都不能在 `inner` 的 `SpinNoIrqLock` guard 仍存活时阻塞。`up`/`unlock` 修改资源后再 wake，醒来的任务重新竞争，不能假设它一定获得资源。

### 15.3 futex 用户/内核两侧

```rust
// [no_std,U][RV/LA]：概念模板；futex_wait/futex_wake 是 syscall wrapper 占位名。
use core::sync::atomic::{AtomicU32, Ordering};

fn wait_ready(word: &AtomicU32) {
    loop {
        if word.load(Ordering::Acquire) == READY {
            return;
        }
        futex_wait(word, WAITING); // 内核仍会检查 expected；返回后无论原因都重查
    }
}

fn publish_ready_and_wake(word: &AtomicU32) {
    word.store(READY, Ordering::Release);
    futex_wake(word, 1);
}
```

当前 `futex_wait_addr` 的内核步骤是：先读用户 word，值不等 expected 返回 `EAGAIN`；注册 queue/timeout 后，入队 predicate 再读用户 word；醒来后区分 Ready、TimedOut、Canceled 和 signal。用户 word 的原子性和 payload 发布不由 syscall 自动提供。

## 16. 中断、SMP 调度和 TLB shootdown 的联动

### 16.1 IRQ-safe 与 sleep-safe 的选择

| 当前上下文 | 可用方向 | 禁止/注意 |
|---|---|---|
| 硬中断、软件 IPI、timer handler | 原子 bit、短 `SpinNoIrqLock`、非阻塞 wake/投递 work | `SleepMutex`、`MutexBlocking`、condvar、semaphore down、WaitQueue wait |
| 任务但持有 noirq spin guard | 只做极短内存操作；必要时让 guard 尽快 Drop | I/O、调度、分配、同步等 TLB ack；不能手动重新 enable IRQ |
| 普通内核任务、可能等待 block I/O | `SleepMutex`、`MutexBlocking`、WaitQueue | 进入 wait 前释放 PCB/task/page/queue 等短锁 |
| scheduler block/switch 窗口 | 按 `block_current_and_run_next` 使用 `LocalIrqSave` | 不要让同 hart IRQ 看到“已取 current 但还未提交阻塞”的中间状态 |
| 用户态 syscall 入口 | 按 `UserSyscallIrqGuard`/当前 syscall 约定 | 不要把用户态允许 IRQ 推广到 hardirq/noirq 路径 |

本地 IRQ mask 只能阻止当前 hart 的重入；SMP 下另一 hart 仍能访问共享对象，所以共享对象的 owner/状态仍需 Atomic 或锁。

### 16.2 scheduler race 的检查顺序

遇到“唤醒了但没有运行”或“任务在两个队列”，按这个顺序记录，不要先改 ordering：

1. 当前 hart 和远端 hart 的 `Processor.current`；若启用 `current_task_cache`，检查 `CURRENT_TASK_PTRS` 的 Acquire/Release 发布。
2. 任务 `task_status`：`Interruptible/Uninterruptible` 是否已转为 `Runnable`。
3. `sched.on_rq` 和实际 runqueue 中的条目数量。
4. `on_cpu`：是否仍在切换；远端 wake 是否按 Acquire 等到 post-switch Release。
5. `current_wq_handle`：waiter 是在原队列、已 requeue，还是已清理。
6. 是否在 `block_current_and_run_next` 的 LocalIrqSave 之外手写了阻塞状态转换。

`runqueue.rs::wakeup_task` 已处理 `Running`/`Zombie` 等边界；新路径优先调用它，不要只改 `task_status` 或只 `push_back`。

### 16.3 TLB mailbox 的最小心智模型

```text
launch lock
  → 写 kind/参数/target
  → 清 ack
  → 发布 seq/active
  → 本地 flush + IPI
  → 等 Acquire 读取所有目标 ack
  → active=false，清理 mailbox

目标 hart：
  Acquire 观察 active/seq/target
  → per-hart CAS claim 一次 seq
  → Acquire 读取 payload
  → 执行本地架构 TLB flush
  → AcqRel 设置 ack bit
```

RISC-V 使用 `sfence.vma` 族包装；LoongArch 使用 `dbar`/`invtlb`/`ibar`，当前 HAL 对 ASID/range 可能保守回退为全 flush。不要因为两边都有 `flush_tlb_asid` 名字就假设实现相同。

地址空间变更还要检查 `MemorySet::tlb_generation`、`active_user_harts`、每-hart `seen_tlb_generation` 和 `DeferredUserReclaim`。只有等 shootdown 后才释放旧用户页/页表，不能只看 Rust 引用计数降为零。

## 17. 引用计数、任务唤醒与资源回收

### 17.1 `Arc` 保护什么

```text
Arc<T>：原子地管理 T 的 strong/weak 引用计数和对象寿命
Arc<T> 不等于：T 内普通字段可无锁并发读写
```

当前路径中的典型组合：

- `Arc<TaskControlBlock>` 在 WaitQueue 中保活排队任务；waker 取出后再交给 `wakeup_task`。
- `Arc<SpinNoIrqLock<...>>` 用于 page cache 等短临界区，page state 与 waiter 一起存活。
- `Weak` 用于 cache/process 等反向引用，`upgrade()` 失败时表示对象已结束，不应 unwrap 成“必然存在”。
- current task cache 使用 raw pointer，但权威 `Arc` 在 Processor；手动 strong count 是特定不变量，不是通用 AtomicPtr 模板。

### 17.2 page cache / block completion 的模式

`os/src/fs/page_cache.rs` 的 page state 通常是 `Arc<SpinNoIrqLock<CachePage>>`：loader 在 page lock 内设置 `LOADING`/pin，其他任务在 predicate 中等待 `UPTODATE || !LOADING`；loader 完成后清 `LOADING`、置 `UPTODATE`、减 pin，再 `wake_all`。检查时必须同时看 page lock、wait queue 和 Arc 是否还持有 page。

block completion worker 的状态位使用 `BLOCK_COMPLETION_WORK_PENDING.store(true, Release)`，worker 以 `swap(false, AcqRel)` 消费，WaitQueue predicate 用 Acquire。这是“原子 pending hint + wait queue”的完整例子：hint 发布工作存在，queue 负责睡眠，worker 醒来后仍要重新检查队列。

### 17.3 退出/信号/延迟释放的顺序

`task/mod.rs` teardown 会先 snapshot process task 集合，释放 process-inner，再处理 task-inner；还会移除 wait handle、futex/timer 等外部引用，等待远端 `on_cpu.load(Acquire)`，然后才进入 stopping/release 路径。不要在 process-inner guard 内调用可能拿 task-inner 的清理函数。

用户地址空间回收还必须先 TLB shootdown 再 Drop `UserReleaseBatch`。引用计数只说明 Rust owner 数量，不说明远端 hart 的 TLB 中没有旧翻译。

## 18. 现象 → 原因 → 检查 → 修复：扩展排查表

| 现象 | 优先怀疑 | 检查证据 | 修复方向 |
|---|---|---|---|
| 所有 hart 卡住，日志停在 TLB ack | target hart IRQ 关闭、noirq 临界区过长、IPI 未服务、payload/ack 顺序破坏 | 记录 seq、target mask、ack mask、missing hart；查该 hart 是否在 `SpinNoIrqLock` 竞争和 `poll_pending_shootdown` | 缩短 noirq 区；保持 poll allocation-free；修正 mailbox Release/Acquire、per-hart CAS 与 ack；不要只改成 SeqCst |
| 单 hart 取得锁后永远自旋 | `SpinLock` 没关本地 IRQ而被 handler 重入 | 查同一锁是否在 `trap/irq`、console、IPI 路径出现；查持有者是否被中断 | 改 `SpinNoIrqLock` 或 IRQ 只置位/投递 work；缩短 guard |
| 自旋 CPU 100%，owner 不变 | owner 被抢占、关 IRQ、退出未清 owner；把 `MutexSpin` 用在无 current task 路径 | 打印 owner/current hart、`task_status`、`on_cpu`、IRQ/noirq depth | 可阻塞上下文改 blocking/sleep lock；修复 owner 生命周期；增加诊断，不以盲目放宽 ordering 代替修复 |
| waiter 永久睡眠或偶发 timeout | 条件修改与入队没有线性化；wake 早于入队且没有 recheck；stale handle | 记录 `WaitReason`、queue 长度、`current_wq_handle`、predicate 前后值；futex 记录 expected/generation/timeout state | 使用 `wait_with_reason_or_skip`/keyed skip；条件先 Release/锁内修改；唤醒后循环 |
| Condvar 偶发丢 signal | deprecated `Condvar::wait` 的 unlock→enqueue 窗口 | 对照 `os/src/sync/condvar.rs`：是否依赖裸 `wait`，生产者是否可在窗口 signal | 共享 predicate + while；迁移 WaitQueue skip/futex |
| futex `EAGAIN` 很多 | wait 前 word 已变化；不是 bug，表示 expected 不匹配 | 记录用户 word 和 expected；检查是否在 syscall 前重读、是否有其他消费者 | 按循环重试；用 AtomicU32/锁保护协议，不把 EAGAIN 当成功睡眠 |
| futex `EINTR`/`ETIMEDOUT` 后状态错乱 | 忽略 signal/timeout 返回；超时 registry 清理不完整 | 查 `handle_futex_wait_timeout`、slot state、deadline、signal pending | 先清理/重查 predicate，再决定重试或向用户返回；不要继续使用已取消 wait handle |
| task Runnable 但无进展、重复入队、sched invariant 报错 | `on_cpu`/`on_rq`/status 更新 race，绕过 scheduler API | 开 `sched_invariant_checks`；记录四状态、当前 hart、runqueue 条目；查 repair 日志 | 用 LocalIrqSave 的 block API；统一 `wakeup_task`/enqueue API；远端 Acquire 等切换完成 |
| PCB/task/queue 三方死锁 | 锁顺序反转、wake/回调仍持 queue guard、持锁进入调度 | 从调用栈画 A→B；特别找 PCB→task、task→processor、queue→wakeup | snapshot Arc 后释放外层锁；取出 waiter 后释放 queue guard；拆作用域 |
| 读到旧 payload/状态不一致 | Relaxed 被当成发布；读写普通字段绕锁；对象地址复用 | 列出每个字段所有 reader/writer，标记 lock 或同步 atomic；检查 generation | 同一把锁或同一 flag 的 Release/Acquire 链；加入版本/生命周期协议 |
| 退出后 UAF/double free | raw pointer/WaitQueueHandle/Arc::from_raw 不保活；页在 TLB flush 前回收 | 查 raw pointer 所有者、Arc strong count、Weak upgrade、`on_cpu`、`DeferredUserReclaim` | 用 Arc/Weak/锁/代数；清理 handle；等待远端切换和 shootdown |
| signal/child exit 后 wait4/futex 不醒 | hint 发布但没 wake；handle 被 requeue/teardown 清除；process/task 反向锁 | 查 `wake_signal_waiters`、`notify_parent_child_exit`、`return_work` 的 Release/Acquire 和 wait handle | 先发布状态再对应 wake；按当前 handle 定向唤醒；按 teardown 锁序清理 |
| 页表更新后仍执行旧映射/旧指令 | 把普通 fence 当 TLB flush；遗漏远端 hart 或过早释放页 | 查 generation/active mask/seq/ack；区分 `sfence.vma`/`invtlb` 与 `atomic::fence` | 按 hal/arch wrapper flush；完成 shootdown 后再回收；RV/LA 分别验证 |
| 只在 LA 失败或性能异常 | 假设 LA 有 RV 同样的 ASID/range 精确 flush；硬编码 RV 指令 | 查 `os/src/arch/loongarch64/paging.rs` 和 HAL fallback | 使用抽象 API；接受当前保守全 flush，单独优化前先证明语义 |
| console/panic 路径卡住 | console atomic lock 被 IRQ/递归日志重入；持 noirq 做长输出 | 查 `os/src/console.rs` 的 IRQ 保存、调用栈和 owner | 保持 console lock 的 IRQ 协议；减少持锁输出；避免日志递归 |

## 19. 离线构建、检索与 Markdown 验证

### 19.1 仓库 pin 与目标

当前 `rust-toolchain.toml` 固定 `nightly-2025-01-18`，内核默认目标 `riscv64gc-unknown-none-elf`；`user/rust-toolchain.toml` 使用同一 nightly。`os/.cargo/config.toml`/`user/.cargo/config.toml` 还设置 linker、frame pointer 和目标相关 flags。LoongArch 目标由 Makefile 切换为 `loongarch64-unknown-none`。

```sh
# [宿主 shell][RV/LA]：先确认当前实际工具链，离线环境不会下载。
rustc --version
rustup run nightly-2025-01-18 rustc --version
rustup target list --installed
```

主构建优先沿用仓库 Makefile：

```sh
# [宿主 shell][RV]：SMP=4 进行并发验证。
make -C os kernel ARCH=riscv64 SMP=4

# [宿主 shell][LA]：需要本机已有目标、链接器和 QEMU。
make -C os kernel ARCH=loongarch64 SMP=4

# [宿主 shell][RV]：根 Makefile 路径。
make all BUILD_ARCH=rv SMP=4 KEEP_SDCARD=1
make run RUN_ARCH=rv SMP=4
```

直接 Cargo check 可作为类型/feature 诊断，但不一定代替链接和镜像流程：

```sh
# [宿主 shell][RV]：仅在依赖已缓存、目标已安装时使用；--offline 不下载。
cargo +nightly-2025-01-18 check --offline \
  --manifest-path os/Cargo.toml \
  --target riscv64gc-unknown-none-elf \
  --no-default-features \
  --features 'ext4,platform-qemu-virt,legacy-vdb-names,trap_context_cache,process_identity_cache,return_work_cache,current_task_cache'
```

`os/Makefile` 的默认 `EXTRA_FEATURES` 还会涉及 perf probe/缓存等配置。不要用一条自定义 Cargo 命令推断“完整 Makefile 构建成功”；可先运行：

```sh
# [宿主 shell]：只打印，不构建；用于核对最终 target、linker、feature。
make -C os -n kernel ARCH=riscv64 SMP=4
```

### 19.2 快速检索与 scope 检查

```sh
# [宿主 shell]：只读检索。
rg -n 'Atomic|Ordering::|compare_exchange|fetch_|fence|SpinNoIrqLock|SleepMutex|MutexBlocking|Condvar|Semaphore|WaitQueue|wakeup_task|block_current|shootdown|on_cpu|on_rq' \
  os/src fs/src user/src

# [宿主 shell]：检查本资料 Markdown 的围栏、空白和标题。
rg -n '^#{1,6} |^```' docs/final_prepare/06-rust-atomics-locks-memory-ordering.md
git diff --check -- docs/final_prepare/06-rust-atomics-locks-memory-ordering.md

# [宿主 shell]：目标是新增未跟踪文件时，git diff 默认不显示它；必须同时看 status。
git status --short
git diff --name-only
git diff -- docs/final_prepare/06-rust-atomics-locks-memory-ordering.md
```

前文未逐块写标签的 `core` 示例按 `[no_std,K/U][RV/LA]` 理解；`std::sync` 示例按 `[std,U/宿主测试]` 理解；shell 命令按 `[宿主 shell]` 理解。新增代码块均在本附录中显式标注。`futex_wait`/`futex_wake` 仅是用户 syscall wrapper 占位名，不能当成当前 `user` crate 已导出的函数名。

## 20. 交付前的并发专项 checklist

### 原子与内存序

- [ ] 每个共享字段已经标出唯一 authoritative owner：锁、Atomic 状态机或 immutable 发布对象。
- [ ] `Relaxed` 只用于计数/提示或已由同一把锁保护的数据；发布数据使用同一原子对象的 Release/Acquire 链。
- [ ] CAS failure 没有 `Release`/`AcqRel`；weak CAS 在循环；检查 ABA、generation、溢出和对象寿命。
- [ ] `SeqCst` 没被用来替代互斥、IRQ mask、TLB flush、生命周期或死锁修复。
- [ ] `fence`、`compiler_fence`、架构 TLB/设备 barrier 的层次已经区分。

### 锁与等待

- [ ] 已按上下文选择 `SpinLock`、`SpinNoIrqLock`、`MutexBlocking`、`MutexSpin`、`SleepMutex` 或 WaitQueue；确认是否允许睡眠。
- [ ] 可能被本地 IRQ 重入的 `SpinLock` 已改为 IRQ-safe 版本或证明 IRQ 不取该锁。
- [ ] 不持 PCB/task/queue/page/noirq guard 进入 wait、schedule、I/O、TLB ack、可能分配/递归日志。
- [ ] 等待使用“检查→入队→重查→阻塞”；唤醒后循环；生产者先改变条件再 wake。
- [ ] 旧 Condvar 使用了外部 predicate/while，新增代码优先 skip/predicate WaitQueue 或 futex。
- [ ] Semaphore down 醒来重查 count；futex 正确处理 EAGAIN/EINTR/ETIMEDOUT。

### scheduler、SMP、TLB 与回收

- [ ] 用 `SMP=2/4` 验证过，不只用默认 `SMP=1`。
- [ ] block/wake 路径保留 `LocalIrqSave` 的原子窗口；`on_cpu` post-switch Release、远端 Acquire。
- [ ] 没有重复 runqueue、双 hart current、stale wait handle；必要时开 `sched_invariant_checks`。
- [ ] TLB mailbox 的 target/seq/payload/ack 发布和 claim 顺序完整；poll 路径无睡眠/分配/递归锁。
- [ ] RISC-V 与 LoongArch 分别走 hal/arch flush；没有把 `sfence.vma`/`invtlb` 当 atomic fence。
- [ ] 旧页/页表/用户栈释放在 shootdown 和远端切换完成后；Arc/Weak/raw handle 生命周期有明确 owner。

### 环境与交付

- [ ] `rustc --version`、目标、linker、QEMU 和仓库 nightly pin 已记录；离线依赖缓存已确认。
- [ ] 已用 Makefile 或等价完整命令构建；Cargo check 失败时区分 target/依赖/链接环境问题和源码问题。
- [ ] `git diff --check` 通过，Markdown 代码围栏成对，命令中的 feature 与当前 `Cargo.toml`/Makefile 一致。
- [ ] `git status --short` 确认只新增/修改本指定资料文件；没有触碰其他 worker 的文件。
