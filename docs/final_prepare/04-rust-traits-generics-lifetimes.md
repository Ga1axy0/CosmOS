# CosmOS Rust 现场速查：Trait、泛型、生命周期与智能指针

这份资料面向几天后断网、不能使用 Codex 的 Rust OS 参赛者。内容以当前仓库实现为准；标准 Rust 规则只在需要解释编译器行为时补充。每个代码块第一行会标明适用环境。

## 0. 环境标签与最重要的结论

| 标签 | 含义 |
| --- | --- |
| **no_std/内核** | os/ 内核；可用 core、alloc 和 no_std 依赖，不能直接用 std。 |
| **no_std/fs** | fs/ 文件系统 crate；fs/src/lib.rs 有 #![no_std]。 |
| **no_std/用户态** | user/ 和 user/src/bin/；用户程序也有 #![no_std]，通过 user_lib 使用系统调用。 |
| **std/主机工具** | fs-fuse/；可使用主机文件 I/O 与 std::sync::{Arc, Mutex}。 |
| **RV** | riscv64gc-unknown-none-elf，QEMU virt 内核路径。 |
| **LA** | loongarch64-unknown-none，LoongArch direct boot 路径。 |
| **RV/LA** | 抽象不依赖架构，但要检查 cfg(target_arch = ...) 下的实现。 |

现场先记住五句话：

1. 单一拥有者用 Box；单线程共享可考虑 Rc；多 hart/线程共享用 Arc。
2. Arc 只解决共享所有权，不解决内部可变性；可变状态还需要锁或原子类型。
3. 泛型 T: Trait 是编译期选择；dyn Trait 是运行期 vtable 选择。
4. 生命周期描述借用之间的关系；不要为了绕过错误随意制造 'static。
5. 内核锁的关键不是名字而是“能否睡眠、是否关本地中断、guard 作用域多大”。

## 1. 现场一分钟索引

| 需求/错误 | 先查 |
| --- | --- |
| 多个文件系统后端共用调用方 | trait、Arc<dyn VfsNode>、as_any() |
| RV/LA 共享上层代码、底层 frame/PTE 不同 | associated type、PagingArch::Entry、TrapContextAbi::Frame |
| 接受任意满足约束的实现 | 泛型 D: Trait 或 where D: Trait |
| 运行时替换后端/异构容器 | dyn Trait；检查 dyn-compatible/object-safe |
| 返回/保存引用、批量 I/O | 生命周期 'a；确认 buffer 的拥有者 |
| 借用太久/同时可变借用 | 缩小 guard/借用作用域，拆分读取和修改 |
| 多执行流共享 | Arc<T> + 合适的锁；检查 Send + Sync |
| 单线程对象图 | Rc<T>；不要用于内核 SMP 共享路径 |
| 运行时借用冲突 | RefCell<T> 的 borrow/borrow_mut；这不是同步 |
| 堆对象或 FFI 指针 | Box<T>、Box::into_raw/Box::from_raw |
| 父子/反向引用形成环 | Weak<T> + upgrade() |
| E0277/E0599 trait bound | 看缺少的 trait、feature/cfg、where 约束 |
| E0038 dyn incompatible | 泛型方法、返回 Self、缺 Self: Sized 限制 |
| E0499/E0502/E0597 | 找出第一次借用和 guard 的实际作用域 |
| Send/Sync 错误 | 从最内层字段逐层定位，不要盲加 unsafe impl |

## 2. 当前仓库的构建边界

### 2.1 包、版本和 no_std

| 路径 | 当前事实 | 对本主题的影响 |
| --- | --- | --- |
| os/Cargo.toml | edition 2021；依赖 fs 时 default-features = false，启用 io_perf_counters、kernel_sleep_mutex；默认 feature 有 ext4、platform-qemu-virt 等。 | 内核文件系统边界是 fs trait + alloc::sync::Arc。 |
| os/src/main.rs | #![no_std]、#![no_main]、extern crate alloc。 | 用 core::...、alloc::...，不能导入 std::sync::Mutex。 |
| fs/Cargo.toml | edition 2018；spin = "0.7.0"、lazy_static 的 spin_no_std；有 kernel_sleep_mutex。 | fs 可独立 no_std；feature 决定 sleep mutex 是否调用内核 hook。 |
| fs/src/lib.rs | #![no_std]、extern crate alloc；公开 BlockDevice、VfsNode、Inode。 | 内核和 FUSE 主机适配器共用抽象。 |
| user/Cargo.toml + user/src/lib.rs | edition 2018；user/src/lib.rs 有 #![no_std]，使用 buddy_system_allocator、spin = "0.9"。 | 用户态不是 std 用户态；并发主要通过系统调用和内核同步对象。 |
| fs-fuse/Cargo.toml | edition 2018；主机工具，fatfs 启用 std。 | 只有这里的 std::sync::Mutex 示例可直接用于主机文件。 |
| rust-toolchain.toml | 根目录锁定 nightly-2025-01-18，含 llvm-tools-preview 和 RV target；user/、fs/src/ext4_rs/ 也锁定该 nightly。 | 仓库含 nightly feature；现场先确认 toolchain，不要任意换 stable。 |

入口文件：

- [os/src/main.rs](../../os/src/main.rs)：内核 no_std 和 alloc。
- [fs/src/lib.rs](../../fs/src/lib.rs)：文件系统 no_std 模块及公开导出。
- [user/src/lib.rs](../../user/src/lib.rs)：用户堆和 __user_start。
- [fs-fuse/src/pack/easyfs.rs](../../fs-fuse/src/pack/easyfs.rs)：主机 std 的 BlockDevice 实现。
- [rust-toolchain.toml](../../rust-toolchain.toml)、[user/rust-toolchain.toml](../../user/rust-toolchain.toml)：工具链锁定。

### 2.2 离线检查命令

这些命令只描述仓库已有入口；成功还取决于现场是否缓存依赖、target 和 LLVM 组件。

~~~sh
# 适用：主机 shell；核对 RV/LA 的 nightly 和 target
rustup run nightly-2025-01-18 rustc --version
rustup target list --installed

# 适用：仓库根目录 shell；RV/LA 均可
make cargo-config

# 适用：no_std/内核，RV
CARGO_NET_OFFLINE=true make -C os kernel ARCH=riscv64

# 适用：no_std/内核，LA
CARGO_NET_OFFLINE=true make -C os kernel ARCH=loongarch64

# 适用：no_std/用户态；构建 RV/LA 的 user/src/bin
CARGO_NET_OFFLINE=true make -C user build ARCH=riscv64
CARGO_NET_OFFLINE=true make -C user build ARCH=loongarch64
~~~

语言级快速检查可以用：

~~~sh
# 适用：no_std/内核；目标来自 os/Makefile
cd os
CARGO_NET_OFFLINE=true cargo check --offline \
  --target riscv64gc-unknown-none-elf \
  --no-default-features \
  --features ext4,platform-qemu-virt,legacy-vdb-names,trap_context_cache,process_identity_cache,return_work_cache,current_task_cache
~~~

注意：

- make -C os kernel ARCH=... 的 release recipe 还会按 QEMU_MAJOR 决定是否传 --cfg qemu7；手写 cargo check 不是完整 link/启动验证。
- os/Makefile 的 OFFLINE 主要控制 env 目标是否安装 target、cargo-binutils、rust-src 和 LLVM；它不会自动给 kernel recipe 加 --offline。断网时用 CARGO_NET_OFFLINE=true 或显式 cargo --offline。
- kernel_sleep_mutex 是 fs 依赖的 feature，不是 os 的 feature；内核依赖声明已显式启用它，不要在 os --features 里凭空添加。
- cargo check 仍可能写 Cargo target 目录；想隔离构建产物时可设置 CARGO_TARGET_DIR=/tmp/cosmos-cargo-check。
- make run、make fast-run 还需要内核镜像和测试盘，并会受 RUN_ARCH、TEST_FS、SMP、rootfs feature 影响。

## 3. 核心模型：约束、选择、关系、所有权

### 3.1 Trait 是行为契约

trait 描述一个类型必须提供的行为，可以有默认方法、关联类型、关联常量和超 trait 约束。impl Trait for Type 将行为绑定到具体类型。

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
use core::any::Any;

pub trait BlockBackend: Send + Sync + Any {
    fn as_any(&self) -> &dyn Any;
    fn read_block(&self, block_id: usize, buf: &mut [u8]);
    fn write_block(&self, block_id: usize, buf: &[u8]);

    fn read_blocks(&self, start: usize, buf: &mut [u8]) {
        assert!(buf.len() % 512 == 0);
        for (offset, block) in buf.chunks_mut(512).enumerate() {
            self.read_block(start + offset, block);
        }
    }
}
~~~

这对应仓库 [fs/src/block_dev.rs](../../fs/src/block_dev.rs) 的 BlockDevice；仓库还定义了带生命周期的 BlockRead<'a>、BlockWrite<'a>。

超 trait 的含义：

- Send：值可以移动到另一个线程/hart 的执行上下文。
- Sync：可以通过共享引用从多个线程/hart 安全访问，粗略看作 &T: Send。
- Any：可把 trait object 转成 dyn Any 后 downcast；as_any() 通常是 trait 自己提供的桥接方法，具体类型通常不能含非 'static 借用。
- 具体实现含裸指针、MMIO、DMA 或自定义锁时，先证明并发和别名不变量，再决定是否需要 unsafe impl。

### 3.2 泛型是编译期参数，dyn Trait 是运行期对象

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
fn read_one_generic<D: BlockBackend>(
    device: &D,
    block_id: usize,
    buf: &mut [u8],
) {
    device.read_block(block_id, buf);
}

fn read_one_dyn(device: &dyn BlockBackend, block_id: usize, buf: &mut [u8]) {
    device.read_block(block_id, buf);
}
~~~

| 写法 | 调度 | 取舍 |
| --- | --- | --- |
| D: Trait | 编译期单态化，通常可内联 | 可能增加代码体积；类型必须在调用点确定。 |
| &dyn Trait/Arc<dyn Trait> | vtable 间接调用 | 后端可替换、可放异构容器；有一次间接调用。 |

当前仓库的真实例子是 [os/src/drivers/block/mod.rs](../../os/src/drivers/block/mod.rs) 的 BTreeMap<String, Arc<dyn BlockDevice>>，以及 [fs/src/vfs.rs](../../fs/src/vfs.rs) 的 VfsNode。VirtIO、EasyFS、FAT32、ext4、procfs、tmpfs 都可以在这些边界后提供实现。

dyn Trait 是一个胖指针概念：数据指针加 vtable 指针。它不是“任何类型都能转”，trait 必须对 trait object 兼容（旧资料常称 object safe）。

### 3.3 生命周期描述借用关系

~~~rust
// 适用：std 或 no_std+alloc；用户态/内核态均可；RV/LA 无关
fn first_byte<'a>(bytes: &'a [u8]) -> Option<&'a u8> {
    bytes.first()
}
~~~

'a 表示返回引用不能超过输入借用的有效区间，不是计时器。若返回 Vec<u8>、Box<[u8]> 或 Arc<[u8]>，就能把拥有权带出函数。

guard 也是生命周期设计：

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可；实际形状见 fs/src/sleep_mutex.rs
pub struct Guard<'a, T> {
    lock: &'a SleepMutex<T>,
}
~~~

guard 借用锁本身，所以不会比锁活得久；Drop 时释放资源。

### 3.4 智能指针分工

| 问题 | 工具 |
| --- | --- |
| 单一拥有者、堆分配 | Box<T> |
| 同线程多所有者 | Rc<T> |
| 多线程/hart 多所有者 | Arc<T> |
| 不阻止释放的观察引用 | Weak<T> |
| 单线程运行时借用检查 | RefCell<T> |
| 多执行流共享可变状态 | Arc<锁<T>>、原子类型或内核同步封装 |

## 4. Trait、泛型、where、associated type

### 4.1 默认方法与实现

~~~rust
// 适用：no_std/内核、no_std/fs、no_std/用户态；RV/LA 无关
pub trait ReadAt {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;

    fn read_all(&self, buf: &mut [u8]) -> usize {
        self.read_at(0, buf)
    }
}

pub struct MemoryFile {
    data: &'static [u8],
}

impl ReadAt for MemoryFile {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.data.len() {
            return 0;
        }
        let n = core::cmp::min(buf.len(), self.data.len() - offset);
        buf[..n].copy_from_slice(&self.data[offset..offset + n]);
        n
    }
}
~~~

仓库同类设计：

- [fs/src/vfs.rs](../../fs/src/vfs.rs) 的 VfsNode 有 find、create、read_at、write_at、stat_attrs；可选能力用默认 EOPNOTSUPP/ENOSYS。
- [os/src/fs/mod.rs](../../os/src/fs/mod.rs) 的 File trait 为不同文件对象提供读写、poll、stat，并用 as_any() 处理少数具体类型分支。
- [os/src/hal/traits.rs](../../os/src/hal/traits.rs) 只描述架构无关接口，具体实现放在 os/src/arch/riscv/ 与 os/src/arch/loongarch64/。

默认方法适合“能力可选且有合理默认错误”，不适合掩盖必须实现的安全或一致性逻辑。当前 VfsNode::read_at、write_at 由后端必须实现。

### 4.2 where 约束

~~~rust
// 适用：std 或 no_std；用户态/内核态均可；RV/LA 无关；示例只用 core trait
fn copy_twice<T: Copy>(x: T) -> (T, T) {
    (x, x)
}

fn copy_twice_where<T>(x: T) -> (T, T)
where
    T: Copy,
{
    (x, x)
}
~~~

复杂共享状态约束：

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
use alloc::sync::Arc;
use core::any::Any;

fn install<T>(state: Arc<T>)
where
    T: Any + Send + Sync,
{
    let _ = state;
}
~~~

对应实际方法是 [fs/src/vfs.rs](../../fs/src/vfs.rs) 的 get_or_insert_page_cache_state<T, F>：

~~~rust
// 适用：no_std/fs；RV/LA 均可；省略真实的状态锁和 downcast 细节
pub fn get_or_insert_page_cache_state<T, F>(&self, init: F) -> (Arc<T>, bool)
where
    T: Any + Send + Sync,
    F: FnOnce() -> Arc<T>,
{
    todo!()
}
~~~

FnOnce 表示初始化闭包最多调用一次，适合“检查后只安装一份状态”的流程。

### 4.3 Associated type

关联类型由某个 trait 实现固定，不是每次调用都重新传入的类型参数：

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 无关
pub trait Decoder {
    type Word: Copy;
    fn decode(&self, bytes: &[u8]) -> Option<Self::Word>;
}

struct U16Decoder;

impl Decoder for U16Decoder {
    type Word = u16;

    fn decode(&self, bytes: &[u8]) -> Option<Self::Word> {
        if bytes.len() < 2 {
            None
        } else {
            Some(u16::from_le_bytes([bytes[0], bytes[1]]))
        }
    }
}
~~~

仓库的两个关键实例：

1. TrapContextAbi 在 [os/src/hal/traits.rs](../../os/src/hal/traits.rs) 声明 type Frame: Copy；RISC-V 指定 RiscvTrapContextFrame，LoongArch 指定 LoongArchTrapContextFrame。
2. PagingArch 声明 type Entry: Copy 和 PA_BITS、VA_BITS、LEVELS 等关联常量；RV/LA 都将 Entry 指向 PageTableEntry，但 token、PTE、TLB 操作不同。

泛型代码通过 Self::Frame 使用公共能力：

~~~rust
// 适用：no_std/内核；RV/LA 均可
use crate::hal::traits::TrapContextAbi;

fn read_user_pc<A: TrapContextAbi>(frame: &A::Frame) -> usize {
    A::user_pc(frame)
}

// 推断不清时：
// let pc = <RiscvTrapContextAbi as TrapContextAbi>::user_pc(&frame);
~~~

选择关联类型还是泛型参数：

| 设计 | 含义 | 适合 |
| --- | --- | --- |
| trait Convert { type Output; } | 一个实现对应一个自然输出 | TrapContextAbi::Frame、Iterator::Item |
| trait Convert<O> { ... } | 同一个实现可对应多个 O | 一个类型确实支持多种输出协议 |

### 4.4 dyn-compatible 检查

以下 trait 不能直接作为 dyn Trait；泛型方法和返回 Self 是原因：

~~~rust
// 适用：std 或 no_std；用户态/内核态均可；RV/LA 无关；故意展示不兼容方法
trait NotDynCompatible {
    fn generic_method<T>(&self, value: T);
    fn make_self(&self) -> Self;
}
~~~

若泛型方法只给静态分发使用，可以限制为 Self: Sized：

~~~rust
// 适用：std 或 no_std；用户态/内核态均可；RV/LA 无关
trait MostlyDynCompatible {
    fn name(&self) -> &'static str;

    fn generic_method<T>(&self, value: T)
    where
        Self: Sized,
    {
        let _ = value;
    }
}
~~~

此时 name 可通过 &dyn MostlyDynCompatible 调用，generic_method 不可。遇到 E0038 时：

1. 查泛型方法、fn ... -> Self、带 Self 的非 Self: Sized 参数。
2. 确认是否误把只适合静态分发的 trait 放入 Arc<dyn Trait>。
3. 确认 associated type 是否需要绑定，如 dyn Iterator<Item = u8>。
4. 如果后端本来就是编译期固定，改为 fn f<T: Trait>(x: &T)。

### 4.5 Any 和向下转型

当前 VFS 用 trait object 保持统一接口，用 Any 处理少数后端专属分支：

~~~rust
// 适用：no_std/fs；RV/LA 均可；抽象自 fs/src/vfs.rs
use alloc::sync::Arc;
use core::any::Any;

trait Node: Send + Sync + Any {
    fn as_any(&self) -> &dyn Any;
}

struct RegularNode;

impl Node for RegularNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn only_regular(node: &dyn Node) -> bool {
    node.as_any().downcast_ref::<RegularNode>().is_some()
}

fn take_erased<T: Any + Send + Sync>(state: Arc<dyn Any + Send + Sync>) -> Option<Arc<T>> {
    state.downcast::<T>().ok()
}
~~~

实际路径：

- VfsNode: Send + Sync + Any + Debug，所有实现提供 as_any()。
- ext4 rename/link 用 as_any().downcast_ref::<Self>() 确认后端类型。
- Inode 保存 Option<Arc<dyn Any + Send + Sync>> 作为 page-cache 类型擦除状态，再 downcast::<T>() 取回。

反模式是把所有业务塞进 Any 再到处 downcast；优先把稳定能力放进 trait，只把少量后端专属操作保留为 downcast。

## 5. 生命周期与借用设计

### 5.1 省略规则和显式关系

| 形状 | 常见推断 |
| --- | --- |
| fn f(x: &T) -> &T | 返回值与唯一输入借用相同 |
| fn f(&self) -> &T | 返回值与 self 借用相同 |
| 多个输入且返回引用 | 通常要显式命名生命周期 |
| fn f(x: &'a T) -> &'a T | 返回借用不超过 'a |
| struct S<'a> { p: &'a T } | 结构体保存借用，必须有 'a |

~~~rust
// 适用：std 或 no_std；用户态/内核态均可；RV/LA 无关
fn same_slice(x: &[u8]) -> &[u8] {
    x
}

fn choose<'a>(left: &'a [u8], right: &'a [u8], use_left: bool) -> &'a [u8] {
    if use_left { left } else { right }
}
~~~

choose 的标注不是延长引用，而是表达返回值位于两个输入借用的共同有效区间。

### 5.2 CosmOS 的批量借用

[fs/src/block_dev.rs](../../fs/src/block_dev.rs) 的真实设计：

~~~rust
// 适用：no_std/fs；RV/LA 均可
pub struct BlockWrite<'a> {
    pub start_block: usize,
    pub data: &'a [u8],
}

pub struct BlockRead<'a> {
    pub start_block: usize,
    pub data: &'a mut [u8],
}

pub trait BlockDevice {
    fn read_blocks_many(&self, reads: &mut [BlockRead<'_>]);
    fn write_blocks_many(&self, writes: &[BlockWrite<'_>]);
}
~~~

该 API 表示同步调用：设备不能保存脱离调用者的 data 借用，返回后调用者仍拥有 buffer。若改成异步，不能只把函数名改成 submit，必须改变所有权：

- 请求拥有 Box<[u8]> 或 Arc<[u8]>；
- 调用方提供 'static buffer，并定义完成/回收协议；
- 或使用有明确 pin/DMA invariant 的地址/长度协议，这需要窄范围 unsafe。

### 5.3 Guard 作用域

~~~rust
// 适用：no_std/内核；RV/LA 均可；用 os::sync::SleepMutex 的形状演示
fn update_and_do_io(state: &SleepMutex<State>) {
    {
        let mut guard = state.lock();
        guard.prepare();
        // 不把 guard 带进可能重新拿同锁的阻塞 I/O。
    } // guard Drop，锁释放

    do_blocking_io();
}
~~~

遇到 E0499/E0502 时，通常不是增加生命周期参数，而是：

1. 用 { ... } 缩小 guard/临时引用。
2. 第二次借用前显式 drop(guard)。
3. 先复制标量、枚举、Arc::clone，再释放锁。
4. 把检查/读取和修改/回写拆开。
5. 返回拥有数据，或使用 split_at_mut 等已证明安全的切分 API。

### 5.4 move、线程/回调和 'static

保存闭包的线程/回调常要求 'static，意思是其中不能有非静态借用，不是“永远不释放”。

~~~rust
// 适用：std/主机工具或 no_std/用户态；内核需替换线程 API；RV/LA 无关；只表达所有权关系
fn spawn_owned(name: alloc::string::String) {
    let task = move || {
        let _ = name;
    };
    let _ = task;
}
~~~

[user/src/bin/tcp_echo_server.rs](../../user/src/bin/tcp_echo_server.rs) 用 Box::into_raw(Box::new(WorkerCtx { ... })) 把地址传给 thread_create。这是绕过借用检查的手动协议，必须证明：

- Box 在 worker 最后一次访问前一直存在；
- 地址没有移动、提前释放或重复释放；
- thread_create 失败时的泄漏是否有意；
- worker 退出时是否需要 Box::from_raw 回收。

不要把普通栈变量地址交给可能在函数返回后执行的任务。

### 5.5 Arc 与生命周期不是一回事

Arc::clone(&x) 克隆强引用计数，不复制底层对象；&Arc<T> 只是借用一个句柄。

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
use alloc::sync::Arc;

fn keep_device<D: ?Sized>(device: &Arc<D>) -> Arc<D> {
    Arc::clone(device)
}
~~~

如果错误要求 'static，先问谁要保存引用：

- 只在本次调用使用：API 用 &T 或 &'a T。
- 要跨线程/调用保存：让对象拥有数据，使用 Box、Arc、Vec，或让保存者带 'a。
- 真正全局且永不移动的内存才考虑 'static。
- PhysAddr::get_ref<T>() -> &'static T（[os/src/mm/address.rs](../../os/src/mm/address.rs)）是 unsafe 的映射承诺，不能作为普通业务的逃生门。

## 6. 智能指针和同步封装

### 6.1 Box、Rc、Arc、Weak、RefCell

| 类型 | no_std 可用性 | 语义 | 当前仓库建议 |
| --- | --- | --- | --- |
| Box<T> | alloc::boxed::Box | 单一拥有者、堆对象 | 稳定堆对象、Box<dyn Trait>、FFI 指针。 |
| Rc<T> | alloc::rc::Rc | 非原子计数；单线程；不是 Send/Sync | no_std+alloc 可以用，但不要用于内核 SMP 共享。 |
| Arc<T> | alloc::sync::Arc | 原子计数、多执行流共享所有权 | 内核设备、VFS、inode、page cache、任务对象的首选句柄。 |
| Weak<T> | alloc::sync::Weak | 不阻止释放；upgrade() 返回 Option | 反向索引、父子关系、缓存队列、回调表。 |
| RefCell<T> | core::cell::RefCell | 单线程运行时借用检查；冲突 panic | 仅单线程局部使用；不是锁。 |

### 6.2 Box 和 FFI

~~~rust
// 适用：no_std/用户态或 no_std/内核；RV/LA 均可；需要 alloc
extern crate alloc;
use alloc::boxed::Box;

struct Context {
    id: usize,
}

fn pass_raw_once(ctx: Box<Context>) -> *mut Context {
    Box::into_raw(ctx)
}

unsafe fn reclaim_raw(ptr: *mut Context) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}
~~~

Box::into_raw 后 Rust 不再自动释放；必须恰好一次 Box::from_raw，或明确记载故意泄漏。裸指针不携带生命周期和线程安全证明。

### 6.3 Rc + RefCell 的边界

~~~rust
// 适用：std 或 no_std+alloc；用户态/内核态局部代码均可；RV/LA 无关；只允许同一线程
extern crate alloc;
use alloc::rc::Rc;
use core::cell::RefCell;

fn local_graph() {
    let value = Rc::new(RefCell::new(0usize));
    *value.borrow_mut() += 1;
    let another = Rc::clone(&value);
    assert_eq!(*another.borrow(), 1);
}
~~~

Rc 计数非原子，不能送到另一个线程；RefCell 的借用状态不是同步机制。即使外面套 Arc，Arc<RefCell<T>> 也不会自动成为 Sync。仓库 [os/src/sync/up.rs](../../os/src/sync/up.rs) 明确说明旧的 RefCell 方案已改成内部使用 SpinNoIrqLock 的 UPSafeCell，以支持 SMP。

### 6.4 Arc + 锁

~~~rust
// 适用：no_std/内核；RV/LA 均可；锁类型按是否允许阻塞选择
use alloc::sync::Arc;
use crate::sync::SpinNoIrqLock;

struct Counters {
    hits: usize,
}

fn hit(counters: &Arc<SpinNoIrqLock<Counters>>) {
    let mut guard = counters.lock();
    guard.hits += 1;
}
~~~

常见错误：

- Arc<RefCell<T>>：内层 RefCell 不是 Sync。
- Arc<dyn Trait>：trait object 或具体实现缺 Send/Sync。
- Arc<Mutex<T>>：T 不满足 Mutex 的 Send 约束，或导入了错误 crate 的 Mutex。

### 6.5 Weak 和回收

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
use alloc::sync::{Arc, Weak};

struct Parent {
    child: Option<Arc<Child>>,
}

struct Child {
    parent: Weak<Parent>,
}

fn parent_if_alive(child: &Child) -> Option<Arc<Parent>> {
    child.parent.upgrade()
}
~~~

仓库模式：

- [os/src/fs/page_cache.rs](../../os/src/fs/page_cache.rs)：CachePage 弱指向 PageMapping，PageMapping 弱指向 Inode，inactive 队列保存弱引用。
- [os/src/mm/memory_set.rs](../../os/src/mm/memory_set.rs)：file mapping registry 保存 Weak<ProcessControlBlock>，不阻止进程释放。
- [os/src/task/task.rs](../../os/src/task/task.rs)：TCB 对 process 保存 Weak，避免任务和进程互相强持有。
- [fs/src/inode_cache.rs](../../fs/src/inode_cache.rs)、[fs/src/dentry_cache.rs](../../fs/src/dentry_cache.rs)：缓存强持有热对象，但只在引用关系允许时回收。

Weak::upgrade() 返回 None 是正常生命周期分支：对象可能已经被最后一个强引用释放，调用方要决定跳过、重建或返回错误。

### 6.6 锁类型必须按语义选择

| 类型/路径 | 竞争行为 | 中断/阻塞语义 | 适用 | 禁忌 |
| --- | --- | --- | --- | --- |
| spin::Mutex<T>（fs） | 自旋 | 不要假设它会关闭内核中断 | EasyFS/FAT32 短临界区 | 不要持锁跨 block I/O。 |
| fs::sleep_mutex::SleepMutex<T> | kernel_sleep_mutex 开启时 C hook；否则自旋 fallback | 是否真正睡眠取决于 feature/链接 | fs block cache、可跨 I/O 状态 | 独立 fs 测试未接内核 hook 时不要假设可睡眠。 |
| os::sync::SpinLock<T> | 原子自旋 | 不自动关中断 | 短、不会被中断重入的临界区 | 中断可能重入同锁时不要用。 |
| os::sync::SpinNoIrqLock<T> | 原子自旋 | 关本地 supervisor interrupt，Drop 恢复 | scheduler、页表、短硬件状态 | 不要跨 I/O、调度或睡眠。 |
| os::sync::SleepMutex<T> | 竞争时放入 WaitQueue | 不关本地中断，可跨 I/O | 文件系统、FileDescription | 无 current task 时竞争获取会断言；中断路径不要阻塞。 |
| UPSafeCell<T>/UPIntrFreeCell<T> | 当前实现也是原子自旋 | exclusive_access 关本地中断 | legacy static 独占访问 | 名字带 UP 不能忽略当前 SMP 实现。 |
| std::sync::Mutex<T> | 主机线程阻塞，lock 返回 Result | 主机 OS 语义 | 仅 fs-fuse | 不要导入内核 no_std。 |

实现位置：

- [os/src/sync/spin.rs](../../os/src/sync/spin.rs)：SpinLock、SpinNoIrqLock 和 guard Drop。
- [os/src/sync/sleep_mutex.rs](../../os/src/sync/sleep_mutex.rs)：可睡眠 mutex，允许跨文件/块 I/O。
- [fs/src/sleep_mutex.rs](../../fs/src/sleep_mutex.rs)：kernel_sleep_mutex cfg、C hook 和 fallback。
- [os/src/sync/up.rs](../../os/src/sync/up.rs)：UPSafeCell、UPIntrFreeCell。
- [fs-fuse/src/pack/easyfs.rs](../../fs-fuse/src/pack/easyfs.rs)：主机 Mutex 的 lock().unwrap()；spin Mutex 没有同样的 poisoning Result。

### 6.7 guard 最小作用域

~~~rust
// 适用：no_std/内核；RV/LA 均可；把 crate::sync 换成实际模块路径
use crate::sync::SpinNoIrqLock;

struct Cache {
    bytes: usize,
}

fn read_then_io(cache: &SpinNoIrqLock<Cache>) {
    let bytes = {
        let guard = cache.lock();
        guard.bytes
    }; // 先释放 SpinNoIrqLock

    submit_or_wait_for_io(bytes);
}

fn submit_or_wait_for_io(_bytes: usize) {}
~~~

不要在 guard 仍存活时再次锁同一对象、调用可能睡眠的路径，或把 guard 的引用转裸指针延长生命周期。需要跨 I/O 时改用可睡眠锁，并仍然只保留必要状态。

### 6.8 VFS/设备的实际组合

~~~rust
// 适用：no_std/fs；RV/LA 均可；抽象自 fs/src/easyfs/efs.rs 和 fat32/mod.rs
// 下面只展示所有权/锁的形状；BlockDevice、Metadata、Inode 的具体定义和构造签名
// 必须以当前后端源码为准，不能把此片段当作完整可编译实现。
use alloc::sync::Arc;
use spin::Mutex;

struct FileSystem {
    block_device: Arc<dyn BlockDevice>,
    state: Mutex<Metadata>,
}

impl FileSystem {
    fn root(fs: &Arc<Mutex<Self>>) -> Inode {
        let device = {
            let guard = fs.lock();
            Arc::clone(&guard.block_device)
        };
        Inode::new(Arc::clone(fs), device)
    }
}
~~~

实际代码中：

- EasyFileSystem 保存 Arc<dyn BlockDevice>，create/open 返回 Arc<Mutex<Self>>，root_inode 接受 &Arc<Mutex<Self>>。
- Fat32FileSystem 保存 Arc<dyn BlockDevice> 和 spin::Mutex<Fat32Inner>，root_inode 返回 Arc<Inode>。
- ext4 适配层使用后端 trait object，并用 Weak<Mutex<()>> 做 inode 级锁表。

## 7. 文件路径与函数/类型映射

| 主题 | 路径 | 关键符号/事实 |
| --- | --- | --- |
| 内核 no_std | [os/src/main.rs](../../os/src/main.rs) | #![no_std]、extern crate alloc |
| 文件系统 no_std | [fs/src/lib.rs](../../fs/src/lib.rs) | BlockDevice、VfsNode、Inode 导出 |
| 用户 no_std | [user/src/lib.rs](../../user/src/lib.rs) | 用户堆、__user_start |
| 主机适配器 | [fs-fuse/src/pack/easyfs.rs](../../fs-fuse/src/pack/easyfs.rs) | BlockFile(Mutex<File>)、std Arc/Mutex |
| 块设备 trait | [fs/src/block_dev.rs](../../fs/src/block_dev.rs) | BlockDevice: Send + Sync + Any、BlockRead/BlockWrite 生命周期 |
| VirtIO 实现 | [os/src/drivers/block/virtio_blk.rs](../../os/src/drivers/block/virtio_blk.rs) | impl BlockDevice for VirtIOBlock |
| 设备注册表 | [os/src/drivers/block/mod.rs](../../os/src/drivers/block/mod.rs) | BTreeMap<String, Arc<dyn BlockDevice>> |
| VFS trait | [fs/src/vfs.rs](../../fs/src/vfs.rs) | VfsNode: Send + Sync + Any + Debug、默认方法、as_any |
| inode 包装 | [fs/src/vfs.rs](../../fs/src/vfs.rs) | Inode 的 Arc<dyn VfsNode> 与 Mutex 状态 |
| 类型擦除 cache | [fs/src/vfs.rs](../../fs/src/vfs.rs) | page_cache_state<T>、get_or_insert_page_cache_state<T,F> |
| EasyFS | [fs/src/easyfs/efs.rs](../../fs/src/easyfs/efs.rs) | Arc<dyn BlockDevice>、Arc<Mutex<Self>> |
| EasyFS inode | [fs/src/easyfs/inode.rs](../../fs/src/easyfs/inode.rs) | impl VfsNode for EasyInode |
| FAT32 | [fs/src/fat32/mod.rs](../../fs/src/fat32/mod.rs) | Arc<Fat32FileSystem>、spin::Mutex<Fat32Inner> |
| 泛型 block cache | [fs/src/block_cache.rs](../../fs/src/block_cache.rs) | get_ref<T>、get_mut<T>、read<T,V>、modify<T,V> |
| 架构无关 HAL | [os/src/hal/traits.rs](../../os/src/hal/traits.rs) | TrapContextAbi::Frame、PagingArch::Entry |
| RV 实现 | [os/src/arch/riscv/trap.rs](../../os/src/arch/riscv/trap.rs) | type Frame = RiscvTrapContextFrame |
| LA 实现 | [os/src/arch/loongarch64/trap.rs](../../os/src/arch/loongarch64/trap.rs) | type Frame = LoongArchTrapContextFrame |
| 泛型页号范围 | [os/src/mm/address.rs](../../os/src/mm/address.rs) | SimpleRange<T>、StepByOne、VPNRange |
| 页表根生命周期 | [os/src/mm/page_table.rs](../../os/src/mm/page_table.rs) | AddressSpaceRoot 内 Arc<PageTableRootFrame> |
| page cache 弱引用 | [os/src/fs/page_cache.rs](../../os/src/fs/page_cache.rs) | Weak<Inode>、Weak<PageMapping>、弱 inactive 队列 |
| 内核 spin lock | [os/src/sync/spin.rs](../../os/src/sync/spin.rs) | SpinLock、SpinNoIrqLock、guard |
| 内核 sleep lock | [os/src/sync/sleep_mutex.rs](../../os/src/sync/sleep_mutex.rs) | SleepMutex、WaitQueue |
| fs sleep shim | [fs/src/sleep_mutex.rs](../../fs/src/sleep_mutex.rs) | kernel_sleep_mutex cfg、C hook/fallback |
| 用户 Box/FFI | [user/src/bin/tcp_echo_server.rs](../../user/src/bin/tcp_echo_server.rs) | Box::into_raw、thread_create |

## 8. CosmOS 抽象设计模板

### 8.1 泛型算法 + dyn 后端

~~~rust
// 适用：no_std/内核或 no_std/fs；RV/LA 均可
extern crate alloc;
use alloc::sync::Arc;
use core::any::Any;

pub trait Storage: Send + Sync + Any {
    fn as_any(&self) -> &dyn Any;
    fn read_block(&self, id: usize, out: &mut [u8]);
    fn write_block(&self, id: usize, data: &[u8]);
}

pub struct Cache<D: Storage + ?Sized> {
    device: Arc<D>,
}

impl<D: Storage + ?Sized> Cache<D> {
    pub fn new(device: Arc<D>) -> Self {
        Self { device }
    }

    pub fn read(&self, id: usize, out: &mut [u8]) {
        self.device.read_block(id, out);
    }
}

fn use_static_backend<D: Storage>(device: Arc<D>) {
    let cache = Cache::new(device);
    let mut block = [0u8; 512];
    cache.read(0, &mut block);
}

fn use_runtime_backend(device: Arc<dyn Storage>) {
    let cache: Cache<dyn Storage> = Cache::new(device);
    let mut block = [0u8; 512];
    cache.read(0, &mut block);
}
~~~

?Sized 很关键：默认泛型隐含 Sized，而 dyn Storage 是 unsized；保存 trait object 的 Arc<D> 要写 D: ?Sized。

### 8.2 借用型同步批量接口

~~~rust
// 适用：no_std/fs；RV/LA 均可；同步调用，返回前完成 buffer 访问
pub struct ReadRequest<'a> {
    pub start_block: usize,
    pub data: &'a mut [u8],
}

pub trait BatchReader {
    fn read_many(&self, requests: &mut [ReadRequest<'_>]);
}

fn read_two<R: BatchReader>(reader: &R, first: &mut [u8], second: &mut [u8]) {
    let mut requests = [
        ReadRequest { start_block: 0, data: first },
        ReadRequest { start_block: 1, data: second },
    ];
    reader.read_many(&mut requests);
}
~~~

如果未来改为异步，必须同时改变 buffer 所有权和完成通知，不要只把方法名改成 submit。

### 8.3 受锁保护的闭包

~~~rust
// 适用：no_std/内核；RV/LA 均可；示例使用 CosmOS SpinNoIrqLock
use crate::sync::SpinNoIrqLock;

pub struct Locked<T> {
    inner: SpinNoIrqLock<T>,
}

impl<T> Locked<T> {
    pub const fn new(value: T) -> Self {
        Self { inner: SpinNoIrqLock::new(value) }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut guard = self.inner.lock();
        f(&mut *guard)
    }
}
~~~

闭包内不能做阻塞 I/O、重新进入同一把锁或长时间计算；需要跨 I/O 时改为可睡眠锁，并把锁内工作压到最小。

### 8.4 架构无关 trait + associated type

~~~rust
// 适用：no_std/内核；RV/LA 公共代码
pub trait TrapAbi {
    type Frame: Copy;
    fn user_pc(frame: &Self::Frame) -> usize;
    fn set_user_pc(frame: &mut Self::Frame, pc: usize);
}

fn advance_pc<A: TrapAbi>(frame: &mut A::Frame, len: usize) {
    let next = A::user_pc(frame) + len;
    A::set_user_pc(frame, next);
}
~~~

公共代码不要假定 frame 有 RV 的 x[32] 或 LA 的 r[32] 字段；真实仓库用 TrapContextAbi 方法访问寄存器来保持边界。

## 9. 常见错误：现象 → 原因 → 检查 → 修复

### 9.1 std 混入 no_std

**现象**：找不到 std，或 std::sync::Mutex::lock().unwrap() 在内核失败。

**原因**：os、fs、user 都是 no_std；只有 fs-fuse 是 host std。

**检查：**

~~~sh
# 适用：仓库根目录 shell；RV/LA 均可
rg -n 'use std::|std::sync|std::fs|extern crate std' os/src fs/src user/src
sed -n '1,45p' os/src/main.rs
sed -n '1,40p' fs/src/lib.rs
~~~

**修复**：用 alloc 的 String、Vec、Box、Arc；按 crate 选择 spin::Mutex 或 crate::sync。主机 std 代码不要反推到内核。

### 9.2 trait 没有实现（E0277/E0599）

**现象**：方法存在但 trait bound 不满足，或 Arc::new(Concrete) 无法转为 Arc<dyn Trait>。

**原因**：impl 被 feature/target cfg 排除、方法签名不完全匹配、模块没导入、具体类型缺 Send/Sync、泛型 where 不完整。

**检查：**

~~~sh
# 适用：no_std/内核或 no_std/fs；RV/LA 均可
rg -n 'trait BlockDevice|impl BlockDevice for|trait VfsNode|impl VfsNode for' fs/src os/src
rg -n '#\\[cfg|feature = "ext4"|feature = "kernel_sleep_mutex"' fs/src os/src
~~~

依次确认 impl 每个参数/返回值/receiver 完全匹配；模块被 mod 引入；当前 target/feature 没排除实现；trait object 的 Send、Sync、Any 满足；where 只补最小 bound。

### 9.3 Send/Sync 错误

**现象**：T cannot be sent/shared between threads safely。

**原因**：共享边界内有 Rc、RefCell、裸指针或非线程安全外部类型；Arc 的原子计数不改变 T 的线程语义。

**检查：**

~~~rust
// 适用：std 或 no_std；用户态/内核态均可；RV/LA 无关；小探针中检查 auto trait
fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

fn check<T: Send + Sync>() {
    assert_send::<T>();
    assert_sync::<T>();
}
~~~

从最内层字段向外拆：Arc 内的 Mutex/RefCell，再到锁内字段，最后到裸指针、DMA、MMIO 的安全不变量。

**修复**：单线程局部才用 Rc；多 hart 可变状态用 Arc<SpinNoIrqLock<T>> 或 Arc<SleepMutex<T>>；硬件 wrapper 只有证明 invariant 后才能像 os/src/sync/spin.rs 那样写 unsafe impl。

### 9.4 dyn incompatible（E0038）

**现象**：声明 Arc<dyn Trait> 失败，或 trait object 上某方法不可调用。

**原因**：泛型方法、非 Self: Sized 的返回 Self、vtable 无法确定的签名。

**检查/修复**：

1. 搜索泛型方法、-> Self、参数中的 Self。
2. 只给静态分发的方法加 where Self: Sized。
3. 需要运行时调用的方法改为具体参数/返回值。
4. 固定后端改用泛型；VFS/设备边界才保留 dyn。

### 9.5 借用和生命周期错误（E0499/E0502/E0597/E0515/E0716）

**现象**：借用太久、返回局部变量引用、临时值被释放、同时可变借用。

**检查**：

1. 看错误箭头标出的第一次借用在哪里开始/结束。
2. 找 let guard、let ref、match 临时值是否延长作用域。
3. 检查返回值是否引用局部 Vec、String、guard。
4. 检查线程/回调是否真的要求 'static。

**修复**：小作用域、显式 drop、复制标量和 Arc 句柄、返回拥有数据、用 move 移入拥有值、拆分“先查再改”。

### 9.6 RefCell 运行时 panic

**现象**：already borrowed、BorrowMutError；编译通过但 borrow_mut panic。

**原因**：同线程仍有旧 Ref/RefMut，或把 RefCell 当多线程锁。

**修复**：缩小 ref 作用域并显式 drop；跨 hart 共享改用 SpinNoIrqLock、SleepMutex 或 UPSafeCell。

### 9.7 锁死、卡住、无法调度

**现象**：QEMU 无输出、syscall 不返回、SMP 下偶发卡死。

**原因**：spin lock 持锁跨 I/O/调度；中断重入同锁；锁顺序反转；SleepMutex 在没有 current task 时竞争。

**检查：**

~~~sh
# 适用：现场源码审查；RV/LA 均可
rg -n 'SpinNoIrqLock|SpinLock|SleepMutex|spin::Mutex|\\.lock\\(\\)' os/src fs/src
rg -n 'read_block|write_block|wait_with_reason|suspend_current|schedule|page_fault' os/src fs/src
~~~

为每把锁记录持锁后的调用链和全局顺序。SpinNoIrqLock/spin Mutex 不跨阻塞；可跨文件/块 I/O 的状态用 os::sync::SleepMutex 或正确 feature 下的 fs sleep shim。

### 9.8 block cache 泛型转换风险

[fs/src/block_cache.rs](../../fs/src/block_cache.rs) 的 get_ref<T>/get_mut<T> 用泛型 T 做裸指针转换。泛型不会自动证明大小、对齐、初始化、布局、字节序和别名安全。

检查 size_of::<T>() 是否在块内、偏移是否满足 align_of::<T>()、磁盘格式是否与 repr/填充一致、是否有多个可变引用。非对齐 FAT32 BPB/目录项优先使用当前代码的 read_bytes 并按字节解析；把 unsafe 缩到最窄边界并写清 invariant。

### 9.9 Arc/Weak 缓存回收异常

检查 [fs/src/inode_cache.rs](../../fs/src/inode_cache.rs) 的 strong_count/reclaim、[fs/src/dentry_cache.rs](../../fs/src/dentry_cache.rs) 的 positive dentry、[os/src/fs/page_cache.rs](../../os/src/fs/page_cache.rs) 的 Weak owner 和 dirty retain。

反向关系改 Weak；删除/rename 后按稳定 key 失效 cache；inode 号会复用时加入 generation 或显式失效协议。strong_count 只能辅助诊断/回收，不能代替正确性协议。

## 10. 编译器错误现场定位

### 10.1 先分类，不要先乱改生命周期

1. 解析/导入：缺 crate、trait import、feature/cfg。
2. 类型推断：E0282、E0308、E0107；先确定 T、associated type 和 object type。
3. trait bound：E0277、E0599；沿 required by 补最小约束。
4. dyn compatibility：E0038。
5. 借用/生命周期：E0499、E0502、E0597、E0515、E0505、E0716。
6. Send/Sync：从最内层字段拆解。
7. 链接/架构：这是 target、linker 或工具链问题，不要改 trait 来修。

### 10.2 最小探针和解释命令

~~~sh
# 适用：std/主机工具；RV/LA 无关；不依赖 Cargo registry，隔离 trait/生命周期语法
rustc --edition=2021 --crate-type=lib --emit=metadata probe.rs

# 适用：主机 shell；RV/LA 无关；当前 nightly，查看编译器自带解释
rustc --explain E0277
rustc --explain E0038
rustc --explain E0499
rustc --explain E0597
~~~

探针只保留一个 trait/impl、一个泛型函数或 Arc<dyn Trait>、一个局部 buffer/guard；通过后再接回 VFS、驱动、宏和链接脚本。

### 10.3 仓库快速搜索

~~~sh
# 适用：仓库根目录 shell；RV/LA 均可；断网可用
rg -n 'trait |impl .* for|where |type Frame|type Entry|dyn ' os/src fs/src user/src
rg -n 'alloc::(boxed::Box|rc::Rc|sync::(Arc|Weak))|RefCell|SpinNoIrqLock|SleepMutex|spin::Mutex' os/src fs/src user/src
rg -n 'Arc::downgrade|Weak::upgrade|Arc::clone|Box::into_raw|Box::from_raw' os/src fs/src user/src
~~~

### 10.4 工具链错误和语言错误

| 现象 | 可能原因 | 方向 |
| --- | --- | --- |
| can't find crate for core | target 未安装或 toolchain 不对 | 检查 rustup target 和 nightly-2025-01-18。 |
| 依赖下载失败/找不到 riscv crate | Cargo cache/config 不完整 | make cargo-config，检查 vendor/path 依赖和离线 cache。 |
| use of unstable feature | 没使用仓库要求的 nightly | 看根/user/ext4 的 rust-toolchain。 |
| linking with ... failed | target linker、脚本、LLVM 或架构环境 | 先 cargo check，再走 make -C os kernel ARCH=...。 |
| 只有 LA 失败 | LA cfg 分支实现/类型不匹配 | 对照两套 TrapContextAbi/PagingArch。 |
| std 错误只出现在 fs-fuse | 可能正常，这是 host crate | 不要把 host import 规则反推到内核。 |

## 11. 稳定官方资料索引

正文已尽量自包含；以下资料名和入口适合赛前缓存：

- Rust Book：Generics、Traits、Lifetimes、Smart Pointers
  <https://doc.rust-lang.org/book/ch10-00-generics.html>
  <https://doc.rust-lang.org/book/ch10-02-traits.html>
  <https://doc.rust-lang.org/book/ch10-03-lifetime-syntax.html>
  <https://doc.rust-lang.org/book/ch15-00-smart-pointers.html>
- Rust Reference：Trait objects
  <https://doc.rust-lang.org/reference/types/trait-object.html>
- Rustonomicon：Send and Sync
  <https://doc.rust-lang.org/nomicon/send-and-sync.html>
- API 名：alloc::boxed::Box、alloc::rc::Rc、alloc::sync::Arc、alloc::sync::Weak、core::cell::RefCell。

这些链接是离线资料索引，不代表现场可以联网；具体行为以当前 nightly 的 rustc --explain、仓库源码和 Cargo feature 为准。

## 12. 最后现场 checklist

### 编译前

- [ ] 工具链为 nightly-2025-01-18；target 是 riscv64gc-unknown-none-elf 或 loongarch64-unknown-none。
- [ ] 已确认代码属于 os、fs、user 还是 fs-fuse，没有把 std import 带进 no_std。
- [ ] alloc 路径正确：Box、Arc、Weak、Vec。
- [ ] Rc/RefCell 只用于确定单线程局部设计；多 hart/线程共享使用 Arc + 同步封装。
- [ ] trait 的 Send/Sync/Any/Debug 约束和实现字段实际匹配。
- [ ] 动态后端检查 dyn-compatible；性能/固定后端优先泛型。
- [ ] associated type 与 RV/LA 分支对应；公共代码不访问私有 frame 字段。
- [ ] where 只写真实需要的 bound；不使用无关 Clone/'static 掩盖设计问题。

### 借用与资源

- [ ] 返回引用时能说清借用的拥有者；局部 Vec/String/guard 没有被返回。
- [ ] BlockRead/BlockWrite 的 buffer 生命周期覆盖同步提交和完成。
- [ ] 线程/FFI 参数不是栈引用；Box::into_raw 有明确回收/泄漏协议。
- [ ] Arc::clone 只解决所有权；真正可变状态被锁或原子保护。
- [ ] Weak::upgrade 的 None 分支有重建/跳过/错误处理。
- [ ] SpinNoIrqLock/spin Mutex 没有跨 I/O、睡眠、调度或可能重入的调用。
- [ ] SleepMutex 只在允许阻塞且有 current task 的上下文竞争获取。
- [ ] guard 作用域尽可能短，必要时 I/O 前显式 drop。

### 验证与启动

- [ ] 先 cargo check --offline 或 CARGO_NET_OFFLINE=true make -C os kernel ARCH=...，再做完整 release/link。
- [ ] RV 和 LA 都检查涉及 cfg 的 trait impl，不只凭 RV 通过。
- [ ] 用 rustc --explain 和 rg 定位第一条错误，避免被级联错误带偏。
- [ ] QEMU 启动前确认 kernel、测试盘、RUN_ARCH、SMP、rootfs 和 feature。
- [ ] 卡住时先画锁顺序、检查持锁 I/O/中断重入，再查生命周期/trait。
- [ ] 最后确认没有把 fs-fuse 的 std、Mutex::lock().unwrap() 或 File API 复制进内核。

---

本文对应当前仓库离线资料包主题“Trait、泛型、生命周期与智能指针”。若源代码后来改变，优先以同名 trait、Cargo feature、target 配置和锁实现为准，再更新本页命令和路径。
