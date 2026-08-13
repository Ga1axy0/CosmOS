# CosmOS 现场速查：Rust `unsafe`、裸指针、内存布局与 FFI

> 目标：断网、不能查 Codex 时，快速判断一个 `unsafe` 块是否成立，能否把用户指针/物理地址/MMIO/磁盘字节转换成 Rust 类型，并能在 RISC-V 与 LoongArch 两条路径上定位问题。
>
> 本文以当前工作树的实际实现为准。仓库当前配置是 `nightly-2025-01-18`；内核、用户库、`fs`、LoongArch 直接启动器都使用 `no_std`，但现场临时实验可以在宿主机用 `std` 写一个等价的小测试。代码块都标明了适用环境；没有标注为“可直接复制到 CosmOS”的代码，不要直接粘进内核。

## 0. 先记住这一页

### `unsafe` 不变量总表

`unsafe` 只是把检查责任交给程序员，不是“允许 UB”。进入每一个裸指针操作前，按下面顺序回答：

| 不变量 | 必须能回答的问题 | 典型违反后果 |
|---|---|---|
| 地址/对象存活 | 指针指向哪个 allocation、页框、MMIO 寄存器或 FFI 缓冲区？它还活着吗？ | use-after-free、悬垂指针、随机崩溃 |
| 范围 | `len * size_of::<T>()` 是否溢出？整个区间是否在同一 allocation/同一已验证用户范围内？ | 越界读写、`from_raw_parts` UB |
| 对齐 | 地址是否满足 `align_of::<T>()`？页对齐不等于结构体对齐。 | Load/Store fault、未定义行为、`E0793` |
| 初始化/有效位模式 | 读出的每一个字段是否已经初始化？`bool`、枚举、引用、函数指针等值是否合法？ | 产生 invalid value、后续任意位置炸掉 |
| 别名与可变性 | 是否同时存在重叠的 `&mut`、`&` 或写入指针？是否有数据竞争？ | 优化后结果错、偶发崩溃 |
| 生命周期/回收 | 返回的引用活多久？页表解除映射、TLB 刷新、页框回收发生在它之前还是之后？ | stale TLB、回收后访问、跨线程悬挂 |
| 地址空间/硬件语义 | 这是用户 VA、内核 VA、PA 还是 MMIO VA？当前页表和权限允许吗？ | 内核页表故障、访问错误的设备或物理页 |
| ABI/布局 | Rust 类型的字段顺序、padding、调用约定、指针宽度和 C/汇编约定是否一致？ | FFI 参数错位、trap frame 损坏 |

最小审查模板：

```rust
// 适用环境：std/no_std 均可；内核/用户态均可；架构无关。
// SAFETY: 调用者保证：
// 1. ptr 来源于仍存活的单一对象/已验证的地址空间；
// 2. [ptr, ptr + len * size_of::<T>()) 不溢出且整个范围可读/可写；
// 3. ptr 满足 T 的对齐要求；
// 4. 读取时 T 已初始化且位模式有效；写入时不会违反别名规则；
// 5. 使用期间不会释放、解除映射、改变权限或并发修改该范围。
unsafe { /* 只放无法由 Rust 类型系统表达的最小操作 */ }
```

### 术语不要混用

| 名称 | 含义 | 能否直接解引用 |
|---|---|---|
| PA/物理地址 | 硬件地址，通常只是 `usize` | 不能；先经过平台 direct map 或设备映射 |
| VA/虚拟地址 | 当前地址空间中的数值 | 不能只因数值看起来有效就解引用；要有当前页表和权限 |
| `*const T` / `*mut T` | Rust 裸指针，带类型和指针语义但不自动验证 | 只有在满足不变量时才能读/写 |
| `&T` / `&mut T` | 已向 Rust 承诺有效、对齐、存活和别名规则的引用 | 可以安全使用；承诺不能撤回 |
| `NonNull<T>` | 非空裸指针包装，不代表拥有对象 | 仍需验证可读写、对齐、生命周期 |
| `&[T]` / `&mut [T]` | 指针 + 元素数的引用视图 | `len` 是元素数，不是字节数 |
| MMIO | 设备寄存器，不是普通 RAM | 通常使用正确宽度的 `read_volatile`/`write_volatile` |

## 1. 当前仓库的环境和构建边界

### 1.1 工具链、crate 和 target

来自当前仓库：

- 根目录 [`rust-toolchain.toml`](../../rust-toolchain.toml)：`nightly-2025-01-18`、`llvm-tools-preview`、`riscv64gc-unknown-none-elf`。
- [`os/Cargo.toml`](../../os/Cargo.toml)：edition 2021，内核 `#![no_std]`/`#![no_main]`；默认 feature 含 `ext4`、`platform-qemu-virt` 及若干诊断/缓存 feature。Makefile 的正式内核命令会使用 `--no-default-features` 再显式打开文件系统和额外 feature。
- [`user/Cargo.toml`](../../user/Cargo.toml)：edition 2018，`user_lib`，`no_std` 用户库；[`user/rust-toolchain.toml`](../../user/rust-toolchain.toml) 同样固定到 `nightly-2025-01-18`。
- [`fs/Cargo.toml`](../../fs/Cargo.toml)：edition 2018，`no_std` 文件系统 crate；`fs/src/ext4_rs` 另有自己的 `no_std` crate 和同一 nightly 配置。
- [`bootloader/loongarch64-direct/Cargo.toml`](../../bootloader/loongarch64-direct/Cargo.toml) 与其 [`src/main.rs`](../../bootloader/loongarch64-direct/src/main.rs)：crate edition 2021，入口源码启用 `no_std`/`no_main`/`naked_functions`，只用于 LoongArch 直接启动路径。
- [`os/cargo-config/config.toml`](../../os/cargo-config/config.toml)：把 RISC-V target 链接到 `os/src/linker.ld`，把 LoongArch target 链接到 `os/src/linker-loongarch64.ld`，并强制保留 frame pointer。

常用 target：

| 场景 | target/架构 | 代码中常见 cfg |
|---|---|---|
| 内核/用户 RISC-V | `riscv64gc-unknown-none-elf` | `target_arch = "riscv64"` |
| 内核/用户 LoongArch | `loongarch64-unknown-none` | `target_arch = "loongarch64"` |
| 宿主机小实验 | 当前宿主机 target | 通常是 `std`，不能假设有 CosmOS 页表/MMIO |

`std`/`no_std` 只改变可用库，不改变 Rust 的 UB 规则。`no_std` 仍然有 `core::ptr`、`core::slice`、`core::mem::MaybeUninit`、`core::ptr::NonNull`；内核代码应优先使用 `core` 路径，用户库也通过 `core`/`alloc` 工作。

### 1.2 CosmOS 地址常量（当前 QEMU virt 配置）

这些是仓库当前配置，不是所有板卡或未来提交的通用 ABI；改 target、板卡、页表级数或 linker 后必须重新确认。

| 项目 | RISC-V QEMU virt | LoongArch QEMU virt | 来源 |
|---|---:|---:|---|
| 页大小 | `0x1000` | `0x1000` | [`os/src/config.rs`](../../os/src/config.rs) |
| VA bits / 页表 | 39 / Sv39，3 级，每级 9 bit | 39，3 级，每级 9 bit | [`os/src/arch/riscv/paging.rs`](../../os/src/arch/riscv/paging.rs)、[`os/src/arch/loongarch64/paging.rs`](../../os/src/arch/loongarch64/paging.rs) |
| PA bits | 56 | 48 | 同上 |
| 用户区上界 | `USER_SPACE_END = 1 << 38` | `USER_SPACE_END = 1 << 38` | [`os/src/mm/address.rs`](../../os/src/mm/address.rs)、[`os/src/hal/traits.rs`](../../os/src/hal/traits.rs) |
| `mmap(NULL, ...)` 基址 | `0x1_0000_0000` | `0x20_0000_0000` | `platform/*/qemu_virt/board.rs` |
| 用户栈基址 | `0x0800_0000` | `0x3e_0000_0000` | 同上 |
| 动态链接器基址 | `0x2_0000_0000` | `0x1e_0000_0000` | 同上 |
| RAM direct-map 形式 | `KERNEL_ADDR_OFFSET + pa`，offset `0xffff_ffc0_0000_0000` | `pa | 0x9000_0000_0000_0000`（cached DMW1） | [`os/src/platform/riscv/qemu_virt/mod.rs`](../../os/src/platform/riscv/qemu_virt/mod.rs)、[`os/src/platform/loongarch/qemu_virt/mod.rs`](../../os/src/platform/loongarch/qemu_virt/mod.rs) |
| MMIO 访问形式 | 高地址 MMIO aperture `0xffff_ffe0_0000_0000 + pa` | uncached DMW0，`pa | 0x8000_0000_0000_0000` | 同上 |
| trap trampoline | `usize::MAX - 0x1000 + 1` | `0x0000_003f_ffff_f000` | `platform/*/qemu_virt/mod.rs` |

RISC-V `PagingArch::normalize_virt_addr_input` 会取低 39 bit，之后 `canonicalize_vaddr` 在转换回 `usize` 时按 Sv39 规则补高位；LoongArch 当前实现不做同样的 mask。不要把“某架构上看起来相同的 `usize`”当作跨架构稳定地址。

## 2. 裸指针：从整数到可用对象要经过什么

### 2.1 创建指针不等于访问内存

```rust
// 适用环境：std/no_std；内核态或用户态；RISC-V/LoongArch 均可。
let addr: usize = 0x1000;
let ptr = addr as *mut u32; // 只做数值转换；尚未证明可写、存活或对齐

if !ptr.is_null() {
    // 仍然不能直接 *ptr。
    // SAFETY 必须说明当前地址空间、映射、权限、对齐和生命周期。
    let value = unsafe { core::ptr::read_volatile(ptr) };
    let _ = value;
}
```

地址转换只告诉编译器类型，不会：

1. 检查地址是否落在当前页表的有效 PTE；
2. 检查用户/内核权限、读写/执行权限；
3. 把 PA 变成当前 CPU 可访问的 kernel VA；
4. 把不对齐的地址自动修正；
5. 延长页框、VMA、设备或 FFI 缓冲区的生命周期；
6. 解决并发访问或 TLB 中的旧翻译。

从 `usize` 计算地址时先做整数级检查，再 cast：

```rust
// 适用环境：std/no_std；地址检查模板；不执行实际内存访问。
fn checked_span(start: usize, len: usize, limit: usize) -> Option<(usize, usize)> {
    let end = start.checked_add(len)?;
    (start <= end && end <= limit).then_some((start, end))
}

fn checked_byte_offset(base: usize, offset: usize) -> Option<*const u8> {
    // wrapping_add 不用于掩盖溢出；这里明确使用 checked_add。
    Some(base.checked_add(offset)? as *const u8)
}
```

### 2.2 `add`、`offset`、`wrapping_add` 和 `byte_add`

- `ptr.add(n)` 按 `T` 元素步长计算，要求结果仍在同一 allocation 的范围内或 one-past；`n` 不能导致越界。它适合已知来自同一个 Rust 对象的数组/切片。
- `ptr.offset(n)` 同样按元素步长，且有更严格的有符号 in-bounds 要求；除非确实需要负偏移，不要用它代替检查。
- `ptr.wrapping_add(n)` 只做包裹式指针运算，不能让越界指针变得可解引用；最终访问仍必须落在合法对象内。它也不替代 `usize::checked_add` 的范围检查。
- `ptr.byte_add(n)`（如果当前 toolchain 提供）按字节移动，仍要求相应的 pointer arithmetic 前提；不要因它名字里有 `byte` 就跳过 allocation/溢出检查。
- 系统调用传入的用户地址本质上是“目标地址空间里的整数”。对它逐字节使用 `ptr.add` 会把 Rust allocation 语义和用户页表语义混在一起；内核推荐先做 `checked_add`，按页翻译后复制。当前 `read_cstring_from_user` 使用 `ptr.add(offset)`，其安全前提由外层的用户范围、逐页翻译和最大长度检查承担；审查时要同时检查这几层，而不是只看这一行。

### 2.3 `read` / `write` / `copy`

| 操作 | 语义 | 关键前提 |
|---|---|---|
| `ptr::read(src)` | 从 `src` 按位复制一个 `T`，源内存不变 | 可读、对齐、已初始化且 `T` 位模式有效；非 `Copy` 类型不能继续同时使用源值 |
| `ptr::write(dst, value)` | 写入一个 `T`，不先 drop 旧值 | `dst` 可写且对齐；适合未初始化存储或明确覆盖旧值 |
| `read_unaligned` / `write_unaligned` | 读/写不要求 `T` 对齐 | 仍要求范围、存活和初始化/有效值；不要先创建指向 packed 字段的引用 |
| `copy_nonoverlapping(src,dst,count)` | 按 `T` 复制 `count` 个元素，类似 `memcpy` | 两边有效、对齐、范围完整且绝不重叠；`count` 是元素数 |
| `copy(src,dst,count)` | 允许重叠，类似 `memmove` | 两边有效、对齐、范围完整；按元素计数 |

注意：`ptr::read` 不是“从字节解释成结构体”的通用工具。用户/磁盘字节可能未对齐，也可能不满足 `enum`/`bool`/引用等有效位模式；这种场景用 `read_unaligned` 只解决对齐，不解决有效性。

#### 直接复制字节的模板

```rust
// 适用环境：std/no_std；内核/用户态均可；普通 RAM，不是 MMIO。
fn copy_bytes(dst: *mut u8, src: *const u8, len: usize) {
    // SAFETY: 调用者保证两个范围各自可访问、未溢出、满足 u8 对齐，且不重叠。
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) }
}

// 如果源和目的可能重叠，用 copy，不要把 copy_nonoverlapping 当成优化提示。
fn move_bytes(dst: *mut u8, src: *const u8, len: usize) {
    // SAFETY: 调用者保证整个源/目的范围有效、未溢出、u8 对齐。
    unsafe { core::ptr::copy(src, dst, len) }
}
```

### 2.4 地址运算和指针 provenance 的现场策略

在内核里常见的 `pa as *mut u8`、`user_arg as *const T` 是必要的边界代码，但不能把它们扩散到业务逻辑。推荐分层：

1. **数值层**：`checked_add`、页对齐、`USER_SPACE_END`、长度上限、物理地址宽度检查。
2. **地址空间层**：RISC-V/LoongArch 页表翻译、PTE `U/R/W/X` 检查、缺页/COW/page cache 处理。
3. **Rust 视图层**：只在已验证的单页/单对象上建立 `&T`、`&mut T` 或 `&[u8]`。
4. **业务层**：只接收 `Vec<u8>`、`UserBuffer`、已验证的 POD 值或封装后的 MMIO 类型。

不要把一个任意整数地址保存成长期 `&'static mut T`。如果页表编辑、TLB shootdown 或页框回收可能在该引用存活期间发生，必须缩短引用的作用域，或保留拥有页框/锁/地址空间的 guard。

## 3. slice 与 `from_raw_parts`

### 3.1 Safety 条件

`core::slice::from_raw_parts(data, len)` 的 `len` 是 **元素数**。建立 slice 时必须同时满足：

- `data` 非空、满足 `T` 对齐；即使 `len == 0` 也不能随便传 null 或不对齐地址；
- `data` 对 `len * size_of::<T>()` 字节可读，并且每个 `T` 已初始化且位模式有效；
- 整个范围属于同一个 allocation；两个刚好相邻但来自不同 allocation 的 slice 不能拼成一个 slice；
- 总字节数不超过 `isize::MAX`，地址加长度不 wrap；
- 返回 `&[T]` 的生命周期内不能通过其他路径修改它（除非使用 `UnsafeCell` 语义）。

`from_raw_parts_mut` 另外要求独占可读写：返回的 `&mut [T]` 存活期间不能从其他未派生指针读或写同一范围。

零长度模板：

```rust
// 适用环境：std/no_std；仅演示 slice 构造，调用者必须承担 unsafe 合同。
unsafe fn bytes_from_raw<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        // 空 slice 不需要来自调用者的 allocation；不要用 null 传给
        // from_raw_parts。&[] 是最简单的合法结果。
        return Some(&[]);
    }
    if ptr.is_null() || len > isize::MAX as usize {
        return None;
    }
    // u8 对齐为 1；调用者仍须保证 ptr..ptr+len 在同一 live allocation。
    Some(core::slice::from_raw_parts(ptr, len))
}
```

这个函数仍然是 `unsafe fn`：它无法仅靠 `ptr.is_null()` 证明 allocation、可读性、别名和生命周期。不要为了让调用点变成 safe 就把这些条件藏掉。

### 3.2 跨页用户缓冲区：不要伪造一块连续 slice

用户 VA 连续，不代表：

- 物理页连续；
- 在内核 direct map 中连续；
- Rust 眼里属于同一个 allocation；
- 中间每一页都有相同的 `U/R/W` 权限。

当前 CosmOS 的设计是把一段用户缓冲区拆成每页一段：

- [`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的 `translated_byte_buffer`：根据 token 翻译每一页，返回 `Vec<&'static mut [u8]>`；
- [`os/src/syscall/utils.rs`](../../os/src/syscall/utils.rs) 的 `translated_byte_buffer_with_access`：先检查每页用户权限，必要时触发 lazy/COW/file-backed fault，再调用翻译；
- [`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的 `translated_ref`/`translated_refmut`：只适合对象不跨页的情况；注释已明确跨页要用 byte buffer；
- [`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的 `UserBuffer`/`UserBufferIterator`：供文件、tty 等逐字节/分段消费。

内核 syscall 的推荐调用方式：

```rust
// 适用环境：CosmOS 内核 no_std；用户指针；RISC-V/LoongArch。
use crate::mm::PageFaultAccess;
use crate::syscall::translated_byte_buffer_with_access;

fn copyout_user(dst: *mut u8, src: &[u8]) -> Result<(), crate::syscall::errno::ERRNO> {
    let mut pieces = translated_byte_buffer_with_access(
        dst as *const u8,
        src.len(),
        PageFaultAccess::Write,
    )?;
    let mut at = 0usize;
    for piece in &mut pieces {
        let n = piece.len();
        piece.copy_from_slice(&src[at..at + n]);
        at += n;
    }
    Ok(())
}
```

不要改成下面这种未经验证的写法：

```rust
// 适用环境：CosmOS 内核；反例，不要复制。
unsafe fn bad_copyout(dst: *mut u8, src: &[u8]) {
    // 可能跨页、跨 allocation、越过 USER_SPACE_END，且没有 PTE W/U 检查。
    let out = core::slice::from_raw_parts_mut(dst, src.len());
    out.copy_from_slice(src);
}
```

### 3.3 当前仓库里与 slice 相关的重点审查点

- `os/src/mm/address.rs::PhysPageNum::get_bytes_array` 和 `get_pte_array` 返回 `'static mut` slice。它们只适合内核掌握页框所有权且物理 direct map 稳定的内部路径；不能把这种返回值当作任意用户内存的通用借用。
- `os/src/mm/page_table.rs::translated_byte_buffer` 把物理页的字节数组转成可变 slice。调用者还必须保证 token 对应的 root、页框和 TLB 状态在 slice 使用期间不失效。
- `os/src/bootinfo.rs::bytes_at` 直接从固件/FDT 地址制造 `'static` slice；其前提是固件给出的 blob 在整个解析期间仍驻留、范围经过 FDT 长度检查。它不是用户指针 helper。
- `os/src/fs/page_cache.rs` 在页框物理连续、页被 pin/LOADING 且 direct map 可用时，把连续页作为一个读入缓冲区；这是由 `physically_contiguous_page_run` 和 page-cache 生命周期共同承担的特例。不要从这段代码推导“任意两个 page frame 都能拼成 slice”。
- `fs/src/easyfs/layout.rs::DirEntry::as_bytes` 和 `as_bytes_mut` 依赖 `DirEntry` 大小等于磁盘格式的 `DIRENT_SZ`；若结构体字段或 padding 改动，必须同时更新格式断言/序列化逻辑。
- `fs/src/ext4_rs/src/ext4_defs/block.rs::Block::read_as` 用 `read_unaligned` 读磁盘字节，但 `read_as_mut`/`read_offset_as_mut` 会直接制造 `&mut T`；调用前仍须证明对齐、初始化、位模式和 endian，不能因为类型来自 ext4 就跳过这些检查。

## 4. `MaybeUninit`：从不可信字节得到类型

### 4.1 正确模型

`MaybeUninit<T>` 允许存放“尚未成为合法 `T` 的位”。但：

- `MaybeUninit::uninit().assume_init()` 永远不能代替初始化；
- `assume_init()` 前必须证明 `T` 的每个字段已初始化且有效；
- 只把字节拷完不等于所有位模式都合法。`bool`、有离散 discriminant 的 enum、引用、函数指针、某些 niche 优化类型都不能接收任意字节；
- 不要因为类型实现了一个手写 marker trait 就跳过审查；marker trait 本身不由编译器验证；
- 用 `MaybeUninit<T>` 写 out-parameter 时，使用 `ptr::write` 或按字节复制到未初始化存储；不要在初始化完成前制造一个供读取的 `&T`。

### 4.2 CosmOS 当前 POD 读写路径

[`os/src/syscall/utils.rs`](../../os/src/syscall/utils.rs) 定义了空的 `Pod` trait，并提供：

- `read_pod_from_user<T: Pod>`：先 `read_bytes_from_user`，再把用户字节写入 `MaybeUninit<T>`，最后 `assume_init`；
- `write_pod_to_user<T: Pod>`：把内核 `T` 按字节看成 `u8`，再通过 `write_bytes_to_user` 分页写回；
- `translated_byte_buffer_with_access`：权限检查和缺页/COW 处理在 POD 转换之前发生。

当前实现的 `Pod` 是人工白名单，新增实现时必须检查：

1. 是否 `#[repr(C)]`；
2. 是否只含固定宽度整数、明确的 padding 和有定义的指针数值；
3. 是否 `Copy`，是否可以按位复制而不触发所有权/析构问题；
4. 是否允许用户输入的每一位模式；
5. 是否需要显式 endian 转换或版本字段；
6. 是否应逐字段解析，而不是整体 `assume_init`。

特别注意：把 `MaybeUninit<T>` 的存储直接 cast 成 `&mut [u8]` 再写，是现场应重点复核的模式；`from_raw_parts_mut::<u8>` 的合同包含“指向已初始化元素”的要求。更保守的写法是用 `ptr::copy_nonoverlapping` 写入 `MaybeUninit<T>` 的原始存储，或使用 `[MaybeUninit<u8>; N]`/逐字段构造，完成后再 `assume_init`。当前仓库已有这种 cast 模式，文档不把它自动视为“只要有 MaybeUninit 就安全”。

### 4.3 更保守的整数型 POD 模板

```rust
// 适用环境：std/no_std；内核/用户态均可；仅 T 为“所有位模式有效”的 plain-data 类型。
use core::mem::{size_of, MaybeUninit};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct WireHeader {
    len: u32,
    flags: u32,
}

fn decode_wire_header(bytes: &[u8]) -> Option<WireHeader> {
    if bytes.len() != size_of::<WireHeader>() {
        return None;
    }
    let mut out = MaybeUninit::<WireHeader>::uninit();
    // SAFETY:
    // - bytes 的长度恰好覆盖 WireHeader；
    // - out 的存储足够且按 WireHeader 对齐，cast 后按 u8 写入；
    // - WireHeader 只含 u32，所有 bit pattern 对 u32 都有效；
    // - copy 后再 assume_init，期间没有读取未初始化的 T。
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            out.as_mut_ptr().cast::<u8>(),
            size_of::<WireHeader>(),
        );
        Some(out.assume_init())
    }
}
```

若磁盘/网络格式规定小端，不要直接把本机布局当小端：

```rust
// 适用环境：std/no_std；普通字节协议；架构无关。
use core::convert::TryInto;

fn le_u32(bytes: &[u8]) -> Option<u32> {
    let a: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(a))
}
```

这通常比把磁盘 buffer cast 成 `*const u32` 更容易审查。`fs/src/ext4_rs` 使用 `read_unaligned`/明确 little-endian 的位置要和 ext4 磁盘格式一起看，不能只看 `#[repr(C)]`。

## 5. `NonNull`：非空不是拥有，也不是已验证

### 5.1 语义速查

- `NonNull<T>` 的值不能为 null；`Option<NonNull<T>>` 可以利用 niche，通常与一个裸指针同大小。
- `NonNull<T>` 不自动拥有、不自动 drop、不自动延长生命周期；它甚至可以暂时 dangling，只要不解引用。
- `NonNull::new_unchecked` 的调用者必须证明非空；能用 `NonNull::new` 就不要手写 `new_unchecked`。
- `NonNull<T>` 默认对 `T` 协变。若封装类型会通过它改变 `T`，并且 `T` 可能含短生命周期引用，要用 `PhantomData<Cell<T>>` 等方式表达 invariant。
- `NonNull<T>` 也不自动带来 `Send`/`Sync`，设备寄存器、DMA buffer、并发队列仍需单独审查。

### 5.2 CosmOS MMIO/allocator 的对应关系

- [`os/src/drivers/block/mod.rs`](../../os/src/drivers/block/mod.rs)、[`os/src/drivers/net/mod.rs`](../../os/src/drivers/net/mod.rs) 用 `NonNull<VirtIOHeader>` 表示探测到的 VirtIO MMIO header；先 `NonNull::new(addr as *mut VirtIOHeader)` 判空，再由 `MmioTransport::new` 和平台 MMIO 合同承担后续安全性。
- [`os/src/mm/heap_allocator.rs`](../../os/src/mm/heap_allocator.rs) 用 `NonNull<u8>` 表示 buddy allocator 返回的非空块；free-list 节点本身把空闲块内存当作指针存储，所有 `push/pop/remove` 的地址、对齐、块大小和锁不变量必须一起看。
- `NonNull::dangling()` 只适合表达“尚未有元素但需要一个合法对齐指针”的空容器/零长度状态；不能把它当作可访问的物理地址或 MMIO 地址。

推荐的设备探测模板：

```rust
// 适用环境：CosmOS 内核 no_std；RISC-V/LoongArch；MMIO。
use core::ptr::NonNull;

fn probe_header(addr: usize) -> Option<NonNull<u32>> {
    let header = NonNull::new(addr as *mut u32)?;
    // SAFETY: 调用者已确认 addr 位于本平台设备 aperture、寄存器存在且按 u32 对齐；
    // read_volatile 只能保证编译器不会删掉访问，不会替你检查硬件地址。
    let magic = unsafe { core::ptr::read_volatile(header.as_ptr()) };
    (magic == 0x7472_6976).then_some(header)
}
```

## 6. 内存布局：`repr(C)`、`repr(transparent)`、packed 和 padding

### 6.1 默认 Rust layout 不可当 C ABI

没有 `repr` 时，Rust 只提供满足安全性所需的有限布局保证；字段顺序、padding 的具体安排不应作为磁盘/系统调用/汇编 ABI。需要跨边界时至少使用：

```rust
// 适用环境：std/no_std；内核/用户态；RISC-V/LoongArch。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoVecAbi {
    pub base: usize,
    pub len: usize,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserAddress(pub usize);
```

`#[repr(C)]` 的结构体布局按声明顺序放字段，在每个字段前按字段对齐补 padding，最后按结构体最大对齐补尾部 padding。它保证布局规则更接近 C，但不自动保证语义、长度、指针有效性或 enum discriminant 合法。

`#[repr(transparent)]` 用于一个非零大小的字段包装新类型；它使包装类型在布局和 ABI 上与该字段相同，适合 `PhysAddr(usize)` 这类类型安全 newtype。它不是“任意多个字段都透明”，也不会自动把地址变成有效指针。

`#[repr(C, align(N))]` 提高对齐；`#[repr(C, packed)]` 降低对齐。packed 类型的字段可能不对齐，不能写：

```rust
// 适用环境：std/no_std；演示 packed 反例和修复；不要取 packed 字段引用。
#[repr(C, packed)]
struct DiskHeader {
    tag: u8,
    len: u32,
}

fn read_len_bad(h: &DiskHeader) -> u32 {
    // 反例：&h.len 可能制造未对齐引用；新版本 rustc 还可能直接报 E0793。
    // h.len
    let _ = h;
    0
}

fn read_len_good(ptr: *const DiskHeader) -> u32 {
    // SAFETY: ptr 指向仍存活的 DiskHeader 字节，read_unaligned 不创建 &u32。
    unsafe { core::ptr::addr_of!((*ptr).len).read_unaligned() }
}
```

更常见且更易维护的磁盘解析是从 `[u8]` 取固定字节，用 `from_le_bytes`；只有在已经验证大小、对齐、字段有效性时才将 buffer 视为 `repr(C)` 类型。

### 6.2 布局断言模板

```rust
// 适用环境：std/no_std；编译期布局检查；需要当前 toolchain 支持 offset_of!。
use core::mem::{align_of, size_of};

#[repr(C)]
struct AbiHeader {
    kind: u32,
    ptr: usize,
}

const _: () = {
    assert!(size_of::<AbiHeader>() >= size_of::<u32>() + size_of::<usize>());
    assert!(align_of::<AbiHeader>() == align_of::<usize>());
    // 若 C/汇编 ABI 要求精确 offset，再按目标架构写断言：
    // assert!(core::mem::offset_of!(AbiHeader, ptr) == 8); // 64-bit 示例
};
```

不要在同时支持 32/64 bit 的代码里无条件写 `offset == 8`。CosmOS 当前两条目标都是 64-bit，但 `usize` 是 target-dependent；`IoVec`、`Stat`、`MsgHdr` 等 ABI 结构必须在每个 target 上检查 `size_of`、字段 offset 和 padding。

### 6.3 枚举、指针字段和 padding

- `#[repr(i32)]`/`#[repr(u8)]` 只固定 discriminant 的表示，不能从任意用户字节直接产生一个 Rust enum；先验证数值或解析成整数。
- `Option<&T>`、`Option<NonNull<T>>`、函数指针等可能利用 niche；按字节读写前要有明确的有效值规则。
- C 结构体的 padding 可能不应暴露给用户或磁盘；写回 syscall 时最好显式初始化 padding，避免泄漏未初始化/旧栈字节。
- `usize`、裸指针、`size_t` 的大小随 target 变化；磁盘格式优先使用 `u32/u64`，syscall ABI 则按目标 C ABI 定义。

## 7. `volatile`、MMIO 与普通内存

### 7.1 `volatile` 能做什么、不能做什么

`read_volatile`/`write_volatile` 告诉编译器这次访问必须保留，适合设备寄存器等“不是普通内存”的位置。它们：

- 不提供原子性；
- 不等价于 `Ordering::Acquire/Release`；
- 不自动成为 CPU/设备之间的内存屏障；
- 不修复错误的地址、宽度、对齐、生命周期或寄存器协议；
- 不应作为普通 RAM 并发同步的替代品。

并发共享内存使用原子类型、锁和目标架构要求的 fence；MMIO 顺序还要遵守设备手册及架构屏障（例如当前 LoongArch 平台代码在切换/设备路径中使用 `dbar`/`ibar` 的位置）。

### 7.2 CosmOS 的 MMIO 映射

当前实现中的实际例子：

- RISC-V UART、VirtIO、PLIC、RTC 使用高地址 MMIO aperture；平台地址在 [`os/src/platform/riscv/qemu_virt/board.rs`](../../os/src/platform/riscv/qemu_virt/board.rs) 和 [`mod.rs`](../../os/src/platform/riscv/qemu_virt/mod.rs)。
- LoongArch UART、VirtIO、RTC、PCI/中断设备使用 uncached DMW0 地址；平台地址在 [`os/src/platform/loongarch/qemu_virt/board.rs`](../../os/src/platform/loongarch/qemu_virt/board.rs) 和 [`mod.rs`](../../os/src/platform/loongarch/qemu_virt/mod.rs)。
- [`os/src/drivers/chardev/ns16550a.rs`](../../os/src/drivers/chardev/ns16550a.rs) 的 `Mmio<T>` 将寄存器封装成 `read_volatile`/`write_volatile`。
- [`os/src/drivers/block/mod.rs`](../../os/src/drivers/block/mod.rs)、`drivers/net/mod.rs` 先用 `NonNull` 检查 VirtIO header，再以 `u32` volatile 读取 magic/version/device id。
- [`bootloader/loongarch64-direct/src/main.rs`](../../bootloader/loongarch64-direct/src/main.rs) 直接用 `read_volatile`/`write_volatile` 驱动 UART，并通过 `transmute` 跳到固定 kernel entry；这个例子只适用于当前 direct boot 链路。
- [`os/src/bootinfo.rs`](../../os/src/bootinfo.rs) 的 fw_cfg/FDT 读取用 volatile 访问设备数据，并把返回 blob 拷到静态缓冲区；不要把设备寄存器当普通 cacheable RAM 用 `copy_from_slice` 批量访问。

MMIO wrapper 的最小模板：

```rust
// 适用环境：CosmOS 内核 no_std；MMIO；RISC-V/LoongArch 均可。
#[derive(Copy, Clone)]
struct Reg8 {
    addr: *mut u8,
}

impl Reg8 {
    const fn new(addr: usize) -> Self {
        Self { addr: addr as *mut u8 }
    }

    fn read(&self) -> u8 {
        // SAFETY: addr 是平台已映射、8-bit 对齐且具有 read 语义的 UART 寄存器。
        unsafe { core::ptr::read_volatile(self.addr) }
    }

    fn write(&self, value: u8) {
        // SAFETY: addr 是平台已映射、8-bit 对齐且具有 write 语义的 UART 寄存器。
        unsafe { core::ptr::write_volatile(self.addr, value) }
    }
}
```

不要对 MMIO 使用 `&mut T` 长期借用：Rust 可能据此假设内存不会从别处变化，而设备正是会从别处变化的对象。把每次访问缩到 volatile 原语附近，寄存器宽度按硬件手册固定。

## 8. 物理地址、页表、用户指针和 `mmap`

### 8.1 四种“地址”在 CosmOS 中的转换链

```text
适用环境：CosmOS 内核地址/页表说明；RISC-V/LoongArch 当前 QEMU virt。
用户传入的整数
    │ 先做 checked_add / 用户上界 / 长度检查
    ▼
用户 VA ──当前进程 PageTable::translate──► PTE (PPN + U/R/W/X)
    │                                      │
    │                                      └─PPN -> PA
    ▼
每页用户字节 ──平台 direct_map_phys_to_virt──► 内核可访问 VA
                                              │
                                              └─建立短生命周期的 &[u8]/&mut [u8]
```

不要走 `用户 VA as *mut T` 直接解引用；不要走 `PA as *mut T` 直接解引用。当前内核的正确入口是 `translated_*` 或 `PhysAddr/PhysPageNum` 的内部方法。

### 8.2 `os/src/mm/address.rs` 的类型和边界

- `PhysAddr(pub usize)`、`VirtAddr(pub usize)`、`PhysPageNum`、`VirtPageNum` 都是 `#[repr(C)]` newtype；它们的 `.0` 是数值，不是已验证引用。
- `VirtAddr::floor/ceil/page_offset/aligned` 和 `PhysAddr` 对应方法用于页边界计算；`VirtPageNum::from(VirtAddr)`、`PhysAddr -> PhysPageNum` 在不对齐时会 `assert_eq!`。
- `PhysAddr::get_ref/get_mut` 使用 `phys_to_virt` 后建立 `'static` 引用；这是内核内部物理 direct-map 合同，调用者必须保证 PA 对象存活、类型对齐/初始化，并且没有冲突别名。
- `PhysPageNum::get_pte_array` 用 `page_table_index_bits()` 得到每级表项数；`get_bytes_array` 固定返回 4096 字节。当前代码假设页大小是 `0x1000`，不能在改页大小后只改常量而不改这些长度。
- `phys_to_virt`/`virt_to_phys` 通过 [`os/src/platform/mod.rs`](../../os/src/platform/mod.rs) 绑定具体平台；它们不是用户地址翻译函数。

### 8.3 `PageTable` 和 PTE

[`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的主要合同：

- `PageTableEntry::new(ppn, flags)` 调用架构层 `make_pte`；RISC-V 与 LoongArch 的 bit layout 不同，不能手写另一架构的 PTE bit。
- `map` 先创建中间页表，再写 leaf PTE；`unmap/clear/replace/update_flags` 改 PTE，但调用方仍要处理 TLB shootdown 和旧页框回收。
- `translate(vpn)` 只返回内存中的 PTE 快照，不等于当前 hart TLB 已更新；`translate_va` 在 PTE PPN 上加页内 offset。
- `PageTable::from_token(token)` 是一个不持有 root frame 的视图；token 必须来自仍存活的地址空间，不能把用户传来的任意整数当 token。
- RISC-V 使用 `satp`/`sfence.vma`；LoongArch 使用 `PGDL`/`invtlb`。这些架构函数在 [`os/src/arch/riscv/paging.rs`](../../os/src/arch/riscv/paging.rs)、[`os/src/arch/loongarch64/paging.rs`](../../os/src/arch/loongarch64/paging.rs)。

PTE 的安全检查至少分两层：

1. **Rust/数值层**：VA 范围、VPN 计算、页表页本身可访问、PPN 来自已拥有/有效 frame；
2. **用户权限层**：PTE 必须有 `U`，读/写/执行按本次操作分别有 `R/W/X`。`translated_byte_buffer` 的基础版本主要做翻译，syscall 应使用带 `PageFaultAccess` 的 wrapper。

当前 `sys_mmap` 使用的 `prot_to_map_perm` 会把 `PROT_WRITE` 同时升级成 `R|W`；这是为了满足 RISC-V leaf PTE 的 `W=1,R=0` 无效编码约束。不要把用户 API 的 `PROT_WRITE` 机械等同于只设置 PTE W；该 helper 是两架构共用实现，改架构时仍应核对对应 PTE 规则。

### 8.4 `mmap`/缺页/回收的 unsafe 边界

当前链路：

1. [`os/src/syscall/mman.rs`](../../os/src/syscall/mman.rs)::`sys_mmap` 检查页对齐、长度溢出、`USER_SPACE_END`、`MAP_SHARED/MAP_PRIVATE` 组合和文件权限；
2. [`os/src/task/process.rs`](../../os/src/task/process.rs)::`ProcessControlBlock::mmap`/`mmap_file` 持有进程内锁并调用 `MemorySet`；
3. [`os/src/mm/memory_set.rs`](../../os/src/mm/memory_set.rs) 登记 `Vma`。匿名页通常 lazy；file-backed 映射登记 VMA，访问时再装 page cache；
4. 用户 fault 进入 [`os/src/trap/mod.rs`](../../os/src/trap/mod.rs)，再走 `handle_lazy_user_fault`、COW 或 file-backed fault；
5. `munmap`、COW 替换、truncate/exec 等路径先清 PTE/记录 generation，再做 TLB shootdown，旧页放入 deferred reclaim，之后才释放；
6. [`docs/page_cache_todo.md`](../page_cache_todo.md) 记录当前 page cache/mmap 的近似语义和限制，尤其是 sticky dirty、truncate 失效和 shootdown 第一阶段实现。

这里的关键不是“unsafe 块有没有 panic”，而是引用/切片是否覆盖了页表编辑和 deferred reclaim 的时间窗口。一个返回 `'static mut [u8]` 的 helper 若允许调用者跨 syscall、锁释放、阻塞或调度保存，审查级别要提高到页表/调度器级别。

## 9. FFI、汇编和 trap frame

### 9.1 C ABI 的三件事

要与 C/汇编交互，分别确认：

1. **调用约定**：`extern "C"`、寄存器、返回值、callee-saved/caller-saved、是否允许 unwind；
2. **数据布局**：`#[repr(C)]`、字段宽度、alignment、padding、数组长度、endian；
3. **指针合同**：null 是否允许、长度单位、所有权、回调期间是否有效、是否跨线程/异步保存。

只写 `extern "C"` 不会自动使结构体变成 C layout；只写 `#[repr(C)]` 也不会自动让函数调用 ABI 正确。

### 9.2 仓库中的 FFI/汇编映射

| 边界 | 位置 | 要对照的内容 |
|---|---|---|
| 内核入口清 BSS | [`os/src/main.rs`](../../os/src/main.rs)::`clear_bss` | linker 的 `sbss/ebss` 符号，`from_raw_parts_mut` 长度是地址差 |
| 用户入口 | [`user/src/lib.rs`](../../user/src/lib.rs)::`__user_start` | `user/src/linker*.ld` 的 `start_bss/end_bss`；初始栈 `argc/argv` 是用户地址，不是内核引用 |
| RISC-V trap frame | [`os/src/arch/riscv/trap.rs`](../../os/src/arch/riscv/trap.rs) 的 `#[repr(C)]` frame + [`trap.S`](../../os/src/arch/riscv/trap.S) | 字段顺序、数组寄存器编号、`TRAMPOLINE`、`a0/a1` 参数 |
| LoongArch trap frame | [`os/src/arch/loongarch64/trap.rs`](../../os/src/arch/loongarch64/trap.rs) + [`trap.S`](../../os/src/arch/loongarch64/trap.S) | 保存/恢复顺序、`PGDL`/DMW、`$a0/$a1` |
| 通用 trap context | [`os/src/trap/context.rs`](../../os/src/trap/context.rs)::`TrapContext` | 外层字段变化会影响保存区大小和 signal ABI；不要只改 Rust 不改汇编/断言 |
| 任务切换 | [`os/src/sched/context.rs`](../../os/src/sched/context.rs)、[`os/src/sched/switch.rs`](../../os/src/sched/switch.rs)、`os/src/arch/{riscv,loongarch64}/switch.S` | `TaskContext` 的 `#[repr(C)]` 与两架构 `switch.S` 字段偏移 |
| 直接启动 | [`bootloader/loongarch64-direct/src/main.rs`](../../bootloader/loongarch64-direct/src/main.rs) | `KERNEL_ENTRY`、`extern "C" fn(usize, usize) -> !`、DMW0/DMW1、UART volatile |
| fs sleep mutex | [`os/src/sync/fs_sleep_mutex.rs`](../../os/src/sync/fs_sleep_mutex.rs) | `extern "C"` 导出函数的指针 null/锁对象生命周期；由 `fs/src/sleep_mutex.rs` 调用 |

### 9.3 函数地址 `transmute` 的窄门

当前启动器和 ext4 适配代码中存在类似：

```rust
// 适用环境：仅当前 LoongArch direct boot 的 bootloader；不是通用模板。
let entry: extern "C" fn(usize, usize) -> ! =
    unsafe { core::mem::transmute(KERNEL_ENTRY) };
```

这个 `transmute` 只有在下列条件全部成立时才可能成立：固定地址确实是可执行映射；目标代码使用 `extern "C"` 约定；参数/返回类型和实际入口完全一致；入口不会 unwind；平台允许该整数到函数指针的转换。若能使用 linker 提供的 `extern "C" { fn entry(...); }` 符号，通常比整数 `transmute` 更容易审查。

`fs/src/ext4/mod.rs` 中把一个已知的函数地址转换成 `fn() -> usize` 也属于同一窄门：调用前必须确认该地址确实是正确 ABI、正确签名和可执行代码，不能把任意磁盘值/用户值变成函数指针。

### 9.4 `no_mangle` 和新版 unsafe attribute

仓库同时可见 `#[no_mangle]` 和 `#[unsafe(no_mangle)]`，原因是当前 nightly/代码路径对“改变链接符号名”的 unsafe attribute 采用了不同阶段的写法。现场不要机械替换全仓库；要看当前 rustc 的诊断和该文件已有风格。无论语法是哪一种，符号导出都必须核对：名称唯一、链接可见、调用 ABI 与汇编一致，且不能让安全 Rust 误以为一个任意地址是有效函数。

### 9.5 C ABI 结构体模板

```rust
// 适用环境：CosmOS user/kernel ABI；RISC-V/LoongArch 64-bit；需与同一 target 的 C ABI 对齐。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserStat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub _pad: u32,
    pub size: i64,
}

// 在 kernel 侧：不要直接 *user_ptr；先 read_pod_from_user/write_pod_to_user，
// 并确保该类型的所有字段/位模式都允许按位复制。
```

当前对应实现包括：

- [`os/src/fs/mod.rs`](../../os/src/fs/mod.rs)::`Stat`，`#[repr(C)]`、手动 `impl Pod`；
- [`os/src/syscall/fs.rs`](../../os/src/syscall/fs.rs)::`IoVec`、`PollFd`、`Statx`、`Flock`、BPF ABI 结构；
- [`os/src/syscall/net.rs`](../../os/src/syscall/net.rs)::`MsgHdr`、`IoVec`、socket 地址和 control message；
- [`user/src/net.rs`](../../user/src/net.rs) 与内核对应的用户侧 `repr(C)` 类型；
- [`os/src/trap/context.rs`](../../os/src/trap/context.rs) 与架构 trap frame。

用户侧 `syscall6` 在 [`user/src/syscall.rs`](../../user/src/syscall.rs) 中按架构放入寄存器：RISC-V 使用 `ecall`/`x10..x15`/`x17`，LoongArch 使用 `syscall 0`/`$a0..$a5`/`$a7`，并显式标记 LoongArch caller-saved temporaries。修改 syscall 参数或 asm clobber 时，必须同时看用户 wrapper、内核 `syscall()` 分发和具体 `sys_*` 签名。

## 10. 可直接套用的 CosmOS 模板

### 10.1 内核 copyin/copyout：先分页，再业务

```rust
// 适用环境：CosmOS 内核 no_std；用户态指针；RISC-V/LoongArch。
use alloc::vec::Vec;
use crate::mm::PageFaultAccess;
use crate::syscall::{
    read_bytes_from_user, translated_byte_buffer_with_access, write_bytes_to_user,
};
use crate::syscall::errno::ERRNO;

fn copyin_user(ptr: *const u8, len: usize) -> Result<Vec<u8>, ERRNO> {
    // 该 helper 会检查用户范围、每页权限，并处理当前实现支持的 lazy/COW/fault。
    read_bytes_from_user(ptr, len)
}

fn copyout_user(ptr: *mut u8, src: &[u8]) -> Result<(), ERRNO> {
    write_bytes_to_user(ptr, src)
}

fn use_user_buffer_for_file(
    ptr: *const u8,
    len: usize,
) -> Result<crate::mm::UserBuffer, ERRNO> {
    let pieces = translated_byte_buffer_with_access(
        ptr,
        len,
        PageFaultAccess::Read, // sys_write 的用户 buffer 是内核读
    )?;
    Ok(crate::mm::UserBuffer::new(pieces))
}
```

`sys_read` 的用户 buffer 对内核是写权限，使用 `PageFaultAccess::Write`；`sys_write` 是内核读用户 buffer，使用 `Read`。当前 [`os/src/syscall/fs.rs`](../../os/src/syscall/fs.rs) 的 `sys_read`/`sys_write`、`readv`/`writev` 就是这个方向。不要只看 C 原型里的 `const`：系统调用参数名和内核实际访问方向可能与用户 wrapper 的类型名不同。

### 10.2 用户 ABI 结构体：先 copy bytes，再解析

```rust
// 适用环境：CosmOS 内核 no_std；用户 ABI；RISC-V/LoongArch。
use crate::syscall::{read_pod_from_user, write_pod_to_user, Pod};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CountArg {
    count: u32,
    flags: u32,
}

impl Pod for CountArg {}

fn read_count(ptr: *const CountArg) -> Result<CountArg, crate::syscall::errno::ERRNO> {
    // 当前仓库 helper 已允许结构体跨多个用户页；它不是 translated_ref 的场景。
    read_pod_from_user(ptr)
}

fn write_count(
    ptr: *mut CountArg,
    value: &CountArg,
) -> Result<(), crate::syscall::errno::ERRNO> {
    write_pod_to_user(ptr, value)
}
```

如果结构体包含 enum、bool、引用、函数指针、带 niche 的 `Option` 或用户输入的裸指针，先把它们改成整数/字节字段解析，再做显式范围检查；不要为了复用 `Pod` 强行整体 `assume_init`。

### 10.3 磁盘/网络 packed 数据：`read_unaligned` 或字节解析

```rust
// 适用环境：no_std；fs/ext4/easy-fs/网络解析；RISC-V/LoongArch。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct PackedPair {
    tag: u8,
    value: u32,
}

fn read_value(p: *const PackedPair) -> u32 {
    // SAFETY: p 指向至少 size_of::<PackedPair>() 个仍存活的磁盘/协议字节；
    // addr_of! 不创建未对齐引用；read_unaligned 只解决对齐问题。
    unsafe { core::ptr::addr_of!((*p).value).read_unaligned() }
}
```

若协议是 little-endian，优先：`u32::from_le_bytes([b[0], b[1], b[2], b[3]])`。`read_unaligned` 不会自动做 endian 转换。

### 10.4 物理页/页表访问：保留页框合同

```rust
// 适用环境：CosmOS 内核 no_std；仅内核物理 direct-map；RISC-V/LoongArch。
use crate::mm::{phys_to_virt, PhysAddr, PhysPageNum};

fn zero_one_page(ppn: PhysPageNum) {
    let pa: PhysAddr = ppn.into();
    let va = phys_to_virt(pa.0);
    // SAFETY: ppn 来自当前内核持有的 live frame；平台保证该 RAM PA 有 direct map；
    // 该页当前没有其他 Rust 引用/设备 DMA 写入；长度和页边界为 4096。
    let page = unsafe { core::slice::from_raw_parts_mut(va as *mut u8, 0x1000) };
    page.fill(0);
}
```

不能把这段用于用户 VA、MMIO 或刚刚交给 DMA 但没有同步的 buffer。若是用户指针，改用 `translated_byte_buffer_with_access`；若是 MMIO，改用平台地址 + 正确宽度的 volatile wrapper。

### 10.5 FFI out-pointer：明确 null、长度和初始化

```rust
// 适用环境：std/no_std；extern "C"；RISC-V/LoongArch。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutValue {
    pub value: u64,
}

/// # Safety
/// out 为 null，或指向一个可写、正确对齐、足够大的 OutValue 存储；
/// 调用期间该存储不会被其他线程访问。
pub unsafe extern "C" fn fill_value(out: *mut OutValue) -> i32 {
    let Some(out) = (unsafe { out.as_mut() }) else {
        return -1;
    };
    out.value = 42;
    0
}
```

如果 out 指向未初始化存储，应在 C/Rust 合同里明确“调用者提供未初始化但可写存储”，实现侧使用 `out.write(OutValue { ... })`，不要先 `&mut *out` 再读取旧值。

## 11. CosmOS 文件路径与函数映射

| 主题 | 首查文件 | 关键函数/类型 | 现场用途 |
|---|---|---|---|
| 地址 newtype / PA↔kernel VA | `os/src/mm/address.rs` | `PhysAddr`、`VirtAddr`、`PhysPageNum`、`phys_to_virt`、`virt_to_phys` | 判断“这是 PA 还是 VA”，检查页内 offset/对齐 |
| 页表编码 | `os/src/mm/page_table.rs`、`os/src/arch/*/paging.rs` | `PageTableEntry`、`map`、`clear`、`replace`、`translate_va` | PPN、PTE flags、TLB 前后状态 |
| 用户指针翻译 | `os/src/mm/page_table.rs` | `translated_byte_buffer`、`translated_ref`、`translated_refmut`、`UserBuffer` | copyin/copyout、跨页判断 |
| 用户权限和 fault | `os/src/syscall/utils.rs` | `PageFaultAccess`、`pte_allows_user_access`、`prefault_user_pages` | `EFAULT` 是地址错、权限错还是 lazy/COW |
| VMA/mmap | `os/src/mm/memory_set.rs`、`os/src/syscall/mman.rs` | `Vma`、`MapPermission`、`mmap_anonymous`、`mmap_file`、`sys_mmap` | VMA 元数据、页故障和权限 |
| 进程页表切换/回收 | `os/src/task/process.rs`、`os/src/mm/tlb_shootdown.rs` | `mmap`、`munmap`、`handle_*_fault`、`DeferredUserReclaim` | PTE 清除后何时可以释放页 |
| trap frame | `os/src/trap/context.rs`、`os/src/arch/*/trap.rs`、`trap.S` | `TrapContext`、`return_to_user`、`__alltraps`/`__restore` | Rust/汇编字段偏移、返回用户寄存器 |
| syscall FFI | `user/src/syscall.rs`、`os/src/syscall/mod.rs` | `syscall`、`syscall6`、`syscall` 分发 | 寄存器参数和指针类型是否匹配 |
| syscall POD | `os/src/syscall/utils.rs`、`os/src/syscall/fs.rs`、`os/src/fs/mod.rs` | `Pod`、`read_pod_from_user`、`write_pod_to_user`、`Stat`/`IoVec` | ABI layout、跨页结构体复制 |
| MMIO | `os/src/platform/*/qemu_virt`、`os/src/drivers/*` | `mmio_phys_to_virt`、`Mmio<T>`、`read_volatile` | 正确 aperture、寄存器宽度和访问顺序 |
| 磁盘布局 | `fs/src/easyfs/layout.rs`、`fs/src/ext4_rs/src/ext4_defs/*` | `#[repr(C)]`/`packed`、`read_unaligned` | 磁盘 bytes 不要直接当对齐 Rust 对象 |
| block/page cache | `fs/src/block_cache.rs`、`os/src/fs/page_cache.rs` | `get_ref/get_mut`、direct contiguous run | allocation/对齐/页框 pin 和回收 |
| 启动 linker | `os/src/linker*.ld`、`user/src/linker*.ld` | `sbss/ebss`、`start_bss/end_bss`、`KERNEL_ENTRY` | 符号地址、物理/虚拟加载地址 |

## 12. 常见坑：现象 → 原因 → 检查 → 修复方向

| 现象 | 常见原因 | 检查步骤 | 修复方向 |
|---|---|---|---|
| syscall 返回 `-EFAULT` | 用户地址为 null/越界/溢出；页未映射；PTE 没有 `U` 或所需 R/W/X | 打印原始 ptr/len；用 `checked_add`；按页查看 `PageTable::translate` 和 flags；区分 Read/Write/Exec | 走 `translated_byte_buffer_with_access`；跨页用 `UserBuffer`/byte copy；不要直接 `translated_ref` |
| `sys_read` 能读小 buffer，跨页就 fault | 把连续用户 VA 当成一块内核 slice；第二页未 prefault 或物理不连续 | 查看 `ptr & (PAGE_SIZE-1)`、`len` 是否越过页尾；逐页打印 VPN/PPN/PTE | 分页翻译；每段分别复制；需要时先让 `prefault_user_pages` 处理 lazy/file/COW |
| `translated_ref::<T>` 读坏字段或 fault | `T` 跨页、地址不对齐或 PTE 权限不足 | 检查 `ptr % align_of::<T>()`、`offset + size_of::<T>()` 是否落在同页；查看 helper 注释 | 跨页改 `read_pod_from_user`/byte copy；不对齐改 `read_unaligned` 或逐字节解析 |
| 编译报 `E0793: reference to packed field is unaligned` | `repr(packed)` 字段被隐式取引用 | 搜索 `&packed.field`、格式化宏、方法调用；检查字段类型 alignment | `addr_of!((*p).field).read_unaligned()`，或复制到对齐的局部值后访问 |
| 编译报 unsafe operation 或运行时随机崩 | `unsafe fn` 内仍未局部包裹 unsafe；Safety 合同缺失；release 优化暴露 alias UB | `cargo check`/`cargo clippy`；逐个 unsafe block 写不变量；比较 debug/release | 缩小 unsafe 边界；用 slice/owned buffer/lock；不以“没崩”证明安全 |
| 读出结构体后 enum/bool 行为异常 | 任意用户/磁盘 bytes 直接 `assume_init::<T>`，位模式无效 | 检查 `T` 字段是否有 enum/bool/reference/Option niche；检查 Pod impl | 先读整数/字节并验证，再构造合法 Rust 值；不要给任意类型加 `Pod` |
| `read` 后 double free/堆损坏 | 对非 `Copy` 类型用 `ptr::read` 复制后又使用/Drop 原对象 | 查 `T` 是否含 `Vec/String/Box/&mut`；看源对象和返回值是否都离开作用域 | 只对 plain `Copy` 使用；移动用 `ptr::read` + 明确忘记/重建所有权，或优先安全 API |
| RISC-V `PROT_WRITE` 页持续 StoreFault | PTE 只写 W 未写 R；RISC-V leaf encoding 不允许 W=1,R=0 | 打印 VMA permission 与 PTE flags；查看 `prot_to_map_perm` | 让写页包含 R|W（仓库当前 helper 已这么做）；不要手写 PTE bit |
| LoongArch 未对齐用户访问进入 ADEM/地址错误 | 用户指令对非自然对齐地址访问；内核把它当普通 fault | 看 `BADV/BADI/ERA`，查 `os/src/arch/loongarch64/trap.rs` 的 unaligned emulation | 使用已实现的 `emulate_user_unaligned`；内核 copyin 走 byte buffer，不直接解引用 |
| `mmap` 成功但第一次访问 SIGSEGV/SIGBUS | VMA 登记成功但 fault 类型/权限/EOF 不匹配；file-backed 页超 EOF | 打印 VMA kind、fault access、file page index、EOF；查看 `handle_file_page_fault` | 区分匿名 lazy、MAP_SHARED、MAP_PRIVATE COW、尾页补零和 EOF 后 SIGBUS |
| `munmap`/COW 后偶发访问旧数据或 kernel fault | 清 PTE 后没有对所有可能运行该地址空间的 hart 做 TLB shootdown，或过早释放页 | 查看 `tlb_generation`、active/loaded hart mask、deferred reclaim；在 fault 点打印 token/ASID | 先更新 PTE，再 fence/shootdown，最后释放旧页；复用现有 `DeferredUserReclaim` |
| MMIO 写入没有效果，或读值像被优化掉 | 普通 load/store 代替 volatile；错误 physical→MMIO VA；寄存器宽度/offset 错 | 对照 `platform/*/board.rs`、`MMIO` 表、设备手册；确认 RISC-V 高 aperture/LA DMW0；查 `read_volatile` 宽度 | 封装 MMIO；使用正确宽度 volatile；并发顺序另用 atomic/fence/硬件 barrier |
| VirtIO 探测 panic/读到全零 | header 地址不是当前平台 MMIO VA、slot stride 错、`NonNull` 只判空未验证硬件 | 打印 slot/base；查 `VIRTIO_MMIO_BASE/STRIDE/SLOTS`；检查 magic/version/device id | 从平台常量计算地址；先验证设备映射，再交给 `MmioTransport::new` |
| FFI 参数错位、trap return 后寄存器乱 | Rust struct 未 `repr(C)`、字段新增未同步汇编、`extern "C"` 签名/寄存器约定不匹配 | `size_of/align_of/offset_of`；查看 `trap.S`/`switch.S`；反汇编和 GDB 检查寄存器 | 固定 layout；加编译期断言；同步 Rust/汇编/用户 wrapper；不要用默认 Rust layout |
| 启动后 BSS/全局变量损坏 | linker symbol 差、把物理地址当虚拟地址、`ebss - sbss` 溢出 | 查 `os/src/linker*.ld`、入口 assembly、`readelf -S/-s`；确认 target | 保持 `clear_bss` 的符号合同；RISC-V high-half/LoongArch DMW 路径分开处理 |
| 文件系统读取偶发错字节 | block cache 把任意 offset cast 成对齐 `&T`；packed/on-disk endian 未处理 | 检查 `get_ref/get_mut` 调用 offset；看 `read_unaligned`/`from_le_bytes` | 原始 bytes + `read_unaligned`/显式 endian；验证 `size_of::<T>()` 和 block 边界 |
| 只在 SMP/高负载出现数据错 | `&mut` slice/裸指针跨锁保存，或 volatile 当原子同步 | 查引用存活范围、锁、DMA、atomic ordering、是否跨调度/阻塞 | 缩小借用；用锁/原子；DMA 增加 ownership/cache/fence 合同；volatile 只保设备访问 |

## 13. 如何把 unsafe 边界缩到最小

### 13.1 “验证在外，解引用在内”

不推荐：

```rust
// 适用环境：CosmOS 内核；反例。
fn parse_user_bad(p: *const u32) -> u32 {
    unsafe { *p }
}
```

推荐把不可验证条件集中到一个 helper，调用方只获得 safe value：

```rust
// 适用环境：CosmOS 内核 no_std；用户指针；RISC-V/LoongArch。
fn parse_user_good(p: *const u32) -> Result<u32, crate::syscall::errno::ERRNO> {
    crate::syscall::read_pod_from_user(p)
}
```

如果 helper 的合同不能覆盖某个调用者（例如指定进程而不是 current process、需要 Exec 权限、需要锁住页框），就增加参数/guard，而不是在调用点偷偷 `unsafe`。

### 13.2 把硬件/地址空间资源放进类型或 guard

一个合格的 kernel wrapper 至少应表达其中一部分：

- `PhysPageNum`/`VirtPageNum`：不把 PA/VA 当同一整数类型到处传；
- `AddressSpaceRoot`：root frame 的强引用与 token 一起存活；
- `UserBuffer`：明确这是分页翻译后的 kernel view；
- `Mmio<T>`：集中 volatile 和寄存器宽度；
- `FrameTracker`/`PrivatePage`：页框拥有权和 COW/page-cache 引用；
- `SpinNoIrqLock`/atomic：并发和中断上下文边界。

当前 [`os/src/mm/page_table.rs`](../../os/src/mm/page_table.rs) 的 `AddressSpaceRoot`、[`os/src/mm/memory_set.rs`](../../os/src/mm/memory_set.rs) 的 deferred reclaim、[`os/src/drivers/chardev/ns16550a.rs`](../../os/src/drivers/chardev/ns16550a.rs) 的 `Mmio<T>` 都是“把不变量集中在少数边界”的方向。

### 13.3 每个 unsafe 块都写可以被别人复核的 Safety 注释

差的注释：

```rust
// 适用环境：CosmOS 内核；反例，仅用于审查说明。
// SAFETY: should be safe.
unsafe { *ptr }
```

好的注释应写来源、范围、对齐、初始化、别名、生命周期和硬件语义：

```rust
// 适用环境：CosmOS 内核；示意如何写可复核的 Safety 注释。
// SAFETY:
// - ptr 来自当前进程的 PageTable::translate，VPN 已检查 < USER_SPACE_END；
// - PTE 有 U|R，且 size_of::<u32>() 不跨页；
// - 当前调用持有 process lock，页框在返回的局部引用存活期间不会被 munmap/COW 替换；
// - ptr % align_of::<u32>() == 0；用户页内容已初始化为合法 u32 位模式。
let value = unsafe { ptr.read() };
```

若其中任一条只能靠“通常如此”而不是代码/锁/页表证明，就不能把这段包装成 safe API。

### 13.4 先做静态检查，再做运行时最小验证

现场断网时可按这个顺序：

1. `rg -n "unsafe|from_raw_parts|transmute|read_volatile|write_volatile|as \*const|as \*mut"` 锁定边界；
2. 看 `git diff` 是否有人改了同一文件/同一结构体；
3. 运行 `cargo fmt --check`/`cargo check`（会依赖本地 cache；target 构建会写构建产物）；
4. 对布局加 `size_of/align_of/offset_of` 编译期断言；
5. QEMU 中用最小用户程序触发：跨页 copyin、非对齐字节解析、`mmap` 首次读/写、`munmap` 后再访问、MMIO 探测；
6. 只有在怀疑 alias/lifetime UB 时再考虑 Miri/模型检查；Miri 通常不能直接替代 `riscv64gc-unknown-none-elf` + QEMU 的页表/MMIO 语义。

## 14. 现场命令模板

以下命令是当前仓库 Makefile/Cargo 配置下的模板；是否能执行取决于现场是否已准备 target、依赖缓存、QEMU、交叉工具链和镜像。`cargo build`/`make kernel-*` 会写 `target/`、stamp 或镜像，确认不会覆盖队友正在使用的构建目录后再运行。

### 14.1 确认工具链与 target（只读）

```sh
# 适用环境：宿主机 shell；不依赖 CosmOS 运行时。
rustup show active-toolchain
rustc +nightly-2025-01-18 -Vv
rustup target list --installed
qemu-system-riscv64 --version
qemu-system-loongarch64 --version
```

### 14.2 只做格式/配置/命令展开检查

```sh
# 适用环境：仓库根目录；只读检查为主。
cargo fmt --manifest-path os/Cargo.toml -- --check
cargo fmt --manifest-path user/Cargo.toml -- --check
cargo fmt --manifest-path fs/Cargo.toml -- --check
make -n -C os kernel ARCH=riscv64 OFFLINE=1
make -n -C os kernel ARCH=loongarch64 OFFLINE=1
git diff --check -- docs/final_prepare/05-rust-unsafe-pointers-layout.md
```

`make -n` 不执行 recipe；`cargo fmt --check` 不应改文件。若工具版本不支持 `--manifest-path` 的该组合，进入 crate 目录执行 `cargo fmt -- --check`。

### 14.3 当前 Makefile 的内核构建入口

```sh
# 适用环境：仓库根目录；会写构建产物；RISC-V。
make cargo-config
make -C os kernel ARCH=riscv64 OFFLINE=1

# 适用环境：仓库根目录；会写构建产物；LoongArch。
make -C os kernel ARCH=loongarch64 OFFLINE=1

# 适用环境：根 Makefile 的架构目标；可能进一步依赖用户程序/镜像。
make BUILD_ARCH=rv kernel-rv
make BUILD_ARCH=la kernel-la
```

`os/Makefile` 的默认 `ARCH`、`QEMU`、linker 和 `TARGET` 会按 `riscv64`/`loongarch64` 分支选择。LoongArch 运行/打包还会经过 `bootloader/loongarch64-direct`；不要用 RISC-V 的 `objcopy`/GDB/QEMU 参数替代。

### 14.4 布局和符号检查

```sh
# 适用环境：已有 ELF；只读查看布局/符号。
rustc +nightly-2025-01-18 --print cfg --target riscv64gc-unknown-none-elf
rustc +nightly-2025-01-18 --print cfg --target loongarch64-unknown-none
readelf -S os/target/riscv64gc-unknown-none-elf/release/os
readelf -s os/target/riscv64gc-unknown-none-elf/release/os | rg 'sbss|ebss|stext|etext|trampoline'
```

若 ELF 路径不存在，先执行对应构建；不要把 `file` 显示的机器架构、target triple、QEMU 选项和 `cfg(target_arch)` 混为一件事。

## 15. 最后现场 checklist

### 写/改一个 unsafe 边界前

- [ ] 我能说出指针来源：Rust allocation、页表翻译、PA direct map、MMIO、linker symbol 还是 FFI 参数。
- [ ] 我对所有地址/长度做了 checked arithmetic，没有 `start + len` 溢出。
- [ ] 我知道长度单位是 bytes 还是 `T` 元素数；`copy_nonoverlapping` 的 `count` 没写错。
- [ ] 我检查了 `null`、对齐、`size_of::<T>()`、跨页和 `USER_SPACE_END`。
- [ ] 读 `T` 前证明初始化和有效位模式；写入未初始化存储时没有先产生无效引用。
- [ ] 没有用 `read` 复制非 `Copy` 所有权值；没有把 `read_unaligned` 当作 endian/validity 修复。
- [ ] 没有把两个不同页/不同 allocation 伪装成一个 slice；用户跨页使用 `UserBuffer`/byte copy。
- [ ] `&mut` 的独占性覆盖整个借用期间；没有跨锁、阻塞、调度、TLB shootdown 或 DMA 生命周期保存它。
- [ ] 若是 MMIO，地址 aperture、寄存器宽度、volatile 和硬件 barrier 都对；没有把 volatile 当原子同步。
- [ ] 若是 FFI/汇编，`extern "C"`、`repr(C)`、字段 offset、栈/寄存器和 symbol 都与另一侧核对。

### 改页表/mmap/fault/reclaim 前

- [ ] 区分用户 VA、内核 direct-map VA、PA 和 MMIO VA。
- [ ] RISC-V/LoongArch 的 PTE 编码通过 `hal`，没有复用另一架构的 bit mask。
- [ ] 每个用户访问检查 `U` 和本次 `Read/Write/Exec` 所需权限。
- [ ] lazy/COW/file-backed fault 的 VMA kind、EOF 和 page cache ownership 对得上。
- [ ] PTE 清除/替换后先完成本地和远端 hart 的 TLB 处理，再释放旧页/解除 pin。
- [ ] 只在当前 lock/guard 保证 root、页框、VMA、引用存活的范围内使用翻译后的 slice。
- [ ] `mmap` 长度、固定地址、offset、权限和 `USER_SPACE_END` 的检查在 syscall 边界完成。

### 出现 fault 或错误时

- [ ] 记录 `pc`、fault VA、访问类型、当前 token/root、VPN/PPN、PTE bits/flags、VMA kind 和页内 offset。
- [ ] 先判断是地址/权限/对齐/初始化/别名/生命周期/ABI 哪一类，不要一看到 `unsafe` 就只加 `volatile`。
- [ ] RISC-V 看 `scause/stval/satp`；LoongArch 看 `ESTAT/BADV/BADI/ERA/PGDL`。
- [ ] 检查 `git diff`：当前仓库可能有其他 worker 的未提交改动；不要回滚或覆盖不属于自己的文件。
- [ ] 只修改负责文件（本资料包任务的目标是 `docs/final_prepare/05-rust-unsafe-pointers-layout.md`）。

## 16. 离线官方资料名

正文已按当前仓库实现展开；下面只列稳定的官方资料名和 URL，现场若资料包包含 HTML/缓存可直接检索：

- [The Rust Reference — Behavior considered undefined](https://doc.rust-lang.org/reference/behavior-considered-undefined.html)：对齐、悬垂/范围、别名、data race、invalid value、ABI/asm 等。
- [The Rust Reference — Type layout / Representations](https://doc.rust-lang.org/reference/type-layout.html)：size、alignment、默认 Rust layout、`repr(C)`、`repr(transparent)`、`packed`。
- [`core::slice::from_raw_parts` / `from_raw_parts_mut`](https://doc.rust-lang.org/core/slice/fn.from_raw_parts.html)：slice 的 allocation、非空、对齐、长度和独占合同。
- [`core::ptr::read` / `write` / `read_unaligned` / `write_unaligned`](https://doc.rust-lang.org/core/ptr/index.html)：裸指针读写和对齐/所有权语义。
- [`core::ptr::read_volatile` / `write_volatile`](https://doc.rust-lang.org/core/ptr/index.html)：volatile 访问，不等于原子或 fence。
- [`core::mem::MaybeUninit`](https://doc.rust-lang.org/core/mem/union.MaybeUninit.html)：未初始化存储、out-pointer、`assume_init` 前提。
- [`core::ptr::NonNull`](https://doc.rust-lang.org/core/ptr/struct.NonNull.html)：非空裸指针、niche、variance 和 dangling 语义。
- [The Rustonomicon — FFI](https://doc.rust-lang.org/nomicon/ffi.html)：C ABI、外部函数和安全 wrapper。

官方文档可能随 nightly 对 API 页面版本更新；现场判断以本仓库 `nightly-2025-01-18` 能编译的 API 和源码注释为准，尤其不要把更新版示例中的新 API 直接用于旧缓存环境。
