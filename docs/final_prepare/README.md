# CosmOS 现场决赛离线资料包

这组资料是给“断网、不能使用 Codex、只能依靠本地代码和编译器”的现场环境准备的。正文以当前仓库实现为准；架构规范和 Rust 官方文档列在每篇末尾，建议在比赛前把需要的 HTML/PDF 也下载到本地。

## 快速入口

| 文件 | 适合查什么 |
| --- | --- |
| [01-rust-syntax-no-std.md](01-rust-syntax-no-std.md) | Rust 语法、`core`/`alloc`、`no_std` |
| [02-rust-collections.md](02-rust-collections.md) | 容器、复杂度、借用冲突 |
| [03-rust-option-result-iterators.md](03-rust-option-result-iterators.md) | `Option`、`Result`、迭代器、闭包 |
| [04-rust-traits-generics-lifetimes.md](04-rust-traits-generics-lifetimes.md) | trait、泛型、生命周期、智能指针 |
| [05-rust-unsafe-pointers-layout.md](05-rust-unsafe-pointers-layout.md) | 裸指针、布局、`unsafe` 审查 |
| [06-rust-atomics-locks-memory-ordering.md](06-rust-atomics-locks-memory-ordering.md) | 原子、内存序、锁、SMP |
| [07-riscv64-privileged.md](07-riscv64-privileged.md) | RISC-V 特权架构、陷阱、Sv39、TLB |
| [08-loongarch64-privileged.md](08-loongarch64-privileged.md) | LoongArch64 CSR、异常、TLB、启动 |
| [09-elf-process-stack-syscall-abi.md](09-elf-process-stack-syscall-abi.md) | ELF、进程、用户栈、系统调用 ABI |
| [10-qemu-gdb-debugging.md](10-qemu-gdb-debugging.md) | 构建、QEMU、GDB、日志、反汇编 |
| [11-cosmos-codebase-map.md](11-cosmos-codebase-map.md) | CosmOS 模块地图、启动流程、改题入口 |

## 使用方法

1. 先看第 11 篇，记住目录和关键调用链。
2. Rust 代码写不出来时，按 01→05 查；遇到并发或 SMP 问题查 06。
3. 看到异常地址时，先查 07/08，再查 10；不要直接凭猜测改页表。
4. 做 `exec`、`fork`、系统调用或用户指针题时，按 09 的端到端链路走。
5. 每次改动只解决一个层次的问题，并保留一份能编译/能启动的版本。

## 仓库事实基线

- 内核是 `#![no_std]`、`#![no_main]`，但通过 `extern crate alloc` 使用堆容器；见 `os/src/main.rs` 和 `os/Cargo.toml`。
- RISC-V 目标是 `riscv64gc-unknown-none-elf`，内核使用 Sv39；LoongArch 目标是 `loongarch64-unknown-none`，当前页表抽象也是 39 位虚拟地址、三级索引。
- 架构分层主要在 `os/src/arch/riscv`、`os/src/arch/loongarch64`；共用抽象在 `os/src/hal/traits.rs`。
- `make -C os` 的 `ARCH` 接受 `riscv64/rv64/rv` 和 `loongarch64/la64/la`；默认是 RISC-V。
- 当前工作区可能有未提交的内核实验性改动。资料中的行号只作为定位线索，现场应以当前文件内容为准；函数名和路径比固定行号更可靠。

## 建议提前缓存的官方资料

这些链接是联网时的下载入口；比赛前应把页面另存为 HTML 或 PDF，放入本地资料目录，并实际测试离线搜索。

- Rust Reference：<https://doc.rust-lang.org/reference/>
- Rust `core`：<https://doc.rust-lang.org/core/>
- Rust `alloc`：<https://doc.rust-lang.org/stable/alloc/>
- Rust Embedded Book 的 `no_std`：<https://doc.rust-lang.org/stable/embedded-book/intro/no-std.html>
- Rust 原子类型：<https://doc.rust-lang.org/core/sync/atomic/>
- RISC-V Privileged Architecture：<https://docs.riscv.org/reference/isa/priv/priv-index.html>
- LoongArch 文档总入口：<https://loongson.github.io/LoongArch-Documentation/README-EN.html>
- LoongArch Volume 1：<https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html>
- QEMU GDB usage：<https://qemu.readthedocs.io/en/master/system/gdb.html>

## 现场最小检查表

- [ ] 完全断网后，仍能用 `rg` 搜到这组文件和仓库源代码。
- [ ] 已缓存 `rustc --print sysroot` 下的 Rust 文档，且知道目标环境支持哪些 `core`/`alloc` API。
- [ ] 能独立启动 RISC-V 和 LoongArch 两套 QEMU 命令。
- [ ] 能从 `sepc/stval/scause` 或 `ERA/BADV/ESTAT` 追到故障指令。
- [ ] 能写出一个最小的用户指针校验、页表遍历、锁保护队列和系统调用模板。
- [ ] 修改前已保存工作区状态；不使用 `git reset --hard` 等不可恢复操作。
