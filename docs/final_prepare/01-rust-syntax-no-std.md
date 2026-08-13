# Rust 语法、`core`/`alloc` 与 `no_std` 现场速查（CosmOS）

> 面向 CosmOS Rust OS 现场决赛的离线资料。默认假设：选手在断网环境中编译用户程序或内核，不能临时查询 Codex；应以本仓库源码、锁文件和构建脚本为准。
>
> 本文专属范围：常用 Rust 语法、模式匹配、结构体/枚举、模块、数组/切片、类型转换、`const`/`static`、`core` 与 `alloc`、`no_std` 限制、格式化/日志替代方案，以及对应的编译错误定位。

## 0. 先记住这几条

1. `#![no_std]` 不是“没有 Rust 标准库”：`core` 仍然存在；需要堆容器时显式使用 `alloc`，需要文件、线程、宿主 I/O 的 `std` API 则不能直接使用。
2. CosmOS 的用户态、内核和文件系统库都是 `no_std`，但它们的 crate edition 不同：`user`/`fs` 是 Rust 2018，`os` 是 Rust 2021。语法以实际编译器报错为准，不要把 edition 差异误当成运行时差异。
3. 用户程序不是 Linux 普通 ELF：`user/Makefile` 为它们选择裸机目标、链接脚本和架构汇编。不要在宿主机默认 target 下验证完就认为现场目标一定能链接。
4. CosmOS 的 `print!`/`println!` 是仓库自定义宏，不是 `std` 的宏；格式串位置必须是字面量，例如 `println!("{}", msg)`，不要写 `println!(msg)`。
5. 系统调用包装通常返回 `isize`：负数是错误码，先判断 `< 0`，再转换成 `usize`。直接把负数 `as usize` 会得到一个很大的无符号数。
6. 用户态堆在当前实现中只有 `128 * 1024` 字节；能编译不代表运行时一定有足够内存。内核堆按需增长，但分配仍可能触发页表/锁/回收路径。
7. 需要跨用户态/内核态共享的结构体时使用 `#[repr(C)]`，并明确整数宽度、指针对齐和字节序；不要依赖默认 Rust 布局。

## 1. 本仓库的编译边界与快速索引

### 1.1 目标、edition、架构条件

| 部件 | manifest / edition | 构建目标 | `cfg(target_arch)` | 现场含义 |
| --- | --- | --- | --- | --- |
| 用户库和 `user/src/bin/*.rs` | `user/Cargo.toml` / 2018 | `riscv64gc-unknown-none-elf` 或 `loongarch64-unknown-none` | `riscv64` 或 `loongarch64` | 裸机用户 ELF，不能依赖 `std` |
| 内核 | `os/Cargo.toml` / 2021 | 同上 | `riscv64` 或 `loongarch64` | `no_std + no_main`，自己提供入口、panic、allocator |
| 文件系统库 | `fs/Cargo.toml` / 2018 | 由内核链接 | 通常跟随调用者 | `no_std + alloc`，使用内核提供的全局分配器 |
| 宿主打包工具 | `fs-fuse/Cargo.toml` | 宿主机 target | 宿主环境 | 这是 `std` 工具，不要把它的写法复制到用户/内核 |

准备时在宿主机实际看到的工具链是 `rustc 1.86.0-nightly (6067b3631 2025-01-17)`、`cargo 1.86.0-nightly (088d49608 2025-01-10)`；仓库没有 `rust-toolchain` 固定文件，因此现场以提供的工具链为准。当前源码直接使用 `#![feature(linkage)]`、`#![feature(alloc_error_handler)]` 等特性，不能假定 stable 工具链可以编译。

### 1.2 源码到概念的映射

| 现场要查的内容 | 首选文件 | 当前实现中的关键点 |
| --- | --- | --- |
| 用户 crate 根、入口和用户堆 | [`user/src/lib.rs`](../../user/src/lib.rs) | `#![no_std]`、`extern crate alloc`、`LockedHeap`、`__user_start`、弱 `main`、系统调用高层包装 |
| 用户 panic | [`user/src/lang_items.rs`](../../user/src/lang_items.rs) | 打印位置/消息后调用 `exit(-1)` |
| 用户输出和格式化 | [`user/src/console.rs`](../../user/src/console.rs) | `core::fmt::Write` + `VecDeque<u8>`，`print!`/`println!` 最终调用 `write(STDOUT, ...)` |
| 用户 raw syscall 与双架构汇编 | [`user/src/syscall.rs`](../../user/src/syscall.rs) | RV64 用 `ecall`，LA64 用 `syscall 0`；参数数组分别为 3/6 个 `usize` |
| 用户应用模板 | [`user/src/bin/ls.rs`](../../user/src/bin/ls.rs)、[`user/src/bin/sh.rs`](../../user/src/bin/sh.rs) | `#![no_std]`, `#![no_main]`；入口签名在仓库中有带 `argc/argv` 与无参数两种既有写法 |
| 内核 crate 根和启动流程 | [`os/src/main.rs`](../../os/src/main.rs) | `#![no_std]`, `#![no_main]`, `extern crate alloc`，`rust_main`、`cfg(target_arch)` |
| 内核堆和分配失败 | [`os/src/mm/heap_allocator.rs`](../../os/src/mm/heap_allocator.rs) | `#[global_allocator]`、按需增长、`#[alloc_error_handler]`、`init_heap()` |
| 内核内存初始化 | [`os/src/mm/mod.rs`](../../os/src/mm/mod.rs) | `mm::init()` 先初始化 frame allocator，再初始化堆和地址空间 |
| 内核直接输出 | [`os/src/console.rs`](../../os/src/console.rs) | `core::fmt::Write` 写 UART/early console，带多核自旋锁和关中断保护 |
| 内核日志 | [`os/src/klog.rs`](../../os/src/klog.rs) | `log::Log`、`VecDeque<u8>` 环形缓冲、`LOG` 构建时环境变量、`info!` 等宏 |
| 内核 panic | [`os/src/lang_items.rs`](../../os/src/lang_items.rs) | 输出位置后调用 `sbi::shutdown()` |
| 文件系统 no_std 边界 | [`fs/src/lib.rs`](../../fs/src/lib.rs) | `#![no_std]` + `extern crate alloc`，没有自己的 `#[global_allocator]` |
| target/rustflags | [`user/cargo-config/config.toml`](../../user/cargo-config/config.toml)、[`os/cargo-config/config.toml`](../../os/cargo-config/config.toml) | linker script、RV64 浮点特性、内核 frame pointer |
| 实际构建参数 | [`user/Makefile`](../../user/Makefile)、[`os/Makefile`](../../os/Makefile)、根 [`Makefile`](../../Makefile) | target、release、`--no-default-features`、文件系统 feature、QEMU/镜像路径 |

### 1.3 最小速查表

| 目的 | no_std 写法 | 备注 |
| --- | --- | --- |
| 可变局部变量 | `let mut n = 0usize;` | 默认不可变；`mut` 是绑定属性 |
| 借用 | `&value`, `&mut value` | 同一时刻不能有冲突的可变/不可变借用 |
| 字符串切片 | `&str` | UTF-8 借用，不拥有内容 |
| 拥有的字符串 | `alloc::string::String` | 需要 allocator |
| 字节切片 | `&[u8]`, `&mut [u8]` | syscall buffer 通常使用这两种类型 |
| 动态数组 | `alloc::vec::Vec<T>` | 需要 allocator，`push` 可能重新分配 |
| 固定数组 | `[T; N]` | 栈/静态上大小编译期固定 |
| 核心容器 | `core::...` | 不分配，例：`core::mem`, `core::slice`, `core::fmt` |
| 堆容器 | `alloc::...` | 例：`Vec`, `String`, `Box`, `Arc`, `BTreeMap`, `VecDeque` |
| 格式化参数 | `format_args!("x={}", x)` | 本身不创建 `String`；要看目标 `Write` 实现 |
| 分配格式化 | `alloc::format!("x={}", x)` | 返回 `String`，需要堆空间 |
| 自定义输出 | `use core::fmt::Write as _; write!(dst, "...")` | 不是 `std::io::Write` |
| 可选值 | `Option<T>` / `Some` / `None` | 用 `if let` 或 `match` |
| 可恢复错误 | `Result<T, E>` / `Ok` / `Err` | `?` 要求外层返回兼容的 `Result`/`Option` |
| 系统调用结果 | `isize` | 先检查负值，再转无符号类型 |
| 编译条件 | `#[cfg(target_arch = "riscv64")]` | RV64 与 LA64 必须分别编译 |

## 2. 最常用 Rust 语法

### 2.1 绑定、类型推导和表达式

Rust 的大多数语句也是表达式；块的最后一个无分号表达式是返回值。`if`、`match` 的所有分支必须得到同一类型。

> 适用环境：用户态/内核态/文件系统，`no_std`，RV64 或 LA64；纯语法示例，不依赖 OS API。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
let count = 3usize;              // 不可变绑定
let mut total = 0usize;          // 可变绑定
total += count;

let count = count + 1;           // shadowing：新绑定，可改变类型
let label = if count > 3 { "many" } else { "few" };

let result = match count {
    0 => 0usize,
    1..=4 => count * 10,
    n if n % 2 == 0 => n / 2,
    _ => 1,
};

fn add_one(value: usize) -> usize {
    value + 1                    // 无分号，返回表达式值
}

fn early(value: Option<usize>) -> usize {
    let Some(value) = value else {
        return 0;
    };
    value
}
```

现场最容易漏掉的是“分号改变返回值”：`{ value }` 返回 `T`，`{ value; }` 返回 `()`。函数需要返回 `!` 时表示永不返回，CosmOS 的 `exit()`、panic handler 就属于这类边界。

```rust
// 适用环境：用户态 no_std；示例使用 CosmOS 用户库
#[macro_use]
extern crate user_lib;

use user_lib::exit;

fn die(reason: &str) -> ! {
    println!("fatal: {}", reason);
    exit(1)
}
```

### 2.2 所有权、移动、复制和借用

- `String`, `Vec<T>`, `Box<T>`, `Arc<T>` 等拥有堆资源的值通常是 move 类型；赋值或按值传参后，原绑定不能再使用。
- 整数、`bool`、字符、只包含 `Copy` 字段且显式 `#[derive(Copy, Clone)]` 的小结构通常可以复制。
- `&T` 是共享借用，`&mut T` 是独占可变借用；借用结束通常由最后一次使用决定，但复杂代码中可用花括号缩短生命周期。
- 函数参数写 `&str`/`&[u8]` 表示只读借用；写 `&mut [u8]` 表示函数可以填充调用者的 buffer。

> 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

fn append_mark(text: &mut String) {
    text.push('!');
}

fn byte_sum(bytes: &[u8]) -> u32 {
    bytes.iter().map(|&byte| byte as u32).sum()
}

let mut owned = String::from("ok");
append_mark(&mut owned);             // 可变借用结束后可再次使用 owned
let view: &str = owned.as_str();
let sum = byte_sum(view.as_bytes());

let mut values = Vec::new();
values.push(sum);
let moved = values;
// values.push(1);                   // 错误：values 已 move 给 moved
let copied = 7usize;
let other = copied;                  // usize 是 Copy，copied 仍可使用
```

如果循环中要同时修改同一数组的两个不相交区域，不能先持有两个整体 `&mut` 借用；使用 `split_at_mut` 让编译器看见不相交性：

> 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
fn zero_two_halves(buf: &mut [u8]) {
    let mid = buf.len() / 2;
    let (left, right) = buf.split_at_mut(mid);
    left.fill(0);
    right.fill(0);
}
```

借用 guard 也遵循同一规则。比如内核的 `SpinNoIrqLock::lock()`、`UPSafeCell::exclusive_access()` 返回 guard；guard 还在作用域内时锁仍被占用，必要时先 `drop(guard)` 再获取另一把锁或调用会再次加锁的函数。

### 2.3 `&str`、`String`、字节和 C 字符串

| 类型/表达式 | 含义 | 现场用法 |
| --- | --- | --- |
| `&str` | UTF-8 字符串切片，借用 | 用户 `main` 的 `argv`、`open(path: &str)` |
| `String` | 拥有、可增长的 UTF-8 字符串 | shell 拼接命令、`alloc::format!` |
| `&[u8]` | 任意字节只读视图 | `write(fd, buf)`、二进制协议 |
| `&mut [u8]` | 任意字节可写视图 | `read(fd, buf)`、syscall 输出 buffer |
| `b"abc"` | `&'static [u8; 3]` | 可自动借用为 `&[u8]`，不含 NUL |
| `"abc\0"` | 含 NUL 的 `&'static str` | 当前 `exec` 的显式 C 风格路径写法 |
| `str.as_bytes()` | `&[u8]` | 查找 NUL、传给写接口 |
| `core::str::from_utf8(bytes)` | `Result<&str, Utf8Error>` | 从目录项/系统调用 buffer 解码 |

`&str` 不等于 C 字符串。当前 `user_lib::open/link/...` 会在内部复制字符串并补 NUL；`exec` 的既有调用有时直接传 `"setupsh\0"`，有时用 `String` 自己构造参数数组。不能把一个普通 `&str` 的 `as_ptr()` 当成必然 NUL 结尾。

> 适用环境：用户态 no_std；RV64/LA64；使用仓库 `user_lib` API。

```rust
// 适用环境：用户态 no_std；RV64/LA64；user/src/bin/*.rs
extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{open, write, OpenFlags, STDOUT};

fn c_string(input: &str) -> String {
    let mut value = String::from(input);
    if !value.as_bytes().ends_with(b"\0") {
        value.push('\0');
    }
    value
}

fn show_name(name_bytes: &[u8]) {
    match core::str::from_utf8(name_bytes) {
        Ok(name) => println!("name={}", name),
        Err(_) => {
            let _ = write(STDOUT, b"name=<invalid utf8>\n");
        }
    }
}

fn open_path(path: &str) -> isize {
    // open() 当前会内部补 NUL；此处不要重复把临时 String 的裸指针传出。
    open(path, OpenFlags::RDONLY)
}
```

### 2.4 函数、方法、闭包和迭代器

函数签名先写参数和返回类型；方法的第一个参数通常是 `self`、`&self` 或 `&mut self`。`impl Type` 里的无 `self` 函数是关联函数，用 `Type::new()` 调用。

> 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
extern crate alloc;

use alloc::vec::Vec;

struct Counter {
    value: usize,
}

impl Counter {
    const fn new() -> Self {
        Self { value: 0 }
    }

    fn get(&self) -> usize {
        self.value
    }

    fn increment(&mut self) {
        self.value += 1;
    }
}

let mut counter = Counter::new();
counter.increment();
let current = counter.get();

let doubled: Vec<usize> = [1usize, 2, 3]
    .iter()
    .map(|value| value * 2)
    .collect();
```

闭包捕获方式由使用情况决定：只读通常借用，修改时可变借用，`move` 强制取得捕获值的所有权。内核/用户现场优先使用简单闭包和显式循环，遇到复杂的生命周期错误时先拆成命名函数。

## 3. 模式匹配、`Option`、`Result` 与错误处理

### 3.1 `match` 与常用模式

`match` 必须穷尽所有可能值；`_` 表示其余情况。可以解构 tuple、结构体和枚举，也可以加 `if` guard。

> 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
extern crate alloc;

use alloc::collections::VecDeque;

enum State {
    Ready { pid: usize },
    Sleeping(usize),
    Dead,
}

fn describe(state: &State) -> &'static str {
    match state {
        State::Ready { pid } if *pid == 0 => "idle",
        State::Ready { .. } => "ready",
        State::Sleeping(ms) if *ms == 0 => "wake-now",
        State::Sleeping(_) => "sleeping",
        State::Dead => "dead",
    }
}

fn drain(queue: &mut VecDeque<usize>) -> usize {
    let mut total = 0;
    while let Some(value) = queue.pop_front() {
        total += value;
    }
    total
}
```

常用模式速查：

| 写法 | 含义 |
| --- | --- |
| `Some(value)` | 解构 `Option` 并绑定内部值 |
| `Ok(value)` / `Err(error)` | 解构 `Result` |
| `Some(_)` | 只关心是否有值，不绑定 |
| `State::Ready { pid, .. }` | 只取结构体枚举的部分字段 |
| `(left, right)` | 解构 tuple |
| `0..n` / `0..=n` | 半开/闭区间模式或迭代范围 |
| `value @ 1..=4` | 匹配范围并保留整个值 |
| `x if condition` | guard，匹配后再检查条件 |
| `_` | 忽略剩余情况；不要用它掩盖尚未设计的错误 |
| `matches!(x, Some(_))` | 只需要布尔结果时 |

### 3.2 `Option`、`Result`、`?` 和 no_std 错误类型

`Option<T>` 表示有值/无值；`Result<T, E>` 表示成功/失败。`?` 会在 `None` 或 `Err` 时提前返回，因此外层返回类型必须兼容。

> 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
use core::convert::TryFrom;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    Empty,
    BadDigit,
    Overflow,
}

fn parse_byte(text: &str) -> Result<u8, ParseError> {
    if text.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut value = 0u16;
    for byte in text.as_bytes() {
        if !(b'0'..=b'9').contains(byte) {
            return Err(ParseError::BadDigit);
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((byte - b'0') as u16))
            .ok_or(ParseError::Overflow)?;
    }
    u8::try_from(value).map_err(|_| ParseError::Overflow)
}

fn parse_first(text: &str) -> Result<u8, ParseError> {
    let first = text.split(',').next().ok_or(ParseError::Empty)?;
    parse_byte(first)
}
```

用户态当前很多 syscall API 不返回 `Result`，而是 Linux 风格的负 `isize`。这时手动分支比 `?` 更直接：

> 适用环境：用户态 no_std；RV64/LA64；使用仓库 `user_lib`。

```rust
// 适用环境：用户态 no_std；RV64/LA64；user/src/bin/*.rs
use user_lib::{close, open, read, OpenFlags};

fn read_one(path: &str) -> isize {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return fd;
    }

    let mut buf = [0u8; 64];
    let result = read(fd as usize, &mut buf); // 只有确认 fd >= 0 后才转换
    let _ = close(fd as usize);
    result
}
```

仓库中 `user_lib::wait`/`waitpid`/`waittid` 还会把 `EAGAIN` 转换为 `yield_()` 循环；如果要观察一次原始返回值，调用更底层的包装或参考 [`user/src/lib.rs`](../../user/src/lib.rs) 的实现，不要误以为所有阻塞 API 都是一次 syscall 完成。

### 3.3 `let else`、`if let` 何时使用

- 只处理成功分支：`if let Some(x) = value { ... }`。
- 失败时立刻退出当前函数：`let Some(x) = value else { return ...; };`。
- 多种状态都需要处理：`match`。
- 只要布尔判断：`matches!`。

`let else` 是当前代码中已经使用的语法，但是否可用取决于现场编译器；如果报语法/稳定性错误，用等价的 `match` 替换，不要为了它改动 crate edition。

## 4. 结构体、枚举、trait 与 ABI

### 4.1 结构体、方法和 derive

```rust
// 适用环境：用户态/内核态/文件系统 no_std；RV64/LA64 均可
extern crate alloc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Range {
    start: usize,
    len: usize,
}

impl Range {
    const fn new(start: usize, len: usize) -> Self {
        Self { start, len }
    }

    fn end(&self) -> Option<usize> {
        self.start.checked_add(self.len)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

let range = Range::new(0x1000, 0x1000);
let range_end = range.end();
let _ = (range, range_end);
```

常见结构体形态：

```rust
// 适用环境：用户态/内核态/文件系统 no_std；RV64/LA64 均可
extern crate alloc;

struct Named {
    name: String,
}

struct Tuple(u32, u32);
struct Marker;

enum ResultKind {
    Ready,
    Failed { errno: isize },
}
```

`derive(Debug)` 解决 `{:?}`；`derive(Clone)` 是显式复制逻辑；`derive(Copy, Clone)` 只适合小的、按位复制语义正确的类型。含 `String`/`Vec`/锁 guard 的结构体不能随便 `Copy`。

### 4.2 `#[repr(C)]` 和 CosmOS 系统调用结构体

默认 Rust struct 布局不承诺 C ABI。用户态和内核共同解释的 `TimeVal`、`Timespec`、`Stat`、`SignalAction` 等当前源码使用 `#[repr(C)]`；新增 syscall 参数结构体也应沿用。

> 适用环境：用户态与内核态共享的 no_std ABI；RV64/LA64；字段布局必须两端一致。

```rust
// 适用环境：用户态与内核态共享的 no_std ABI；RV64/LA64
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Request {
    pub op: u32,
    pub flags: u32,
    pub address: usize,
    pub length: usize,
}

// 指针/长度来自用户空间时，内核仍必须做地址和权限检查；repr(C) 不等于安全。
```

注意：`#[repr(C)]` 解决的是布局/对齐/字段顺序，不解决字节序、指针有效性、生命周期、整数溢出或用户地址是否可访问。

### 4.3 枚举、判别值和 bitflags

枚举表达“互斥的状态”，bitflags 表达“可组合的位集合”。用户库的 `OpenFlags`、`MMapFlags`、`MMapProt` 用 `bitflags` 1.x；使用前看当前 crate 的 API 和锁文件，不要凭记忆把 `.bits` 与 `.bits()` 跨 major version 混用。

> 适用环境：用户态 no_std；`user/Cargo.toml` 当前依赖 `bitflags = "1.2.1"`，`user/Cargo.lock` 锁定到 1.3.2；RV64/LA64 均可。

```rust
// 适用环境：用户态 no_std；bitflags 1.x（以 user/Cargo.lock 为准）
use user_lib::OpenFlags;

let flags = OpenFlags::CREATE | OpenFlags::WRONLY;
if flags.contains(OpenFlags::CREATE) {
    println!("create requested");
}
let raw: u32 = flags.bits(); // 若本地宏展开/API报错，先检查锁定的 bitflags 版本
```

系统调用编号在 [`user/src/syscall.rs`](../../user/src/syscall.rs) 中用 `usize` 常量维护；不要用 Rust enum 的默认判别值去猜 ABI 编号。

## 5. 数组、切片、迭代与边界检查

### 5.1 `[T; N]`、`&[T]`、`Vec<T>` 的区别

| 类型 | 长度 | 所有权/分配 | 典型场景 |
| --- | --- | --- | --- |
| `[T; N]` | 编译期固定 | 值本身拥有元素；可在栈或 static | syscall 参数 `[usize; 3]`、`[u8; 512]` |
| `&[T]` | 运行时 `len()` | 借用，只读 | `write(fd, buf)`、函数输入 |
| `&mut [T]` | 运行时 `len()` | 借用，可写 | `read(fd, buf)`、输出 buffer |
| `Vec<T>` | 运行时可增长 | 堆拥有 | 参数数组、目录名集合、缓存 |

数组常量 `[value; N]` 通常要求 `value: Copy` 或是可求值的 inline const。当前用户 CPU 探针使用 `[const { AtomicUsize::new(0) }; MAX_WORKERS]`，这是为了构造每个独立 atomic，而不是复制同一个 atomic。

> 适用环境：用户态 no_std；RV64/LA64；使用固定 buffer 和用户 syscall。

```rust
// 适用环境：用户态 no_std；RV64/LA64；user/src/bin/*.rs
use user_lib::{read, write, STDOUT};

let mut buf = [0u8; 128];
let n = read(0, &mut buf);
if n >= 0 && (n as usize) <= buf.len() {
    let used = n as usize;
    let payload: &[u8] = &buf[..used]; // used 必须 <= buf.len()
    let _ = write(STDOUT, payload);
}
```

### 5.2 迭代器和切片常用操作

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |sum, &byte| sum + byte as u32)
}

fn copy_prefix(src: &[u8], dst: &mut [u8]) -> usize {
    let count = core::cmp::min(src.len(), dst.len());
    dst[..count].copy_from_slice(&src[..count]);
    count
}

fn visit_prefix(src: &[u8]) {
    for (index, byte) in src.iter().copied().enumerate() {
        if index == 16 {
            break;
        }
        let _ = (index, byte);
    }
}
```

常用方法：

- `len`, `is_empty`, `first`, `last`, `get(index)`：`get` 返回 `Option`，避免越界 panic。
- `iter`, `iter_mut`, `enumerate`, `zip`, `position`, `any`, `all`, `map`, `filter`, `find`。
- `&buf[..n]`、`&buf[n..]`、`split_at`、`split_at_mut`。
- `fill(0)`、`copy_from_slice`、`copy_within`；长度不匹配时 `copy_from_slice` 会 panic。
- `chunks`, `chunks_exact` 处理定长协议/页；`windows` 处理滑动窗口。

不要用 `unwrap()` 代替边界设计：现场 panic 只会走用户/内核 panic handler，可能退出程序或关闭内核。只有已经由协议/前置条件证明不会失败时才使用，并在旁边写出证明。

### 5.3 用户目录项等二进制数据的安全切片模式

仓库 `user/src/bin/ls.rs` 对 `getdents64` 返回的记录先检查 `pos + 19 <= nread`，读取 `reclen`，再检查 `pos + reclen <= nread`，最后才切片。这个顺序很重要：

> 适用环境：用户态 no_std；RV64/LA64；解析 syscall 返回的字节 buffer。

```rust
// 适用环境：用户态 no_std；RV64/LA64；解析不可信的字节序列
fn first_record_name(buf: &[u8], nread: usize) -> Option<&str> {
    if nread > buf.len() || nread < 19 {
        return None;
    }
    let reclen = u16::from_le_bytes([buf[16], buf[17]]) as usize;
    if reclen < 19 || reclen > nread {
        return None;
    }
    let field = &buf[19..reclen];
    let name_len = field.iter().position(|&byte| byte == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..name_len]).ok()
}
```

## 6. 类型转换、整数安全和指针

### 6.1 `as`、`From/Into`、`TryFrom`

| 写法 | 适合场景 | 风险 |
| --- | --- | --- |
| `value as usize` | 明确知道范围，或 syscall ABI 需要按位传参 | 可能截断、符号转换或改变语义 |
| `usize::from(value)` | 无损的标准转换 | 需要目标类型实现 `From` |
| `value.into()` | 目标类型已由上下文确定 | 目标不明确时推导失败 |
| `usize::try_from(value)` | 外部输入、负数/大数可能出现 | 返回 `Result`，必须处理失败 |
| `checked_add/mul/sub` | 地址、长度、偏移计算 | 溢出返回 `None` |
| `saturating_add/sub` | 允许钳制到边界的计数/时间 | 不能把钳制误当作精确结果 |
| `wrapping_add` | 环形计数/协议明确需要模运算 | 不适合内存长度和地址安全检查 |

> 适用环境：用户态或内核态 no_std；RV64/LA64；解析外部长度/偏移时优先 checked/try。

```rust
// 适用环境：任意 CosmOS Rust crate（no_std；RV64/LA64 均可）
use core::convert::TryFrom;

fn to_index(value: isize) -> Option<usize> {
    usize::try_from(value).ok()
}

fn end(offset: usize, length: usize, limit: usize) -> Option<usize> {
    let end = offset.checked_add(length)?;
    (end <= limit).then_some(end)
}

let a = 255u8 as usize;              // 明确无损
let b = u64::try_from(usize::MAX);   // 目标/源宽度不确定时保留 Result
```

`isize` 系统调用结果的正确顺序：

```text
ret < 0  -> 按错误码处理
ret >= 0 -> ret as usize / ret as u64
```

不要把 `fd = -2` 转为 `usize` 再索引文件描述符表；不要用 `len as isize` 代替检查，超大长度在窄类型上可能变成负数。

### 6.2 字节序和整数布局

协议/磁盘数据必须显式指定字节序：`u16::from_le_bytes`、`to_le_bytes`、`from_be_bytes`。不要把 `*const u8` 直接转成未对齐的 `*const u32` 后解引用；使用字节数组转换，或在确有布局证明时使用 `read_unaligned`。

> 适用环境：用户态/内核态/文件系统 no_std；解析磁盘、网络或 syscall buffer。

```rust
// 适用环境：任意 CosmOS no_std crate（RV64/LA64）
use core::convert::TryInto;

fn read_le_u32(bytes: &[u8]) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}
```

如果 edition/工具链未把 `TryInto` trait 放入 prelude，就像上例一样显式导入。

### 6.3 指针与 `unsafe` 的最小规则

指针转换本身不验证地址。`from_raw_parts`、`read_volatile`、`write_volatile`、裸指针 `add` 都要求调用者证明：地址有效、长度合法、对齐满足要求（或使用 unaligned）、生命周期和别名规则不被破坏。

当前用户入口 [`user/src/lib.rs`](../../user/src/lib.rs) 用 `read_volatile` 读取初始 `argc/argv`，用 `core::slice::from_raw_parts` 构造字符串；这是 ABI 入口的必要 unsafe，不是可随意复制的普通切片写法。

> 适用环境：用户态/内核态 no_std；只有在已有 ABI/硬件证明时使用；RV64/LA64 的汇编约束不同。

```rust
// 适用环境：no_std FFI/硬件/系统调用边界；不能直接套用到不可信地址
unsafe fn bytes_from_raw<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    // 调用者必须保证 ptr..ptr+len 是可读、已初始化、在同一有效生命周期内的区域。
    core::slice::from_raw_parts(ptr, len)
}
```

## 7. `const`、`static`、原子变量和延迟初始化

### 7.1 `const` 与 `static`

| 项目 | `const` | `static` |
| --- | --- | --- |
| 存储语义 | 可在使用处内联，每次使用可视为一个值 | 程序中有固定存储位置 |
| 可变性 | 不能声明为可变 | `static mut` 可变但访问需要 unsafe，尽量避免 |
| 地址身份 | 不保证同一地址 | 有稳定地址 |
| 线程共享 | 值语义，不要求 `Sync` | 共享 static 必须满足相应安全约束 |
| 适合 | 常量、掩码、数组大小、纯计算 | 全局状态、原子、allocator、只读表 |

`const fn` 允许在编译期构造值；但并非所有函数都能在 static 初始化时调用。不能 const 初始化时，可使用仓库已经依赖的 `lazy_static`（用户和内核都启用 `spin_no_std`）。

> 适用环境：用户态/内核态 no_std；全局共享状态请优先原子或仓库锁类型；RV64/LA64 均可。

```rust
// 适用环境：no_std；RV64/LA64
use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE_SIZE: usize = 4096;
static BOOT_COUNT: AtomicUsize = AtomicUsize::new(0);

fn record_boot() -> usize {
    BOOT_COUNT.fetch_add(1, Ordering::Relaxed)
}

let _ = record_boot();
let current = BOOT_COUNT.load(Ordering::Acquire);
```

`Ordering` 不是装饰品：计数器只需 `Relaxed` 的场景不要过度使用强序；发布数据后供另一个 hart 读取时必须和对应的 `Release`/`Acquire` 设计配套。现场若不确定，先沿用相邻 CosmOS 代码的 ordering 和锁，而不是凭感觉改成 `SeqCst` 或 `Relaxed`。

### 7.2 `static mut`、`.bss` 和启动顺序

用户库当前有：

```rust
// 适用环境：仅说明 user/src/lib.rs 的现有启动实现；不要在普通业务代码复制 static mut
static mut HEAP_SPACE: [u8; USER_HEAP_SIZE] = [0; USER_HEAP_SIZE];
```

它作为用户堆，在 `__user_start` 中通过 unsafe 初始化 `buddy_system_allocator::LockedHeap`。用户库还会先 `clear_bss()`，再初始化堆，随后才创建 `Vec` 解析 `argc/argv`。若增加全局可变对象，应考虑 `.bss` 清零、SMP、重入和 allocator 初始化顺序；不要在启动前调用会分配的 lazy static。

内核 [`os/src/main.rs`](../../os/src/main.rs) 也有 `.bss` 清零和多 hart 栅栏：secondary hart 先等待 bootstrap hart 完成全局初始化。修改启动全局状态时，必须看 `BOOT_BSS_READY`、`BOOT_DONE` 的 Acquire/Release 关系。

## 8. `core`、`alloc` 和 `no_std` 的实际边界

### 8.1 `core` 能做什么

`core` 不依赖操作系统，通常始终可用：

```text
core::cmp          min/max
core::convert      From/TryFrom/TryInto
core::fmt          Arguments、Display、Debug、Write
core::hint         spin_loop、black_box（按工具链）
core::mem          size_of、align_of、forget、MaybeUninit
core::ptr          null、read_volatile、write_volatile
core::slice        from_raw_parts、from_raw_parts_mut
core::str          from_utf8
core::sync::atomic  Atomic*、Ordering
core::cell         UnsafeCell
core::ops          Deref、Drop、Index 等
core::arch         asm、global_asm
```

`core` 不提供文件、socket、线程、操作系统时间、环境变量或普通进程退出；CosmOS 通过用户 syscall wrapper 和内核自身驱动/调度实现这些能力。

### 8.2 `alloc` 能做什么

`alloc` 提供需要堆的纯 Rust 数据结构，但它不负责“向操作系统申请一页内存”；它调用当前 crate 的 `#[global_allocator]`。

| `alloc` 路径 | 当前仓库用法 |
| --- | --- |
| `alloc::string::String` | 用户 shell、路径构造、内核文件系统 |
| `alloc::format!` | 用户 `disk_perf`、内核启动文案/日志辅助 |
| `alloc::vec::Vec` / `alloc::vec!` | 用户参数/目录数据、内核缓冲/集合 |
| `alloc::collections::{BTreeMap, BTreeSet, VecDeque}` | 页缓存、日志、文件系统、网络 |
| `alloc::sync::{Arc, Weak}` | 用户 console、内核任务/文件系统对象 |
| `alloc::boxed::Box` | 需要拥有堆对象时 |

crate 根通常要有：

> 适用环境：no_std crate 根；用户库、内核、`fs` 都采用这一模式；单个 bin 是否还要写取决于它是否直接引用 `alloc` 路径。

```rust
// 适用环境：no_std crate 根；RV64/LA64
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

fn make_message() -> String {
    let mut message = String::new();
    let _ = write!(&mut message, "count={}", Vec::<u8>::new().len());
    message
}
```

### 8.3 三个 crate 的 allocator 关系

#### 用户态

[`user/src/lib.rs`](../../user/src/lib.rs) 中：

- `static HEAP: LockedHeap = LockedHeap::empty();`
- `#[global_allocator]` 把它设为全局 allocator。
- `HEAP_SPACE` 是 `128 * 1024` 字节静态区域。
- `__user_start` 清 BSS 后调用 `HEAP.lock().init(...)`，然后才构造 `Vec`。
- `#[alloc_error_handler]` 会 panic；用户 panic handler 打印后 `exit(-1)`。

用户 bin 通常通过 `user_lib` 间接获得 allocator；不要在每个 `user/src/bin/*.rs` 再定义一个 global allocator、panic handler 或 alloc error handler，否则会产生重复 lang item/allocator。

#### 内核态

[`os/src/mm/heap_allocator.rs`](../../os/src/mm/heap_allocator.rs) 中的 `KernelHeapAllocator` 是全局 allocator。[`os/src/mm/mod.rs`](../../os/src/mm/mod.rs) 的 `init()` 顺序包含 frame allocator、`heap_allocator::init_heap()`、激活 kernel space 和 heap mapping。内核堆实现了 slab/buddy/按需增长等当前仓库逻辑；现场调试分配失败时先看 allocator 初始化和页表，而不是只怀疑 `Vec`。

#### 文件系统库

[`fs/src/lib.rs`](../../fs/src/lib.rs) 只声明 `no_std` 和 `extern crate alloc`，没有独立 allocator。它作为依赖被 `os` 链接，使用调用最终二进制提供的全局 allocator。若把 `fs` 单独做成一个裸机最终程序，必须另行提供 allocator、panic 和入口。

### 8.4 no_std 替换表

| 不要直接写 | CosmOS 现场替代 |
| --- | --- |
| `std::vec::Vec` | `alloc::vec::Vec` |
| `std::string::String` | `alloc::string::String` |
| `std::collections::VecDeque` | `alloc::collections::VecDeque` |
| `std::sync::Arc` | `alloc::sync::Arc`；用户/内核锁按仓库依赖选择 |
| `std::fmt::Write` | `core::fmt::Write` |
| `std::io::Read/Write` | 用户 `read/write` syscall 或自定义 `core::fmt::Write` |
| `std::println!` | 用户/内核仓库自定义 `println!`，或直接 `write` |
| `std::thread::yield_now` | 用户 `yield_()`；内核调度 API |
| `std::time` | 用户 `get_time`/`clock_gettime` 等 wrapper，内核 timer API |
| `std::fs` | 用户 syscall；内核 `crate::fs` |
| `std::error::Error` | 自定义 `enum`/`Result`；必要时使用 `core` 中可用的基础 trait |

第三方 crate 也必须 no_std 兼容。当前配置中的典型开关是：

- `lazy_static = { version = "1.4.0", features = ["spin_no_std"] }`；锁文件解析到 1.5.0。
- `smoltcp` 使用 `default-features = false` 加 `alloc`、网络协议 feature。
- `fs` 以 `default-features = false` 被内核引入，再按 `io_perf_counters`、`kernel_sleep_mutex` 等 feature 选择。
- `os` 的 `log` 启用 `release_max_level_warn`；release 下低级别日志可能在编译期被裁掉。

## 9. CosmOS 中的格式化、输出和日志

### 9.1 `core::fmt` 的两条路径

`format_args!` 生成借用的 `fmt::Arguments`，本身不等于分配一个 `String`。最终是否分配由目标决定：

```text
format_args!(...) -> custom Write::write_fmt -> 可能直接 UART/syscall，不一定分配
alloc::format!(...) -> String -> 必须经过 global allocator
write!(&mut String, ...) -> String 扩容时分配
write!(&mut [fixed buffer writer], ...) -> 可做到无堆格式化
```

`write!` 要求目标实现 `core::fmt::Write`；不要从有 std 的示例复制 `std::io::Write`。

### 9.2 用户态输出的实际行为

[`user/src/console.rs`](../../user/src/console.rs) 的 `ConsoleBuffer`：

- 内部是 `VecDeque<u8>`，容量 `256 * 10 = 2560` 字节。
- `write_str` 按 UTF-8 字节推入 buffer；遇到换行或满容量时 `flush()`。
- `flush()` 调用 `user_lib::write(STDOUT, &[u8])`，不是宿主机 stdout。
- `close(STDOUT)` 也会先 flush。
- 仓库故意忽略 `write_fmt` 的错误，避免 stdout 已关闭时在持有 console mutex 时再次 panic 造成死锁。
- 宏模式要求 `$fmt: literal`，所以动态内容写成 `print!("{}", text)`。

> 适用环境：用户态 no_std；RV64/LA64；`user/src/bin/*.rs`。

```rust
// 适用环境：用户态 no_std；RV64/LA64；使用 user_lib 自定义宏
#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{flush, write, STDOUT};

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let text = "hello";
    println!("text={}", text);       // 正确：格式串是字面量
    let _ = write(STDOUT, b"raw bytes\n");
    flush();
    0
}
```

用户库已有宏的导入方式在仓库中有两类：老代码常用 `#[macro_use] extern crate user_lib;`，部分代码用 `use user_lib::{println, write, ...};`。新文件优先沿用同目录已有写法，遇到宏找不到先检查导入和宏的 crate 归属。

### 9.3 内核输出和 `log`

[`os/src/console.rs`](../../os/src/console.rs) 的 `print!`/`println!` 通过 `core::fmt::Write` 写 UART 或 early console，并用 `CONSOLE_LOCK` 串行化多 hart 输出，同时暂时关闭本地 supervisor interrupt。

内核日志走 [`os/src/klog.rs`](../../os/src/klog.rs)：

```text
trace!/debug!/info!/warn!/error! -> log crate -> SimpleLoggerPrinter
                                     ├─ 追加 16 KiB 内存环形日志
                                     └─ UART 已 ready 时输出带颜色的行
```

`LOG` 是编译内核时通过 `option_env!("LOG")` 读取的环境变量，支持 `ERROR/WARN/INFO/DEBUG/TRACE`；未设置时当前实现的最大级别是 `Off`。它不是启动后在 guest shell 中设置的变量。

> 适用环境：内核态 no_std；RV64/LA64；在 `os` crate 中使用 `log` 宏。

```rust
// 适用环境：内核态 no_std；RV64/LA64；os/src/**/*.rs
debug!("page fault va={:#x}", va);
warn!("short read: expected={}, got={}", expected, got);
error!("filesystem error: {:?}", err);
println!("unconditional boot text");
```

要注意：release `log` feature 可能使 `debug!`/`info!`/`trace!` 的格式化代码在编译期不保留；如果需要现场观察低级别日志，先确认 `os/Cargo.toml` 的 feature、`LOG=DEBUG` 是否在重编译时传入，以及构建是否因为 stamp 命中而跳过了内核。

### 9.4 不依赖堆的固定缓冲格式化模板

在中断、锁内、分配失败路径或不希望触发 allocator 的诊断代码中，可以用固定数组实现 `core::fmt::Write`。溢出时返回 `fmt::Error`，调用者决定截断/放弃，不能静默越界。

> 适用环境：用户态/内核态 no_std；RV64/LA64；代码本身不调用 OS 输出 API。

```rust
// 适用环境：任意 CosmOS no_std crate（RV64/LA64）
use core::fmt::{self, Write as _};

struct FixedBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedBuf<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl<const N: usize> fmt::Write for FixedBuf<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let src = text.as_bytes();
        let end = self.len.checked_add(src.len()).ok_or(fmt::Error)?;
        if end > self.bytes.len() {
            return Err(fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(src);
        self.len = end;
        Ok(())
    }
}

let mut line = FixedBuf::<96>::new();
if write!(&mut line, "pid={} va={:#x}", 7usize, 0x1000usize).is_ok() {
    let bytes = line.as_bytes();
    // 交给当前环境的 write/UART 函数；这里只展示格式化阶段。
    let _ = bytes;
}
```

## 10. 模块、可见性和条件编译

### 10.1 文件模块和路径

```text
crate root: src/lib.rs 或 src/main.rs
mod console;             -> src/console.rs 或 src/console/mod.rs
mod arch;                -> src/arch/mod.rs，再由它声明 riscv/loongarch64
pub mod syscall;         -> 对 crate 外公开模块
pub(crate) mod perf_probe; -> 仅当前 crate 可见
```

当前 user crate 根中：

```rust
// 适用环境：用户库 user/src/lib.rs；Rust 2018；no_std
#[macro_use]
pub mod console;
pub mod net;
mod lang_items;
mod syscall;

pub use console::{flush, STDIN, STDOUT};
pub use syscall::*;
```

因此 bin 可以直接 `use user_lib::{read, write, OpenFlags};`；`lang_items`/`syscall` 的模块路径本身不公开，但 syscall wrapper 通过 `pub use` 暴露。

内核 [`os/src/main.rs`](../../os/src/main.rs) 以 `pub mod`、`mod`、`pub(crate) mod` 组合出 crate 图；内部模块引用常见写法：

```rust
// 适用环境：内核态 no_std；RV64/LA64
use crate::mm::{frame_alloc, PhysPageNum};
use super::SpinNoIrqLock;
use self::inner::State;
```

### 10.2 `use`、重导出和 trait 导入

```rust
// 适用环境：任意 CosmOS no_std crate；RV64/LA64
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use core::fmt::Write as _;       // 只把 trait 放入方法/宏解析范围
use crate::sync::SpinNoIrqLock;

type ByteMap = BTreeMap<usize, VecDeque<u8>>;
```

“方法不存在”常常不是类型真的没有方法，而是 trait 没有 `use` 进作用域。例如 `write!(&mut String, ...)` 需要 `core::fmt::Write`；`deref`/迭代器扩展方法也可能需要对应 trait。优先导入 trait，不要为了绕过错误使用 unsafe。

### 10.3 `cfg` 与双架构代码

目标条件必须与 Rust 的 `target_arch` 名字完全匹配。当前仓库的用户入口、syscall 汇编、内核平台模块都按下面方式分开：

> 适用环境：用户态或内核态 no_std；分别编译 RV64/LA64。

```rust
// 适用环境：CosmOS 用户/内核代码；RV64 和 LA64 分支必须都能解析
#[cfg(target_arch = "riscv64")]
fn arch_name() -> &'static str {
    "riscv64"
}

#[cfg(target_arch = "loongarch64")]
fn arch_name() -> &'static str {
    "loongarch64"
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
fn supported() -> bool {
    true
}
```

Rust 会先对当前目标做 cfg 消除，因此另一架构分支中使用的寄存器名可以不同；但公共类型、函数名和调用方仍必须保持一致。新增分支后至少分别 `cargo build --target ...`，不要只在宿主机 rust-analyzer 中看一边。

## 11. CosmOS 用户态模板与命令

### 11.1 新增用户应用的最小模板

仓库中 `user/src/bin/ls.rs`、`mkdir.rs` 等使用 `argc/argv` 版本；`initproc.rs`、`sh.rs` 等旧程序还存在无参数 `main` 版本。新增程序建议使用带参数的版本，和 [`user/src/lib.rs`](../../user/src/lib.rs) 的 weak main 语义一致。

> 适用环境：用户态 `user/src/bin/example.rs`；`no_std`/`no_main`；RV64 或 LA64；Rust 2018。

```rust
// 适用环境：用户态 user/src/bin/example.rs；no_std/no_main；RV64/LA64
#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exit, write, STDOUT};

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc > 1 {
        println!("arg1={}", argv[1]);
    } else {
        let _ = write(STDOUT, b"no argument\n");
    }

    // 只有确定要终止整个进程且不需要返回值时才调用 exit。
    if argc == usize::MAX {
        exit(2);
    }
    0
}
```

`exit()` 的返回类型是 `!`，调用后不需要再写 `return`；普通 main 返回 `i32`，由用户库入口调用 `exit(main(...))`。不要在 bin 中额外写 `fn main()` 作为 Rust 语言入口；这里是 `#![no_main]` 下导出的符号 `main`。

### 11.2 用户态读写循环模板

> 适用环境：用户态 no_std；RV64/LA64；使用 `user_lib::read/write`，适合现场小工具。

```rust
// 适用环境：用户态 no_std；RV64/LA64；user/src/bin/*.rs
use user_lib::{read, write, STDIN, STDOUT};

fn copy_once() -> isize {
    let mut buf = [0u8; 256];
    let n = read(STDIN, &mut buf);
    if n < 0 {
        return n;
    }
    if n == 0 {
        return 0; // EOF
    }
    let used = n as usize;
    let written = write(STDOUT, &buf[..used]);
    if written < 0 {
        return written;
    }
    written
}
```

注意当前 syscall wrapper 是否保证“一次完成全部请求”要看具体 syscall；`read`/`write` 可能短读/短写，文件或网络基准应像 [`user/src/bin/disk_perf.rs`](../../user/src/bin/disk_perf.rs) 那样循环推进 `done`。

### 11.3 用户态参数转 C 数组模板

`exec`/`execve` 接受 `&[*const u8]`，数组中的每个指针必须指向仍然存活、NUL 结尾的字符串，最后还要放空指针。先构造拥有的 `Vec<String>`，再取指针；不要在构造完指针后继续让 `Vec<String>` 重新分配。

> 适用环境：用户态 no_std；RV64/LA64；`alloc` 已由 user_lib 提供并初始化。

```rust
// 适用环境：用户态 no_std；RV64/LA64；构造 exec 参数
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::exec;

fn run(path: &str, args: &[&str]) -> isize {
    let mut owned: Vec<String> = Vec::new();
    let mut c_path = String::from(path);
    if !c_path.as_bytes().ends_with(b"\0") {
        c_path.push('\0');
    }
    owned.push(c_path);
    for &arg in args {
        owned.push(String::from(arg));
    }
    for arg in owned.iter_mut() {
        if !arg.as_bytes().ends_with(b"\0") {
            arg.push('\0');
        }
    }

    let mut ptrs: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr()).collect();
    ptrs.push(core::ptr::null());
    exec(owned[0].as_str(), ptrs.as_slice())
}
```

这里 `path` 的高层 `exec` wrapper 会按当前实现传给 syscall；如果直接使用 `exec_ptr`/raw syscall，则路径自身也必须符合对应 C 字符串约定。`ptrs` 和 `owned` 必须活到 syscall 返回；真正成功的 `exec` 通常不会返回，失败才返回负 errno。

### 11.4 用户态双架构 raw syscall 只改汇编层

平时优先调用 [`user/src/syscall.rs`](../../user/src/syscall.rs) 和 [`user/src/lib.rs`](../../user/src/lib.rs) 已有 wrapper。必须新增 raw syscall 时，复制相邻函数的寄存器约束，不要凭 Linux x86-64 习惯填写寄存器：

| 架构 | trap 指令 | 返回/前三个参数 | syscall 编号 |
| --- | --- | --- | --- |
| RISC-V | `ecall` | `x10/a0`, `x11/a1`, `x12/a2` | `x17/a7` |
| LoongArch64 | `syscall 0` | `$a0`, `$a1`, `$a2` | `$a7` |

LoongArch 版本还显式标记若干 caller-saved `$t0..$t8` 可能被内核 syscall clobber；继续沿用该文件的写法。用户程序如果在 raw asm 外保存了指针/局部值，必须正确声明 clobber。

## 12. 构建、检查与离线命令模板

所有命令均在仓库根 `/home/kyle/OS/xxOS` 执行，或先 `cd` 到相应目录。根 `Makefile` 的 `cargo-config` 会把版本库中的配置复制到 `user/.cargo/config.toml` 和 `os/.cargo/config.toml`；评测/离线环境如果这两个目录尚不存在，先执行它。

### 12.1 环境和目标检查

> 适用环境：宿主机 shell；不修改源码；需要已有 rustup/cargo。

```bash
# 适用环境：宿主机 shell；离线前检查工具链和目标
rustc --version
cargo --version
rustup target list --installed
rustc --print cfg --target riscv64gc-unknown-none-elf | rg 'target_arch|target_os|target_env|target_pointer_width'
rustc --print cfg --target loongarch64-unknown-none | rg 'target_arch|target_os|target_env|target_pointer_width'
```

预期至少有：`riscv64gc-unknown-none-elf` 和 `loongarch64-unknown-none`。离线时不能临时 `rustup target add`；目标缺失要提前解决。

### 12.2 恢复仓库 Cargo 配置

> 适用环境：宿主机 shell；仓库根；只生成/覆盖 `user/.cargo/config.toml` 和 `os/.cargo/config.toml`，这是构建要求的一部分。

```bash
# 适用环境：仓库根；离线构建前执行一次
make cargo-config
sed -n '1,120p' user/.cargo/config.toml
sed -n '1,120p' os/.cargo/config.toml
```

配置中的关键差异：

- `user` RV64 使用 `-Clink-args=-Tsrc/linker.ld` 和 `-Ctarget-feature=+f,+d`；LA64 使用 `src/linker-loongarch64.ld`。
- `os` RV64/LA64 使用各自 linker script，并设置 `-Cforce-frame-pointers=yes`。
- 不要把 `user` 的链接脚本复制给 `os`，也不要在错误的工作目录下直接运行带相对链接脚本路径的 cargo 命令。

### 12.3 用户程序构建

> 适用环境：宿主机 shell；依赖已经在本地 Cargo cache；RISC-V 用户态 release。

```bash
# 适用环境：宿主机 shell；从 user/Makefile 构建 RV64 用户应用
make -C user build ARCH=riscv64
```

> 适用环境：宿主机 shell；依赖已经在本地 Cargo cache；LoongArch64 用户态 release。

```bash
# 适用环境：宿主机 shell；从 user/Makefile 构建 LA64 用户应用
make -C user build ARCH=loongarch64
```

Makefile 当前默认 `MODE=release`，并将产物放到：

```text
user/target/riscv64gc-unknown-none-elf/release/<app>
user/target/loongarch64-unknown-none/release/<app>
user/build/bin/<app>.bin
user/build/elf/<app>.elf
```

若明确需要 Cargo 离线模式，可从 `user/` 目录执行；这不是 Makefile 默认注入的参数，前提是 registry/git 依赖缓存完整：

> 适用环境：宿主机 shell；依赖和 target 已缓存；用户态 no_std。

```bash
# 适用环境：宿主机 shell；用户 Cargo.lock 与本地缓存必须齐全
cd user
cargo build --release --locked --offline --target riscv64gc-unknown-none-elf
cargo build --release --locked --offline --target loongarch64-unknown-none
cd ..
```

如果当前目录的 `.cargo/config.toml` 未生成，命令行 target 仍可指定 target，但相对 linker script/rustflags 可能缺失；优先回到仓库根执行 `make cargo-config`，再重试。

### 12.4 内核构建和日志

`os/Makefile` 的 `kernel` 目标按架构选择 target，并执行 `--no-default-features --features $(MAIN_FS)`；当前 `MAIN_FS := ext4`。因此不要把 `os/Cargo.toml` 的 `default = [...]` 直接等同于 Makefile 实际启用的 feature 集合。

> 适用环境：宿主机 shell；内核依赖、target、配置和 bootloader 资源已在本地；RV64。

```bash
# 适用环境：宿主机 shell；构建 CosmOS RV64 内核
make -C os kernel ARCH=riscv64

# 适用环境：宿主机 shell；在编译时设置 klog 的最大级别输入
LOG=DEBUG make -C os kernel ARCH=riscv64
```

> 适用环境：宿主机 shell；LoongArch64 内核；依赖和 bootloader 已准备。

```bash
# 适用环境：宿主机 shell；构建 CosmOS LA64 内核
make -C os kernel ARCH=loongarch64
```

内核产物路径：

```text
os/target/riscv64gc-unknown-none-elf/release/os
os/target/loongarch64-unknown-none/release/os
```

离线时建议先直接构建 `kernel`，不要调用会自动 `rustup target add`、`cargo install` 或重打文件系统镜像的 `env`/完整 `build` 路径；`os/Makefile` 的 `env` 在 `OFFLINE` 为空时会尝试联网安装工具。

### 12.5 静态检查和精确找源码

> 适用环境：宿主机 shell；只读检查；目标 crate 的依赖缓存已准备。

```bash
# 适用环境：仓库根；检查文档引用的实现位置
rg -n '#!\[no_std\]|global_allocator|alloc_error_handler|panic_handler|target_arch|core::fmt::Write' user/src os/src fs/src

# 适用环境：宿主机 shell；离线读取 manifest 图，不编译
cargo metadata --manifest-path user/Cargo.toml --no-deps --offline --format-version 1
cargo metadata --manifest-path os/Cargo.toml --no-deps --offline --format-version 1

# 适用环境：相应 crate 目录；只检查 rustfmt，不改文件
cargo fmt --manifest-path user/Cargo.toml --all -- --check
cargo fmt --manifest-path os/Cargo.toml --all -- --check
```

`cargo fmt --check` 只检查格式；`cargo build -vv` 可以显示实际 target、rustflags、feature 和 linker 参数。遇到“代码看起来正确但链接失败”，先用它核对编译命令。

## 13. 常见编译错误：现象 → 原因 → 检查 → 修复方向

| 现象 | 常见原因 | 现场检查 | 修复方向 |
| --- | --- | --- | --- |
| `can't find crate for 'std'`、`use of unresolved module std` | 裸机 crate 是 `no_std` | 看 crate 根和报错依赖；`rg -n 'std::' user/src os/src fs/src` | 改用 `core`/`alloc` 或 CosmOS syscall/内核 API；确认第三方 crate 的 no_std feature |
| `cannot find macro println` | 没有导入 user 自定义宏，或在普通 no_std crate 里误用了 std 宏 | 查 `user/src/console.rs` 和文件顶部导入 | 用户 bin 加 `#[macro_use] extern crate user_lib;` 或按已有写法导入；内核用 crate 内 console 宏；低层代码用 `write` |
| `format argument must be a string literal` | CosmOS `print!`/`println!` 宏限制第一个参数为 literal | 看调用是否为 `println!(msg)` | 改为 `println!("{}", msg)`；不要把动态字符串当 format string |
| `cannot write into ...` / `write_fmt` 方法不存在 | `core::fmt::Write` trait 没有导入，或误导入 `std::io::Write` | 查 `use core::fmt::Write` | `use core::fmt::Write as _;`；确认目标实现的是 fmt Write |
| `no global memory allocator found` | 使用 `Vec/String/Arc` 的最终 crate 没有 global allocator，或 allocator 模块未被链接 | 查 `#[global_allocator]`；用户看 `user/src/lib.rs`，内核看 `os/src/mm/heap_allocator.rs` | 让最终二进制提供一个 allocator；用户 bin 不要重复定义，确保先经过 `__user_start`/内核 `mm::init` |
| `alloc_error_handler`/`panic_impl`/allocator 重复定义 | 在 user bin 或依赖中又定义了 lang item；或错误地链接了 std | 搜索 `panic_handler`、`alloc_error_handler`、`global_allocator` 的全部结果 | 每个最终裸机二进制只保留一套；用户由 `user_lib` 提供，内核由 `os` 提供；不要混入 std |
| `use of unstable feature ...`、`feature may not be used on stable` | 当前源码需要 nightly 特性，现场用了 stable | `rustc --version`; 搜 `#![feature(...)]` | 使用赛事提供的兼容 nightly；不要随意删 feature，先确认该特性是否可替代以及整个链接流程是否仍成立 |
| `can't find crate for core` | 目标 target 未安装 | `rustup target list --installed` | 提前安装/准备目标；断网现场不能临时下载，检查离线工具链包 |
| `linking with ... failed`、找不到 `linker.ld` | 没有生成 `user/.cargo`/`os/.cargo`，或从错误工作目录执行，或 target 选错 | `make cargo-config`; `cargo build -vv`; 检查 `*-config/config.toml` 和 `src/linker*.ld` | 从仓库根恢复配置；在 `user`/`os` 正确目录构建；使用对应架构 target |
| RV64 汇编寄存器报错、`invalid register` | 把 x86/LA64 寄存器写进 RISC-V，或 asm 约束不匹配 | 对照 `user/src/syscall.rs`；看 `cfg(target_arch)` | 复制同架构现有 `syscall/syscall6` 的约束；另一个架构用独立 cfg 分支 |
| LA64 syscall 后局部变量/指针异常 | caller-saved `$t0..$t8` 未声明可能被 clobber | 对照 `user/src/syscall.rs` LoongArch 分支 | 沿用现有 lateout clobber；优先调用已有 wrapper，不要手写 raw asm |
| `cannot move out of ...` / 借用后不能使用 | `String/Vec/Arc` 被 move，或从借用值取了拥有字段 | 看报错箭头和函数参数是否按值；查最后一次 move | 用 `&T`/`&mut T`、`clone()`（确认成本），或重排作用域；不要盲目加 clone 掩盖所有权设计 |
| `cannot borrow ... as mutable more than once` | 两个可变切片可能重叠，或一个 immutable borrow 仍存活 | 看是否同时保存 `&buf[..]` 和 `&mut buf[..]` | 用 `split_at_mut`、缩短作用域、先复制索引/长度；确保区间确实不重叠 |
| `borrowed value does not live long enough` | `Vec<&str>` 借用了随后被修改/销毁的 `String` | 检查 `argv`/`line`/`owned` 的生命周期和 `push`/`drain` | 用 `Vec<String>` 拥有内容，再最后取指针；或保证被借用值活到调用结束 |
| `mismatched types` 出现在 `if`/`match` | 分支返回类型不同，常见是一个分支多了/少了分号 | 检查每个分支最后表达式 | 统一返回类型；需要副作用时显式写 `()`，需要值时去掉分号 |
| `[value; N]` 报 `T: Copy` | 元素不是 Copy，例如 `AtomicUsize`/`String` | 看数组重复初始化表达式 | 使用 `[const { T::new(...) }; N]`（编译器支持时）、`core::array::from_fn` 或循环初始化；不要共享同一可变对象 |
| `the trait bound ... Display is not satisfied` | 使用 `{}` 打印了只实现 Debug 的值 | 看格式占位符 | 改 `{:?}` 并 derive `Debug`，或实现 `core::fmt::Display` |
| 运行时 `panic` 在 `buf[..n]` | `n` 未检查负数/大于 buffer，或协议长度不可信 | 在切片前打印/断言 `0 <= n <= buf.len()` | syscall 先检查 `< 0`；用 `min`/checked 计算；解析目录项先检查 record length |
| fd/长度变成极大值 | 负 `isize` 错误码直接 `as usize` | 在 cast 前查看原始返回值 | `if ret < 0 { ... } else { ret as usize }`；外部数值用 `TryFrom` |
| `unwrap()`/`assert!` 触发用户退出或内核关机 | `Option/Result`、边界、短读假设错误 | 根据用户 `lang_items.rs` 或内核 `lang_items.rs` 的 panic 输出定位 | 先判断错误/EOF/短读；只有已证明不可失败的位置保留 unwrap，并写明不变量 |
| 改了 `LOG=DEBUG` 但看不到 `debug!` | 没有重编译；`LOG` 是 `option_env!` 构建时值；release feature 还可能裁剪 | `make -C os kernel LOG=DEBUG`; `cargo build -vv`; 看 `os/Cargo.toml` | 使 kernel config stamp 变化并重编；必要时用无条件 `println!` 做最小诊断，完成后再移除 |
| `format!`/`Vec` 运行时 heap allocation error | 用户 128 KiB 堆耗尽；内核分配/页表/回收失败 | 看触发路径是否在 `__user_start` 后；减少 buffer/临时 String；检查 allocator 初始化 | 复用固定数组、分块处理、清理临时 Vec；用户不能假设堆会自动扩容 |
| 编译后程序启动立即异常/不进 `main` | `#![no_main]`、`#[no_mangle]`、入口签名或 linker script 不匹配 | 查 `user/src/lib.rs::__user_start`、bin 的 `main`、`cargo build -vv` | 沿用已有 bin 模板；不要定义 Rust 默认 main；保持 `main` 符号和 `i32` 返回约定 |
| 内核新增 public item 报 missing docs | `os/src/main.rs` 开启 `#![deny(missing_docs)]` | 看第一处 warning/error | 给 `pub` item 写文档，或按真实封装需求降为私有/`pub(crate)`；不要关闭全局 lint |
| Cargo 解析到与源码 API 不同的 crate 版本 | `Cargo.toml` 是范围版本，实际版本由 `Cargo.lock` 决定；bitflags 1/2、spin 0.7/0.9 API 可能不同 | 查看对应 `Cargo.lock` 和 `cargo metadata` | 离线用 `--locked`；先按锁文件 API 写；不要为修一个方法随意升级依赖 |

## 14. 现场排查顺序：从错误到修复

1. **先确认边界**：这是 `user`、`os`、`fs` 还是宿主 `fs-fuse`？目标是 RV64 还是 LA64？当前 crate 是 edition 2018 还是 2021？
2. **确认实际命令**：在正确目录执行 `cargo build -vv`，核对 `--target`、linker、rustflags、features、`LOG`，不要只看编辑器诊断。
3. **判断错误类别**：
   - 解析/trait/borrow：先缩小到最小函数和类型；
   - `core/alloc/std`：先检查 no_std 和依赖 feature；
   - 链接/入口：先检查 cargo config、linker、`no_main`、符号；
   - 运行时 panic：先看 buffer/整数/借用转 unsafe 的边界；
   - syscall 负值：先保留 `isize`，不要马上 cast。
4. **查相邻源码**：`rg -n '相似函数名|相似宏|target_arch|global_allocator' user/src os/src fs/src`，优先复制本仓库已经能编译的模式。
5. **最小修复和双目标验证**：先让 RV64 编译通过，再编译 LA64；如果改的是公共 API/结构体/宏，两个架构都要检查。
6. **最后再优化**：减少 clone、换固定 buffer、调整日志级别等优化必须建立在正确性和现场可重复构建之上。

## 15. 现场 checklist

### 开写前

- [ ] 已确认修改属于 `user`、`os`、`fs` 还是宿主工具，并确认目标架构。
- [ ] 已读对应 `Cargo.toml`、`Cargo.lock`、Makefile、cargo config 和相邻实现。
- [ ] 已确认当前 crate 是否 `#![no_std]`；没有把 `std` 示例直接复制进来。
- [ ] 需要 `Vec/String/Arc` 时，确认最终二进制已有 global allocator，且使用时 allocator 已初始化。
- [ ] 用户/内核 ABI 结构体有 `#[repr(C)]`，整数宽度和字节序已明确。

### 写代码时

- [ ] 每个 `if`/`match` 分支返回同一类型；注意无分号表达式。
- [ ] `String`/`Vec` 的 move、借用和生命周期清楚；不为躲 borrow checker 盲目 `clone`。
- [ ] 每次 `isize -> usize` 前都判断负值；长度/偏移用 checked 或显式边界检查。
- [ ] 不可信 buffer 先检查长度，再切片、解码和指针转换。
- [ ] `unsafe` 旁边写出地址有效、对齐、初始化、生命周期和别名证明。
- [ ] 格式化导入 `core::fmt::Write`；动态日志写成 `"{}"` 占位符。
- [ ] 用户输出使用仓库 `println!`/`write`；内核诊断按需要选择 `println!` 或 `log`。
- [ ] 全局状态优先 `const`、原子、仓库锁或 `lazy_static`；避免新增 `static mut`。
- [ ] 架构专属汇编均放在 `#[cfg(target_arch = ...)]` 分支，RV64/LA64 公共 API 保持一致。

### 编译前

- [ ] `rustc --version` 确认工具链满足仓库使用的 nightly features。
- [ ] `rustup target list --installed` 含当前目标；离线 cache 含锁文件要求的依赖。
- [ ] 已执行 `make cargo-config`，配置中的 linker script 路径存在。
- [ ] 用户态按 `make -C user build ARCH=riscv64` 或 `ARCH=loongarch64` 构建；内核按 `make -C os kernel ARCH=...` 构建。
- [ ] 离线复现时使用 `--locked --offline`，并记录真正执行的命令；不要让构建偷偷联网更新依赖。
- [ ] 必要时 `cargo fmt --check`、`cargo build -vv`，确认 feature/target/rustflags 没有串线。

### 交卷前

- [ ] 至少完成目标架构的 release 构建；公共代码/宏/ABI 改动已完成另一架构构建。
- [ ] 用户程序能走过 `__user_start`、堆初始化并进入自己的 `main`。
- [ ] syscall 的负 errno、EOF、短读/短写、空指针和 NUL 结尾约定都已处理。
- [ ] panic、alloc error、日志和锁内路径不会递归分配/递归打印造成更难定位的二次故障。
- [ ] 去掉临时大段 `println!`、探针和不适合 release 的 debug 代码，或明确用 feature/`LOG` 控制。
- [ ] 文档/命令没有把“本仓库当前实现”“依赖具体版本的行为”“稳定 Rust 通用知识”混成无条件事实。

## 16. 离线可查的官方知识关键词

现场若带有本地 Rust 文档，优先按以下官方资料名检索，并以现场工具链版本对应的 API 为准：

- *The Rust Programming Language*：Ownership and Borrowing、Enums and Pattern Matching、Common Collections、Packages and Crates、Recoverable Errors。
- *The Rust Reference*：Expressions、Patterns、Types、Items、Attributes、Conditional compilation、Unsafe code guidelines 相关章节。
- Rust API docs：`core::fmt`、`core::slice`、`core::convert`、`core::sync::atomic`、`alloc::vec::Vec`、`alloc::string::String`、`alloc::collections`。
- `rustc --print cfg --target <target>`、`cargo build -vv` 和本仓库两个 `cargo-config/config.toml`：用于确认现场真实 target/链接参数，不用记忆猜测。

官方资料解释的是语言一般规则；`user/src/lib.rs` 的 128 KiB 用户堆、`println!` 的 literal 限制、RV64/LA64 syscall 寄存器、`LOG` 的构建时语义等，属于 CosmOS 当前实现，必须以本文件前面的路径映射和源码为准。
