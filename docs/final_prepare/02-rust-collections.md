# Rust 容器与数据结构速查（CosmOS 现场版）

这份资料按当前仓库实现整理，目标是断网时能快速回答三个问题：

1. 这批数据应该用哪一种容器？
2. 这个容器在 CosmOS 的内核/用户态是否能用、会不会分配堆内存？
3. 代码已经拿到一个借用、锁或迭代器后，怎样安全地查找、删除和改写？

文中的复杂度只计算容器操作本身，`n` 是容器元素数，`k` 是输出数；不包括堆分配、锁竞争、页表映射和设备 I/O。除非特别说明，复杂度是摊销或平均意义，不能当作硬实时上界。

## 0. 先记住 CosmOS 的编译边界

### 0.1 当前仓库事实

| 组件 | 源码/配置 | 现场应记住的事实 |
| --- | --- | --- |
| 内核 `os` | [`os/src/main.rs`](../../os/src/main.rs)、[`os/Cargo.toml`](../../os/Cargo.toml) | `#![no_std]`，显式 `extern crate alloc`；可以使用 `core` 和 `alloc`，不能直接使用 `std`。 |
| 内核堆 | [`os/src/mm/heap_allocator.rs`](../../os/src/mm/heap_allocator.rs)、[`os/src/config.rs`](../../os/src/config.rs) | 有自定义 `#[global_allocator]`；页大小为 4096，`MAX_KERNEL_HEAP_SIZE = 0x4000_0000` 是配置上限，初始/扩容和实际可用内存仍受物理内存与分配器状态约束。 |
| 内核哈希表 | [`os/Cargo.toml`](../../os/Cargo.toml)、[`os/Cargo.lock`](../../os/Cargo.lock) | 直接依赖锁定为 `hashbrown = 0.12.3`，锁文件中的 `ahash` 为 `0.7.8`。内核实际使用 `hashbrown::HashMap/HashSet`，不是 `std::collections::HashMap`。 |
| 文件系统库 `fs` | [`fs/src/lib.rs`](../../fs/src/lib.rs)、[`fs/Cargo.toml`](../../fs/Cargo.toml) | `#![no_std] + alloc`，被内核以路径依赖使用；其中缓存和 VFS 大量使用 `BTreeMap`、`VecDeque`、`Vec`。 |
| 用户库 | [`user/src/lib.rs`](../../user/src/lib.rs) | `#![no_std] + alloc`，用户程序启动时在 `__user_start` 中初始化 `buddy_system_allocator` 的全局堆，然后才解析 `argv` 和调用 `main`。 |
| 用户堆 | [`user/src/lib.rs`](../../user/src/lib.rs) | `USER_HEAP_SIZE = 128 * 1024`；这是用户库当前静态堆空间，`Vec`、`String`、`Box` 等动态对象都要从这里分配。 |
| 构建目标 | [`rust-toolchain.toml`](../../rust-toolchain.toml)、[`os/.cargo/config.toml`](../../os/.cargo/config.toml) | RISC-V 目标是 `riscv64gc-unknown-none-elf`；LoongArch 目标是 `loongarch64-unknown-none`。容器本身与指令集无关，但编译器、链接脚本和依赖缓存与目标有关。 |
| 固定版本 | [`rust-toolchain.toml`](../../rust-toolchain.toml) | 仓库声明 `nightly-2025-01-18`；不要把现场机器上“最新 nightly”的 API、Cargo 行为或依赖特性当成仓库事实。 |
| 宿主工具 | [`fs-fuse/Cargo.toml`](../../fs-fuse/Cargo.toml) | `fs-fuse` 是宿主侧工具，使用 `std`/`clap`/`fatfs`；它能用 `std::collections`，但这不代表内核和用户态能用。 |

容器属于 Rust 语言/库层，RISC-V 与 LoongArch 的选择规则相同。真正需要区分架构的通常是 syscall 汇编、启动入口、链接脚本和设备寄存器；不要给容器代码加入无必要的 `#[cfg(target_arch = ...)]`。

### 0.2 导入模板

```rust
// 适用环境：内核/文件系统，no_std + alloc；RISC-V/LoongArch 均适用。
// os/src/main.rs 或 fs/src/lib.rs 已经声明 extern crate alloc；普通模块只需这些 use。
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet}; // 只有需要哈希表时才导入；来源是 os/Cargo.toml。
```

```rust
// 适用环境：用户态 no_std + alloc；RISC-V/LoongArch 均适用。
// 每个 user/src/bin/*.rs 通常需要自己声明 extern crate alloc。
#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
```

```rust
// 适用环境：宿主侧 std；仅适用于 fs-fuse 或普通主机测试，不可复制到 os/user。
use std::collections::{BTreeMap, HashMap, VecDeque};
```

`alloc::collections` 里有 `BTreeMap`、`BTreeSet`、`BinaryHeap`、`LinkedList`、`VecDeque` 等，但没有可直接替代 `std::collections::HashMap` 的标准库哈希表；no_std 内核应使用仓库已经配置的 `hashbrown`，或明确引入并锁定另一种 no_std 哈希器/哈希表。

## 1. 一页选择表

| 类型 | 内存/顺序 | 常用操作复杂度 | 适合 CosmOS 的场景 | 最容易踩的坑 |
| --- | --- | --- | --- | --- |
| `[T; N]` 数组 | 内联固定 `N` 个元素；栈上、静态区或结构体内 | 索引 O(1)，线性查找 O(n) | 固定寄存器快照、页表/启动信息上限、固定队列 | `N` 是编译期长度；索引越界 panic；`[x; N]` 的重复初始化有 `Copy`/const 约束。 |
| `&[T]` / `&mut [T]` 切片 | 不拥有数据，只借用连续内存；运行时长度 | 索引 O(1)，遍历 O(n) | syscall buffer、设备 I/O、解析网络/磁盘字节 | 切片不能延长；`split_at_mut` 只能按不重叠区间借用；`&mut` 借用期间不能再改原容器。 |
| `Vec<T>` | 堆上连续、拥有、可增长；保持插入顺序 | `[]`/`get` O(1)，末尾 `push/pop` 摊销 O(1)，`insert/remove` O(n)，`swap_remove` O(1)，`retain` O(n) | 文件数据、页框列表、argv、目录快照、网络缓冲 | `capacity` 不是长度也不是硬上限；`remove(0)` 是 O(n)；扩容可能分配/失败。 |
| `VecDeque<T>` | 堆上可增长环形缓冲；逻辑上从 front 到 back | 两端 `push/pop` 摊销 O(1)，索引 O(1)，中间插入/删除通常移动较近一侧 | 等待队列、就绪队列、TTY/console、socket 接收队列 | 底层可能分成两段，不能假定一个连续切片；`with_capacity` 仍不是硬容量限制。 |
| `BTreeMap<K,V>` | `alloc` 中的有序树；按 key 排序遍历 | 查找/插入/删除 O(log n)，有序遍历 O(n)，范围遍历约 O(log n + k) | VMA、PID/设备注册表、页缓存、mount/epoll 表 | `K: Ord`；遍历时不能直接删除；有序但节点分配多，不是无分配容器。 |
| `hashbrown::HashMap<K,V>` | 哈希表；无稳定遍历顺序 | 平均查找/插入/删除 O(1)，最坏 O(n)，遍历通常与容量相关 | futex key→wait queue、按键等待者 | `K: Eq + Hash` 且相等 key 必须有相等 hash；不要依赖迭代顺序；版本/hasher/特性必须跟 Cargo.lock 一致。 |
| `Option<T>` | `Some(T)` 或 `None`；不是自动增长集合 | 匹配/取值 O(1) | 可选 page cache、可选地址、fd 表槽位、查找成功/失败 | `None`、空 `Vec` 和“已知负结果”语义不同；需要三种状态时使用 enum。 |
| `[Option<T>; N]` / 固定环 | 内联、有界、无堆分配；槽位可空 | 位置 O(1)，扫描 O(N) | 固定数量探针、fd/对象槽位、用户态连接队列 | 必须显式处理满/空；固定环的 head/tail/len 不变量一旦错就会覆盖数据或越界。 |
| `heapless::Vec<T,N>` 等 | 编译期容量，通常内联；满时返回 `Err` | 视具体类型；Vec 索引 O(1)，push O(1) | 真正需要硬容量上限的设备/协议缓冲 | 本仓库 `os` 没有直接声明 `heapless`；它是 vendor/smoltcp 的依赖，不能因为出现在 Cargo.lock 就直接 `use`。 |

经验法则：

- “拥有一段连续、会增长的字节/对象”选 `Vec`。
- “从两端进出、FIFO 或环形缓冲”选 `VecDeque`。
- “需要按 key 排序、范围扫描或可复现顺序”选 `BTreeMap`。
- “只关心 key 是否存在，且平均查找速度优先”选 `hashbrown::HashMap`。
- “数量在编译期已知且不能分配”选数组/手写固定环；“需要库级固定容量语义”才考虑 `heapless`。
- “值可能不存在”用 `Option<T>`，但不要用 `Option` 掩盖错误状态或把 `None` 与空集合混为一谈。

## 2. 数组、切片和 `Option`：所有容器的底座

### 2.1 数组是内联值，切片是借用视图

数组的长度在类型中：`[u8; 512]` 与 `[u8; 4096]` 是不同类型。切片 `[u8]` 没有固定长度，`&[u8]`/`&mut [u8]` 是带长度的借用视图。syscall、磁盘和网络接口通常应该接收切片，而不是接收 `Vec`，这样调用者可以传数组、`Vec` 的切片或映射区间。

```rust
// 适用环境：内核/FS/用户态 no_std + core/alloc；RISC-V/LoongArch 均适用。
fn copy_prefix(dst: &mut [u8], src: &[u8]) -> usize {
    let n = core::cmp::min(dst.len(), src.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

fn read_one(buf: &[u8], index: usize) -> Option<u8> {
    buf.get(index).copied() // 越界返回 None，不会像 buf[index] 那样 panic。
}

fn split_for_two_consumers(buf: &mut [u8], split: usize) {
    let (left, right) = buf.split_at_mut(split); // split > len 会 panic。
    if let Some(first) = left.first_mut() {
        *first = first.wrapping_add(1);
    }
    if let Some(last) = right.last_mut() {
        *last = last.wrapping_add(1);
    }
}
```

边界规则：

- `array[index]`、`slice[index]` 越界会 panic；输入来自用户或磁盘时先比较 `len`，或使用 `get`。
- `&buf[..n]` 要保证 `n <= buf.len()`；用 `min` 或 `get(..n)` 处理不可信长度。
- `split_at_mut` 是借用冲突的标准解法：把一个可变切片拆成两个不重叠切片后，两个区域可以同时修改。
- `&Vec<T>` 会自动解引用为 `&[T]`，函数参数通常直接写 `&[T]`；不要为了读数据强迫调用方先 clone。

### 2.2 非 `Copy` 元素的数组初始化

```rust
// 适用环境：内核 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::VecDeque;
use alloc::sync::Arc;

struct Task;

fn make_queues<const LEVELS: usize>() -> [VecDeque<Arc<Task>>; LEVELS] {
    // from_fn 逐个构造，不要求 VecDeque/Arc/Task 实现 Copy。
    core::array::from_fn(|_| VecDeque::new())
}
```

仓库真实用法见 [`os/src/sched/runqueue.rs`](../../os/src/sched/runqueue.rs)：`[VecDeque<Arc<TaskControlBlock>>; RT_QUEUE_LEVELS]` 通过 `array::from_fn` 初始化。类似地，启动信息中的固定区域和寄存器快照使用数组；数组可以完全不触碰堆。

### 2.3 `Option<T>` 包装容器

`Option<T>` 只有 `Some(T)`/`None` 两个状态。它本身不是 `Vec` 那样的集合；当 `T` 是 `Vec`、`Arc` 或 `Box` 时，`Option` 只是包装那个拥有者/指针，是否分配仍由 `T` 决定。`Some(Vec::new())` 表示“有一个空向量”，不是“没有向量”。

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;

fn take_fd_slot(slots: &mut [Option<Vec<u8>>], fd: usize) -> Option<Vec<u8>> {
    // 先把所有权取出，后面可以安全地修改 slots 的其他槽位。
    slots.get_mut(fd).and_then(|slot| slot.take())
}

fn optional_bytes(value: &Option<Vec<u8>>) -> usize {
    value.as_deref().map_or(0, |bytes| bytes.len())
}

fn install_if_missing(slot: &mut Option<Vec<u8>>) -> &mut Vec<u8> {
    slot.get_or_insert_with(Vec::new)
}
```

常用方法：

- `as_ref()`/`as_mut()`：只借用 `Some` 里的值，不转移所有权。
- `as_deref()`：例如 `Option<Vec<u8>>` 转成 `Option<&[u8]>`，适合只读路径。
- `take()`：把 `Some` 变成 `None` 并取走原值，适合在持有 `&mut` 时解决所有权/借用冲突。
- `replace(new)`：取出旧值并放入新值；`get_or_insert_with` 只在缺失时构造。
- `unwrap()`/`expect()` 只在已经证明必为 `Some` 时用；syscall、磁盘和网络输入不要用它代替错误处理。

CosmOS 中的几个有代表性的语义：

- [`fs/src/vfs.rs`](../../fs/src/vfs.rs) 的 inode 状态用 `Option<Arc<dyn Any + Send + Sync>>` 表示是否安装 page-cache host。
- [`fs/src/fat32/dir.rs`](../../fs/src/fat32/dir.rs) 用 `Option<String>` 表示目录项有没有长文件名；没有长名时回退到 8.3 短名。
- [`os/src/syscall/fs.rs`](../../os/src/syscall/fs.rs) 的 fd 表是“多个 `Option` 槽位”：槽位存在但为 `None` 表示该 fd 未打开。
- [`fs/src/dentry_cache.rs`](../../fs/src/dentry_cache.rs) 没有把“缓存未命中”和“确认不存在”都塞进 `Option`，而是用 `DentryLookup::{Positive, Negative, Miss}`。当状态多于两种时，直接定义 enum 往往比 `Option<Option<T>>` 清楚。

## 3. `Vec<T>`：连续的可增长数组

### 3.1 基本操作和容量语义

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;

fn build_packet(header: &[u8], payload: &[u8]) -> Result<Vec<u8>, ()> {
    let total = header.len().checked_add(payload.len()).ok_or(())?;
    let mut out = Vec::new();
    // try_reserve 只保证“尽量提前报告分配失败”，不能替代长度上限检查。
    out.try_reserve(total).map_err(|_| ())?;
    out.extend_from_slice(header);
    out.extend_from_slice(payload);
    Ok(out)
}

fn fixed_expected_size() -> Vec<u8> {
    // with_capacity(512): len=0、capacity 至少 512；不能立即 out[0]。
    // vec![0; 512]: len=512，元素已经初始化为 0。
    let mut out = Vec::with_capacity(512);
    out.push(0xaa);
    out
}
```

`Vec` 的核心不变量是 `0 <= len <= capacity`。`with_capacity` 只预留空间，不是上限；即使 `capacity() == 32`，第 33 次 `push` 仍可能扩容。需要硬上限时必须自己检查，或使用固定容量方案。

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;

fn append_with_limit(dst: &mut Vec<u8>, src: &[u8], max_len: usize) -> Result<(), ()> {
    let new_len = dst.len().checked_add(src.len()).ok_or(())?;
    if new_len > max_len {
        return Err(());
    }
    dst.try_reserve(src.len()).map_err(|_| ())?;
    dst.extend_from_slice(src);
    Ok(())
}
```

常用复杂度和选择：

- `v[i]`/`v.get(i)` O(1)；遍历、`contains`、`position` O(n)。
- 末尾 `push`/`pop` 摊销 O(1)；扩容时会分配新缓冲并搬移元素，单次可能 O(n)。
- `insert(i, x)` 和 `remove(i)` 要移动一侧元素，最坏 O(n)；`remove(0)` 反复调用会形成 O(n²)。
- 不要求保持顺序时 `swap_remove(i)` O(1)，会把最后一个元素换到 `i`。
- `retain` 按原顺序原地保留元素，遍历 O(n)，通常是批量删除的首选。
- `drain(range)` 把区间交给消费者，适合一次性移走连续区间；注意 drain 迭代器尚未结束时，原 `Vec` 仍处于借用状态。
- `clear` 删除元素但通常保留容量；如果对象生命周期结束，直接让 `Vec` drop。不要为了“清空”无条件 `shrink_to_fit`，它可能产生额外分配/搬移。

### 3.2 CosmOS 中的真实用法

- [`user/src/lib.rs`](../../user/src/lib.rs) 在用户入口把 `argc/argv` 转成 `Vec<&'static str>`，再传给 `main`。
- [`user/src/bin/sh.rs`](../../user/src/bin/sh.rs) 用 `Vec<&str>` 做分词，用 `Vec<String>` 保存可修改的 argv，并用 `drain(i..=i + 1)` 删除重定向参数。
- [`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 用 `Vec<FrameTracker>` 保存页表拥有的帧，并用 `Vec<&'static mut [u8]>` 表达跨页用户缓冲。
- [`os/src/net/mod.rs`](../../os/src/net/mod.rs) 用 `Vec<SocketStorage>` 预建 smoltcp socket storage，也用 `Vec<u8>` 保存收发缓冲。
- [`fs/src/block_cache.rs`](../../fs/src/block_cache.rs) 每个块缓存用 `Vec<u8>` 存储 `BLOCK_SZ` 字节；这里的字节数必须和磁盘接口约定一致。

### 3.3 `Vec` 的边界与现场写法

1. 需要写入第 `i` 个位置时，先 `resize`/`vec![...]` 初始化，或者用 `push`；只有 `capacity` 不会增加 `len`。
2. 来自 syscall 的 `len` 不能直接 `Vec::with_capacity(len)`：先做最大值、加法和乘法的 `checked_*` 检查，再考虑 `try_reserve`。
3. `Vec<u8>` 是拥有的缓冲；只读解析优先接收 `&[u8]`，避免无意义 clone。
4. 跨 syscall/页表保存 `&mut [u8]` 时要确认引用的生命周期和地址空间语义；[`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的 `UserBuffer` 只是内核已翻译后的特定封装，不是任意用户指针都能转成 `'static`。

## 4. `VecDeque<T>`：两端 O(1) 的环形队列

### 4.1 FIFO 模板

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::VecDeque;

fn enqueue_bounded<T>(queue: &mut VecDeque<T>, value: T, limit: usize) -> bool {
    // VecDeque::with_capacity(limit) 仍可在后续自动扩容；硬上限要自己判断。
    if queue.len() >= limit {
        return false;
    }
    queue.push_back(value);
    true
}

fn dequeue_fifo<T>(queue: &mut VecDeque<T>) -> Option<T> {
    queue.pop_front()
}
```

`push_back`/`pop_front` 是经典 FIFO；`push_front`/`pop_back` 用于双端队列。`VecDeque` 的逻辑顺序是 front 到 back，索引 `0` 指向 front，但物理内存可能绕回分成两段。

### 4.2 需要连续字节时

```rust
// 适用环境：用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::VecDeque;

fn consume_console(queue: &mut VecDeque<u8>) {
    // make_contiguous 可能旋转已有元素，但不改变逻辑顺序，也不分配。
    let bytes: &[u8] = queue.make_contiguous();
    consume_bytes(bytes);
    // bytes 的借用到这里结束，之后才能再次修改 queue。
    queue.clear();
}

fn consume_bytes(_bytes: &[u8]) {
    // 替换为 write/syscall/设备提交；这里不假设具体环境。
}
```

如果不想旋转，使用 `let (a, b) = queue.as_slices()`，按两段分别处理。当前仓库的 [`user/src/console.rs`](../../user/src/console.rs) 正是 `VecDeque<u8>` + `make_contiguous()` + `write(STDOUT, ...)`；[`os/src/fs/tty.rs`](../../os/src/fs/tty.rs)、[`os/src/task/wait_queue.rs`](../../os/src/task/wait_queue.rs)、[`os/src/net/unix_socket.rs`](../../os/src/net/unix_socket.rs) 则分别把它用于输入、等待者和 socket 消息。

### 4.3 常见边界

- `VecDeque::with_capacity(n)` 的 `n` 是预留，不是硬上限；TTY、klog、socket 等若有协议/内存上限，要显式在 `len()` 上判断并选择丢弃、阻塞或返回错误。
- `queue[0]` 在空队列时 panic；优先 `front()`/`pop_front()`。
- 不能把 `VecDeque` 直接当成 `&[T]`；使用 `as_slices` 或 `make_contiguous`。
- 对两端队列反复 `Vec::remove(0)` 的替换通常就是 `VecDeque::pop_front()`。

## 5. `BTreeMap<K,V>`：有序 key-value 表

### 5.1 基本模板

```rust
// 适用环境：内核/FS no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::btree_map::Entry;
use alloc::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PageKey(u64);

fn record_page(pages: &mut BTreeMap<PageKey, usize>, key: PageKey, frame: usize) {
    match pages.entry(key) {
        Entry::Vacant(slot) => {
            slot.insert(frame);
        }
        Entry::Occupied(mut slot) => {
            *slot.get_mut() = frame;
        }
    }
}

fn sum_range(pages: &BTreeMap<PageKey, usize>, begin: PageKey, end: PageKey) -> usize {
    pages.range(begin..end).map(|(_, frame)| *frame).sum()
}
```

`BTreeMap` 要求 key 实现 `Ord`；如果是自定义 key，通常一起 derive `Eq, PartialEq, Ord, PartialOrd`。遍历结果按 key 排序，这对于 VMA、页号、PID、设备名和目录名很有用。`entry` 可以把“先 `contains_key` 再 `insert`”合成一次查找，避免重复树查找和竞态窗口。

CosmOS 里的典型映射：

- [`os/src/mm/memory_set.rs`](../../os/src/mm/memory_set.rs)：`vmas`、`data_frames`、`direct_cache_pages` 以虚页号排序，使用 `range(..=cursor).next_back()` 找到包含当前 VPN 的区间。
- [`os/src/fs/page_cache.rs`](../../os/src/fs/page_cache.rs)：页号→缓存页用 `BTreeMap`，脏页用 `BTreeSet`，适合按页号扫描和批量回写。
- [`os/src/sched/runqueue.rs`](../../os/src/sched/runqueue.rs)：CFS 任务用 `(vruntime, pointer)` 这样的有序 key 取得最左任务。
- [`os/src/fs/epoll.rs`](../../os/src/fs/epoll.rs)、[`os/src/drivers/block/mod.rs`](../../os/src/drivers/block/mod.rs)、[`os/src/ipc.rs`](../../os/src/ipc.rs)：兴趣项、设备注册、共享内存按 key 管理。
- [`fs/src/dentry_cache.rs`](../../fs/src/dentry_cache.rs)：两层 `BTreeMap` 保存目录父项→名字→dentry，并借助 `String: Borrow<str>` 允许用 `&str` 查询而不为每次命中创建临时 `String`。

### 5.2 `BTreeMap` 的范围和删除

树迭代器借用了 map；在迭代器仍存活时调用 `remove` 会触发 E0502/E0499。先收集可复制的 key，再进行第二遍修改：

```rust
// 适用环境：内核/FS no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

fn remove_old(map: &mut BTreeMap<u64, usize>, cutoff: u64) {
    let keys: Vec<u64> = map
        .range(..cutoff)
        .filter(|(_, value)| **value == 0)
        .map(|(&key, _)| key)
        .collect();

    for key in keys {
        map.remove(&key);
    }
}
```

这正是 [`os/src/mm/memory_set.rs`](../../os/src/mm/memory_set.rs) 在 `MADV_DONTNEED` 路径里“先收集 resident VPN，再逐个 unmap”的思路，也是 [`os/src/fs/page_cache.rs`](../../os/src/fs/page_cache.rs) truncate 路径避免在 `BTreeMap` iterator 上直接删除的原因。若只是按谓词保留，且当前工具链/容器版本提供 `retain`，可以用 `map.retain(|key, value| ...)`；需要跨版本或收集被删对象时，双阶段 key 列表最稳妥。

注意 `BTreeMap` 的节点本身仍然需要堆分配；“有序”不等于“无分配”。在中断、不可睡眠锁或极小内存路径中，若不能承受节点分配，应改为固定数组/索引表，或预先设计容量和回收策略。

## 6. `hashbrown::HashMap<K,V>`：内核可用的哈希表

### 6.1 当前仓库的用法与约束

```rust
// 适用环境：内核 no_std + alloc；RISC-V/LoongArch 均适用。
use hashbrown::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct FutexLikeKey {
    address: usize,
    private_mm: Option<usize>,
}

fn count_key(map: &mut HashMap<FutexLikeKey, usize>, key: FutexLikeKey) {
    *map.entry(key).or_insert(0) += 1;
}

fn lookup_key(map: &HashMap<FutexLikeKey, usize>, key: &FutexLikeKey) -> Option<usize> {
    map.get(key).copied()
}
```

key 必须同时满足 `Eq + Hash`。最重要的不变量是：如果 `a == b`，那么 `hash(a) == hash(b)`；把 key 放入 map 后，不要通过内部可变性、全局状态或 unsafe 改变它参与 `Eq`/`Hash` 的字段。对于自定义结构体，`#[derive(Eq, PartialEq, Hash)]` 通常比手写实现安全。

当前仓库中：

- [`os/src/sync/futex.rs`](../../os/src/sync/futex.rs) 用 `hashbrown::HashMap<FutexKey, Arc<WaitQueue>>` 管理 futex 等待队列，也用 `HashSet` 做 waiter 快照。
- [`os/src/task/wait_queue.rs`](../../os/src/task/wait_queue.rs) 的 `WaitQueueKeyed<T>` 同时用 `VecDeque<T>` 保证 FIFO 和 `HashMap<T, Arc<TaskControlBlock>>` 做按 key 删除/唤醒；所以 `T` 被约束为 `Default + Eq + Hash + Copy` 等 trait。

### 6.2 平均复杂度不等于硬上界

- 查找、插入、删除平均 O(1)，但冲突严重、扩容或恶意输入时可能 O(n)。
- 哈希表的遍历顺序未定义，不能用于输出排序、协议编码或测试快照的稳定顺序。
- `with_capacity` 是初始容量提示，不是满后返回错误的固定容量；需要硬上限时用数组/`heapless`/手写环，或者插入前比较 `len`。
- 默认 hasher 是仓库依赖版本的一部分。`hashbrown 0.12.3` 的默认特性包含 `ahash`；不要只改 `Cargo.toml` 版本而不更新锁文件，也不要在断网现场临时引入一个缓存里没有的 hasher。
- 哈希表不是安全边界。若 key 完全由外部输入控制，既要考虑哈希冲突导致的 CPU/内存消耗，也要考虑是否需要确定性顺序；无法确认 hasher 或攻击模型时，内核元数据常可选 `BTreeMap` 牺牲 O(log n) 换取排序和可预测行为。

### 6.3 `HashMap` 里的删除

只按值谓词删除时，优先使用容器提供的 `retain`（当前 `hashbrown 0.12.3` 支持该接口）；需要在删除后继续调用会借用 map 的逻辑时，可使用 key 列表：

```rust
// 适用环境：内核 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;
use hashbrown::HashMap;

fn remove_zero_values(map: &mut HashMap<u32, usize>) {
    map.retain(|_, value| *value != 0);
}

fn remove_keys_in_two_phases(map: &mut HashMap<u32, usize>) {
    let keys: Vec<u32> = map
        .iter()
        .filter(|(_, value)| **value > 100)
        .map(|(&key, _)| key)
        .collect();
    for key in keys {
        map.remove(&key);
    }
}
```

`entry` 也能避免常见的“先 `get`、随后 `insert`”借用冲突；如果 map 被 `SpinNoIrqLock` 包住，要让 map guard 的生命周期尽可能短，尤其不要拿着 guard 去阻塞、唤醒任务、做设备 I/O 或调用可能再次访问同一 map 的函数。

## 7. 固定容量替代方案：数组优先，库类型要看依赖

### 7.1 CosmOS 真实的固定环

[`user/src/bin/tcp_echo_server.rs`](../../user/src/bin/tcp_echo_server.rs) 的 `FdQueue` 使用 `[usize; 32]`、`head`、`tail`、`len`，在 12 个 worker 之间传递连接 fd；它在 kernel mutex 保护下运行，队列满时关闭新连接。这是“固定容量 + 明确背压”的完整 OS 用法。

现场可直接改写为更明确返回值的版本：

```rust
// 适用环境：用户态 no_std；RISC-V/LoongArch 均适用；不分配堆内存。
const QUEUE_SIZE: usize = 32;

struct FixedFdQueue {
    buf: [usize; QUEUE_SIZE],
    head: usize,
    tail: usize,
    len: usize,
}

impl FixedFdQueue {
    const fn new() -> Self {
        Self {
            buf: [0; QUEUE_SIZE],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == QUEUE_SIZE
    }

    fn push(&mut self, fd: usize) -> Result<(), usize> {
        if self.is_full() {
            return Err(fd); // 调用方决定关闭、丢弃或稍后重试。
        }
        self.buf[self.tail] = fd;
        self.tail = (self.tail + 1) % QUEUE_SIZE;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<usize> {
        if self.is_empty() {
            return None;
        }
        let fd = self.buf[self.head];
        self.head = (self.head + 1) % QUEUE_SIZE;
        self.len -= 1;
        Some(fd)
    }
}
```

不变量：`0 <= len <= QUEUE_SIZE`，`head/tail` 始终落在数组范围内；`push` 只在不满时写，`pop` 只在非空时读。若把 `QUEUE_SIZE` 改成 0，取模会失去意义，因此固定环的容量必须是正数。

### 7.2 `[Option<T>; N]`：固定槽位但元素可移动

```rust
// 适用环境：内核/用户态 no_std + core；RISC-V/LoongArch 均适用。
fn new_slots<T, const N: usize>() -> [Option<T>; N] {
    core::array::from_fn(|_| None)
}

fn take_slot<T, const N: usize>(slots: &mut [Option<T>; N], index: usize) -> Option<T> {
    slots.get_mut(index).and_then(|slot| slot.take())
}
```

它适合固定数量的对象槽位、探针槽位、缓存项或“可能未安装”的资源。和 `Vec<Option<T>>` 的区别是：数组的槽位数本身固定且不分配；`Vec<Option<T>>` 的槽位数可以动态变化，但每个 `Some(T)` 仍可能拥有堆对象。

### 7.3 `heapless`、`managed` 与当前仓库的边界

vendor 的 [`smoltcp/Cargo.toml`](../../vendor/smoltcp/Cargo.toml) 直接依赖 `heapless = "0.8"`，并在接口、IPv6 选项、邻居缓存等地方使用 `heapless::Vec`/`LinearMap`。但 `os/Cargo.toml` 只直接声明 `smoltcp`，没有声明 `heapless`；Rust 代码不能把“传递依赖出现在 Cargo.lock”当成自己的直接依赖。

如果现场确实允许新增依赖、且离线缓存与锁文件都已准备好，固定容量 API 大致如下；否则优先用数组或当前仓库已有的 `Vec`/`VecDeque`：

```rust
// 适用环境：no_std；RISC-V/LoongArch 均可；前提是当前 crate 的 Cargo.toml 直接声明 heapless 且离线可解析。
use heapless::{Deque, LinearMap, Vec};

fn bounded_examples() {
    let mut bytes: Vec<u8, 64> = Vec::new();
    if bytes.push(0xaa).is_err() {
        // 容量已满：返回错误、丢包或走背压路径。
    }

    let mut queue: Deque<u32, 8> = Deque::new();
    let _ = queue.push_back(1);
    let _ = queue.pop_front();

    let mut table: LinearMap<u16, usize, 16> = LinearMap::new();
    let _ = table.insert(7, 0x1000);
}
```

`heapless::Vec<T,N>` 的 `N` 是容量，`push` 返回 `Result`；这是和 `alloc::Vec` 最容易混淆的地方。vendor smoltcp 还提供基于 `managed::ManagedSlice` 的 [`RingBuffer`](../../vendor/smoltcp/src/storage/ring_buffer.rs)，它可以使用调用者提供的存储，具体是固定切片还是 alloc 存储取决于调用方式和 feature。不要从 smoltcp 内部 API 反推整个 CosmOS 都能直接导入这些类型。

## 8. 迭代删除、批量修改与借用冲突

### 8.1 `Vec`：保序、无序和批量删除

保留满足条件的元素，优先 `retain`：

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;

fn remove_stale(ids: &mut Vec<usize>, current: usize) {
    ids.retain(|id| *id != current); // O(n)，保持剩余元素顺序。
}
```

不需要顺序时，用索引循环 + `swap_remove`，不要在 `iter()` 中删除：

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;

fn remove_even_unordered(values: &mut Vec<usize>) {
    let mut i = 0;
    while i < values.len() {
        if values[i] % 2 == 0 {
            values.swap_remove(i); // 删除后新元素填入 i，所以不递增 i。
        } else {
            i += 1;
        }
    }
}
```

如果必须保序且删除数量少，可以用 `remove(i)`；但反复从中间删除可能 O(n²)。如果要把删除对象另存，可以先 `mem::take` 原 vector，再按条件重新 push，或者设计成一次 `drain(..)` 消费；两者都要评估额外容量和分配。

### 8.2 `VecDeque`：弹出重建最简单

```rust
// 适用环境：内核/FS/用户态 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::collections::VecDeque;

fn keep_ready<T>(queue: &mut VecDeque<T>, mut is_ready: impl FnMut(&T) -> bool) {
    let old = core::mem::take(queue);
    for item in old {
        if is_ready(&item) {
            queue.push_back(item);
        }
    }
}
```

这里不持有 `queue.iter()` 再修改 `queue`；代价是重建过程中可能触发扩容。若当前版本的 `VecDeque::retain` 满足需求，可直接 `queue.retain(|x| ...)`，通常更简洁。

### 8.3 `BTreeMap`/`HashMap`：先 key，后删除

错误形状：

```rust
// 适用环境：示意代码；会触发借用检查错误，不要照抄。
// for (key, value) in map.iter() {
//     if should_remove(value) {
//         map.remove(key); // E0502：iter 借用尚未结束，不能可变借用 map。
//     }
// }
```

正确形状是“只读阶段收集 key，可变阶段删除”：

```rust
// 适用环境：内核 no_std + alloc；RISC-V/LoongArch 均适用。
use alloc::vec::Vec;
use hashbrown::HashMap;

fn purge(map: &mut HashMap<u64, usize>) {
    let doomed: Vec<u64> = map
        .iter()
        .filter(|(_, value)| **value == 0)
        .map(|(&key, _)| key)
        .collect();
    for key in doomed {
        map.remove(&key);
    }
}
```

若 key 不能 `Clone`/`Copy`，可以收集轻量句柄、索引或让 map 的值带有“待删除”标记，然后用 `retain`；不要为了删除而 clone 一个很大的 key。对 `BTreeMap<Vec<u8>, V>`，key clone 还会复制整个路径/地址，应特别注意。

### 8.4 嵌套 map、锁和借用的三层作用域

内核常见类型是 `SpinNoIrqLock<BTreeMap<...>>` 或 `SpinNoIrqLock<HashMap<...>>`。把“锁 guard、map 借用、外部调用”拆成作用域：

```rust
// 适用环境：内核 no_std + alloc；RISC-V/LoongArch 均适用；SpinNoIrqLock 仅作接口示意。
use alloc::sync::Arc;
use hashbrown::HashMap;

fn take_value(/* table: &SpinNoIrqLock<HashMap<u64, Arc<Node>>> */) {
    // 伪代码结构：实际锁类型按调用点替换。
    // let value = {
    //     let mut map = table.lock();
    //     map.remove(&key)
    // };
    // if let Some(value) = value {
    //     wake_or_io(value); // 不持有 map guard 做可能重入/阻塞的工作。
    // }
}
```

原则：

- 先在锁内取出 `Arc`/值的所有权，再释放锁；锁外执行唤醒、回收、I/O 或可能再次访问同一表的函数。
- 在 map 的 `get_mut` 借用仍活着时不要 `remove` 同一个 map；用 `entry` 或缩小作用域。
- 在 `VecDeque::front_mut`/`make_contiguous` 返回的引用仍活着时不要 `push/pop/clear`；让引用先离开作用域。
- 在迭代删除前要考虑锁：收集 key 仍需在锁内完成，但第二阶段可以在同一个短锁作用域内执行，不能把 key 收集后误以为数据在锁外仍不变。

## 9. CosmOS 文件路径→容器/函数映射

下面的路径是现场查代码时优先打开的入口；函数名来自当前仓库，不是泛泛的 Rust 教程。

| 主题 | 路径 | 重点类型/函数和可借鉴点 |
| --- | --- | --- |
| 内核堆与 alloc | [`os/src/mm/heap_allocator.rs`](../../os/src/mm/heap_allocator.rs) | `HEAP_ALLOCATOR`、`init_heap`、`grow`、`alloc_error_handler`；`Vec`/`BTreeMap`/`Arc` 的分配最终经过这里。 |
| 用户堆与 argv | [`user/src/lib.rs`](../../user/src/lib.rs) | `HEAP_SPACE`、`__user_start`、`Vec<&'static str>`；入口先初始化 heap，再构造 argv。 |
| RT/CFS 调度 | [`os/src/sched/runqueue.rs`](../../os/src/sched/runqueue.rs) | `rt_queues: [VecDeque<_>; RT_QUEUE_LEVELS]`、`cfs_tasks: BTreeMap<...>`、`PID2PCB`；固定优先级队列 + 有序 vruntime。 |
| 普通/按键等待 | [`os/src/task/wait_queue.rs`](../../os/src/task/wait_queue.rs) | `WaitQueue` 的 `VecDeque`；`WaitQueueKeyed<T>` 的 FIFO key 队列和 `HashMap<T, Arc<TaskControlBlock>>`；`retain` 清理 key。 |
| futex | [`os/src/sync/futex.rs`](../../os/src/sync/futex.rs) | `FutexKey` derive `Eq/Hash`；`FUTEX_QUEUES` 用 `hashbrown::HashMap`，快照用 `HashSet`。 |
| 页缓存 | [`os/src/fs/page_cache.rs`](../../os/src/fs/page_cache.rs) | `pages: BTreeMap`、`dirty_pages: BTreeSet`、`jobs/inactive: VecDeque`；truncate/`MADV_DONTNEED` 先 collect key 再删除。 |
| 地址空间/VMA | [`os/src/mm/memory_set.rs`](../../os/src/mm/memory_set.rs) | `vmas`、`data_frames`、`direct_cache_pages`；`range(..=cursor).next_back()` 做有序区间定位。 |
| epoll | [`os/src/fs/epoll.rs`](../../os/src/fs/epoll.rs) | `interests: BTreeMap`、`ready: VecDeque`；`collect_ready` 弹出队列并生成 `Vec<EpollEvent>`。 |
| socket | [`os/src/net/mod.rs`](../../os/src/net/mod.rs)、[`os/src/net/unix_socket.rs`](../../os/src/net/unix_socket.rs) | socket storage、状态 `Vec`、TCP/Unix pending `VecDeque`、Unix registry `BTreeMap<Vec<u8>, usize>`、可选地址 `Option<Vec<u8>>`。 |
| TTY/console/klog | [`os/src/fs/tty.rs`](../../os/src/fs/tty.rs)、[`user/src/console.rs`](../../user/src/console.rs)、[`os/src/klog.rs`](../../os/src/klog.rs) | 两端/输入队列；用户 console 用 `make_contiguous` 输出；klog 用 `VecDeque<u8>` 做有界淘汰。 |
| tmpfs/VFS cache | [`os/src/fs/tmpfs.rs`](../../os/src/fs/tmpfs.rs)、[`fs/src/inode_cache.rs`](../../fs/src/inode_cache.rs)、[`fs/src/dentry_cache.rs`](../../fs/src/dentry_cache.rs) | 页号/目录树用 `BTreeMap`，CLOCK 候选用 `VecDeque`；负 dentry 使用三态 enum。 |
| 设备注册 | [`os/src/drivers/block/mod.rs`](../../os/src/drivers/block/mod.rs)、[`os/src/drivers/block/virtio_blk.rs`](../../os/src/drivers/block/virtio_blk.rs) | 设备名/IRQ→设备 `BTreeMap`；in-flight 请求用 `Vec`，完成时按场景 `swap_remove`。 |
| 用户 shell | [`user/src/bin/sh.rs`](../../user/src/bin/sh.rs) | `Vec<&str>` 分词、`Vec<String>` owned argv、`drain` 删除 `<`/`>` 对。 |
| 用户固定队列 | [`user/src/bin/tcp_echo_server.rs`](../../user/src/bin/tcp_echo_server.rs) | `[usize; 32]` + head/tail/len；队列满时 close，适合离线现场改成固定容量 worker queue。 |
| vendor 网络栈 | [`vendor/smoltcp/Cargo.toml`](../../vendor/smoltcp/Cargo.toml)、[`vendor/smoltcp/src/iface/interface/mod.rs`](../../vendor/smoltcp/src/iface/interface/mod.rs) | 版本 `0.13.0`；`no_std` 配置下仍直接使用 `heapless` 的有界协议元数据；不等于 `os` 可直接导入传递依赖。 |

## 10. 现场命令模板与版本注意

命令应从仓库根目录 `/home/kyle/OS/xxOS` 执行。以下命令是模板；它们可能更新 `target/`、生成镜像或需要完整离线缓存，现场先确认磁盘空间和依赖缓存。

```sh
# 适用环境：仓库根目录；检查当前 manifest 能否在本地缓存中解析，不编译 kernel。
cargo +nightly-2025-01-18 metadata \
  --manifest-path os/Cargo.toml --offline --locked --no-deps

# 适用环境：仓库根目录；只检查用户 crate 的语法/类型，目标为 RISC-V。
cargo +nightly-2025-01-18 check \
  --manifest-path user/Cargo.toml \
  --target riscv64gc-unknown-none-elf \
  --offline --locked
```

仓库 Makefile 是更可靠的内核构建入口，因为它会传入文件系统 feature、额外 kernel features 和链接参数：

```sh
# 适用环境：离线仓库；分别构建两种用户程序。
make -C user build ARCH=riscv64
make -C user build ARCH=loongarch64

# 适用环境：离线仓库；分别编译内核。os/Makefile 会选目标与链接脚本。
make -C os kernel ARCH=riscv64
make -C os kernel ARCH=loongarch64

# 适用环境：仓库根目录；先恢复被评测过滤隐藏目录时需要的 Cargo 配置。
make cargo-config
```

当前配置的细节：

- `os/Makefile` 默认 `MAIN_FS := ext4`，kernel 命令使用 `--no-default-features --features ext4`，并附加 `legacy-vdb-names`、缓存探针等 feature；不要把一个只启用 `std` 的本机示例当作内核构建验证。
- `user/.cargo/config.toml` 与 `os/.cargo/config.toml` 的链接参数不同；用户态和内核态不要交叉使用链接脚本。
- 根 [`Makefile`](../../Makefile) 的 `BUILD_ARCH=rv`/`la`、`make run`/`make run-la` 会进一步制作镜像和启动 QEMU；这不是单个容器片段的必要验证，但适合最终集成检查。
- 根 `rust-toolchain.toml` 的 `targets` 明确列出 RISC-V；LoongArch 构建前检查本机/nightly 是否安装 `loongarch64-unknown-none`，不要在断网时假定能下载。
- `--offline` 只禁止网络访问，不会凭空提供缺失 crate；`--locked` 用来防止 Cargo 在现场悄悄改写解析结果。

## 11. 常见坑：错误现象 → 原因 → 检查 → 修复方向

| 错误现象 | 常见原因 | 先检查什么 | 修复方向 |
| --- | --- | --- | --- |
| `use of undeclared crate or module std`、找不到 `std::vec::Vec` | 当前 crate 是 `no_std` | 文件顶部的 `#![no_std]`、调用路径是 `os`/`fs`/`user` 还是 `fs-fuse` | 内核/FS/用户态改用 `alloc::vec::Vec`、`alloc::collections::*`；哈希表用当前 `hashbrown`。 |
| `cannot find type Vec` 或 `alloc` 相关错误 | 没有导入类型，或 crate 根缺 `extern crate alloc` | 看 [`os/src/main.rs`](../../os/src/main.rs)、[`fs/src/lib.rs`](../../fs/src/lib.rs)、[`user/src/lib.rs`](../../user/src/lib.rs) | 根 crate 声明 `extern crate alloc`；模块显式 `use alloc::vec::Vec`；用户 binary 通常也声明 `extern crate alloc`。 |
| `the trait bound K: Hash/Eq is not satisfied` | `HashMap` key trait 不完整 | 自定义 key 的 derive 和泛型约束 | `#[derive(Eq, PartialEq, Hash)]`，并保证 Hash/Eq 一致；不要把只实现 `Ord` 的 key 直接放 HashMap。 |
| `the trait bound K: Ord is not satisfied` | `BTreeMap` key 缺少排序 | key 定义和 `entry/range` 调用 | derive `Ord/PartialOrd/Eq/PartialEq`，或用已有可排序的整数/字符串/元组。 |
| `index out of bounds`、`slice index starts at...` | 把 `capacity` 当作 `len`，或用户长度未经校验 | `len()`、`capacity()`、切片边界、是否先 `push/resize` | 读取用 `get`；写入先 `resize` 或 `push`；所有加法/乘法先 `checked_*`；不要用 `with_capacity` 代替初始化。 |
| `memory allocation of ... bytes failed`、用户程序 panic | `Vec`/`String`/`Box` 超过当前堆、整数溢出后申请超大空间、碎片或对象未释放 | 用户 `USER_HEAP_SIZE`、内核 heap stats/`KERNEL_HEAP_*`、`with_capacity` 参数、临时 clone/collect | 先设置协议/输入上限，使用 `try_reserve`；缩小/分批处理；复用 `Vec`；固定环/数组替代；确认不是在 allocator 初始化前分配。 |
| 固定队列“满了”或数据被覆盖 | 自定义环没有检查 `len == N`，或 `head/tail/len` 更新顺序错误 | 满/空分支、`QUEUE_SIZE`、加锁范围 | `push` 返回 `Result`/`bool`；满时显式丢弃/关闭/阻塞；只让一个同步方案负责保护 head/tail。 |
| `Vec` 删除很慢、调度/网络出现长尾 | 在循环中 `remove(0)` 或中间 `remove`，形成 O(n²) | `rg -n 'remove\(0\)|remove\(' os/src user/src fs/src`，看是否在热路径 | FIFO 改 `VecDeque::pop_front`；无序批量删除改 `swap_remove`；保序批量删除改 `retain`。 |
| `VecDeque` 输出乱码、只处理了一半 | 误把环形存储当一段连续内存 | 是否调用了 `as_slice`/裸指针；`as_slices()` 两段长度 | 分两段处理，或在短临界区调用 `make_contiguous()`；不要持有返回切片时继续 push/pop。 |
| HashMap 输出顺序每次不同、测试偶发失败 | 哈希表没有稳定遍历顺序 | 是否把 `.iter()` 结果直接用于排序/序列化 | 需要确定性时改 `BTreeMap` 或先把 key/value 收集到 `Vec` 再排序；测试不要依赖 hash iteration order。 |
| HashMap 查不到“明明存在”的 key | key 在放入后被改变，或手写 Hash/Eq 不一致 | key 是否有内部可变字段、`Eq` 与 `Hash` 实现、是否实际使用同一 key | key 入表后保持 hash/equality 字段不变；优先 derive；确认没有把地址、生命周期或可变状态错误地当 key。 |
| 在 `for map.iter()` 里 `remove` 报 E0499/E0502 | 迭代器借用仍存活 | 报错指向 iterator 和 remove 的两处借用 | 用 `retain`，或先收集 key 再第二遍删除；必要时用 `Entry`/缩小作用域。 |
| `get_mut` 后不能 `remove`/再次 `get_mut` | 同一个容器上已有可变借用 | `let value = map.get_mut(...)` 的生命周期 | 用 `entry` 一次完成；把只需的值 `copied/cloned/take` 出来；用 `{ ... }` 提前结束借用。 |
| `BTreeMap::range` 路径行为异常或 panic | 范围端点方向/类型不对，或在 range iterator 活着时改 map | `start <= end`、端点是否是同一 key 类型、是否嵌套锁 | 先验证范围；读取/收集与修改分阶段；对“包含当前 cursor 的区间”使用仓库已有 `range(..=cursor).next_back()` 模式。 |
| `VecDeque::with_capacity` 之后仍然扩容 | 容量只是预留，不是固定容量 | 是否有 `len()` 上限检查 | 手动检查 `len >= limit`，或使用数组/`heapless`；对于内核路径决定满时的背压语义。 |
| `HashMap` 编译成功但现场性能/可重复性不符合预期 | `hashbrown`/hasher/feature/版本被改动 | `os/Cargo.toml`、`os/Cargo.lock`、`cargo metadata --offline --locked` | 恢复仓库锁定的 `hashbrown 0.12.3`；不要擅自换 `std`、新 hashbrown 或未缓存 crate。 |
| 内核启动早期就分配失败，或锁内分配/阻塞后死锁 | allocator 尚未完成初始化，或在不可睡眠/可能重入临界区做了重分配 | 调用点是否在 `mm::init`/用户 `__user_start` 之前；是否持有 `SpinNoIrqLock`、是否在 IRQ/trap | 延后构造；预分配/固定数组；缩短锁作用域；锁内只做必要的 map/queue 操作，锁外做唤醒、I/O 和重活。 |
| RISC-V 能编译，LoongArch 缺 `core`/链接失败 | 目标未安装、使用了错误 `.cargo` 配置或链接脚本 | `rustup target list --installed`、`os/.cargo/config.toml`、`user/.cargo/config.toml`、Makefile 的 `ARCH` | 使用仓库声明的 `loongarch64-unknown-none`，分别从 `make -C user/os ... ARCH=loongarch64` 验证；离线缺 target 时先准备工具链，不能靠改容器类型解决。 |

## 12. 最后现场 checklist

### 设计前

- [ ] 写清楚数据是否拥有：借用用 `&[T]`/`&mut [T]`，跨调用/保存才用 `Vec`/`Arc`/`Box`。
- [ ] 写清楚顺序：FIFO 用 `VecDeque`，排序/范围用 `BTreeMap`，不需要顺序才考虑 `HashMap`。
- [ ] 写清楚容量：`with_capacity` 只是预留；硬上限必须有 `len` 检查、错误返回和满载策略。
- [ ] 能不用堆就不用堆：编译期固定数量优先数组、`[Option<T>; N]` 或固定环。
- [ ] `None`、空集合、已知负结果是否是三种不同状态？是则定义 enum。

### 写代码时

- [ ] `os`/`fs`/`user` 用 `alloc` 路径；不要把 `std::collections` 示例直接贴进 no_std crate。
- [ ] 对自定义 HashMap key derive/验证 `Eq + Hash`；对 BTreeMap key derive/验证 `Ord`。
- [ ] 对外部长度做最大值和 `checked_add/mul`；大分配优先 `try_reserve`，不把 `unwrap` 当 OOM 策略。
- [ ] 从 `VecDeque` 两端操作；只有明确需要连续字节时才 `make_contiguous`。
- [ ] 删除时使用 `retain`、`swap_remove`、`pop_front` 或“收集 key→第二遍删除”；不在 iterator 上直接改原容器。
- [ ] 让 `get_mut`、`iter_mut`、`make_contiguous`、锁 guard 的借用尽快结束；需要跨阶段就 `take/copy/clone` 合适的轻量值。
- [ ] 不在持有 map/queue 锁时做阻塞、设备 I/O、任务唤醒或可能再次访问同一容器的调用。

### 离线构建/集成前

- [ ] 确认 `rust-toolchain.toml` 的 nightly 与目标；RISC-V/LoongArch 分别检查 target 是否已安装。
- [ ] 保留并检查 `Cargo.lock`；依赖变更前确认本地 Cargo cache，使用 `--offline --locked`。
- [ ] 至少执行对应 crate 的 `cargo fmt --check` 和 `cargo check`/仓库 Makefile；内核最终以 `make -C os kernel ARCH=...` 为准。
- [ ] 对固定队列测空、单元素、满、满后 push、空后 pop、head/tail 绕回、并发锁保护和丢弃策略。
- [ ] 对 map 测缺失 key、重复 insert、删除最后一项、范围为空、key 顺序/Hash 一致性。
- [ ] 对 `Vec`/`VecDeque` 测零长度、边界长度、超过预留容量、超上限、OOM/错误返回路径。
- [ ] 集成运行需要时再用根 Makefile 的 `make run`/`make run-la`；观察串口日志，不把宿主 `fs-fuse` 的 std 行为误判为 guest 内核行为。
- [ ] 交付前执行 `git status --short -- docs/final_prepare/02-rust-collections.md`；若文件仍未跟踪，用 `git diff --no-index -- /dev/null docs/final_prepare/02-rust-collections.md || test $? -eq 1` 查看全文差异，确认没有把临时测试代码写进其他源码。

## 13. 离线可查的官方资料名

现场若资料包保留源码文档，可按以下稳定名称查 API；正文以上已经给出足够用法，不要求联网：

- Rust `alloc` crate：`alloc::vec::Vec`、`alloc::collections::VecDeque`、`alloc::collections::BTreeMap`。
- Rust `core`：`core::slice`、`core::option::Option`、`core::array::from_fn`。
- `hashbrown 0.12.3` 的 `HashMap` API 与该版本源码/锁文件；不要只看最新版本文档。
- vendor `smoltcp 0.13.0` 的 `Cargo.toml`、`storage::RingBuffer`、`heapless` 使用点。

联网时可查的官方/版本文档入口（只作索引，不是本资料的前置条件）：

- <https://doc.rust-lang.org/alloc/>
- <https://doc.rust-lang.org/alloc/vec/struct.Vec.html>
- <https://doc.rust-lang.org/alloc/collections/vec_deque/struct.VecDeque.html>
- <https://doc.rust-lang.org/alloc/collections/btree_map/struct.BTreeMap.html>
- <https://docs.rs/hashbrown/0.12.3/hashbrown/struct.HashMap.html>
- <https://docs.rs/heapless/0.8.0/heapless/>
