# CosmOS 现场速查：Option、Result、迭代器与闭包

> 面向：断网、不能调用 Codex、需要在几分钟内定位 Rust OS 代码问题的参赛者。
>
> 资料基线：本仓库当前工作树；工具链文件固定为 nightly-2025-01-18，本机实际探测到 rustc 1.86.0-nightly。源码行号是本次整理时的快照；若现场源码已变动，以现场源码、Cargo.toml 和 Makefile 为准。

## 0. 先记住这几句话

1. Option<T> 表示“有值/没有值”，Result<T, E> 表示“成功/失败且失败有原因”。
2. map 改变容器里的值；and_then 的闭包本身返回同类容器，并负责把一层嵌套压平。
3. ? 只做“成功取值、失败提前返回”；它不会记录日志、不会恢复资源、也不会把任意错误自动变成目标错误。
4. 迭代器适配器大多是惰性的；没有 for、next、find、collect、fold 等消费者，闭包不会执行。
5. iter() 借用，iter_mut() 可变借用，into_iter() 通常消耗所有权。现场遇到 moved/borrowed 错误，先查这里。
6. Option 适合“正常的缺席”；文件系统、设备、页表等必须区分 ENOENT、ENOMEM、EIO 等原因时，应优先使用 Result。
7. CosmOS 的内核、fs crate 和用户程序都是 no_std 路线；Vec/String 需要 alloc，不能照搬依赖 std::fs、std::io 或线程的示例。

## 1. CosmOS 代码边界与构建入口

### 1.1 当前仓库的 Rust 边界

| 代码区域 | edition / 属性 | 常用类型来源 | 主要目标与入口 |
| --- | --- | --- | --- |
| os/ | edition 2021，#![no_std]、#![no_main]，extern crate alloc | core + alloc | make -C os kernel ARCH=riscv64 或 ARCH=loongarch64 |
| fs/ | edition 2018，#![no_std]，extern crate alloc | core + alloc | 由 os 的 path dependency 引入；fs/Cargo.toml 默认 feature 当前含 io_perf_counters |
| user/ | library 与 bin 均为 no_std 方向；用户库声明 extern crate alloc | core + alloc | make -C user build ARCH=riscv64 或 ARCH=loongarch64 |
| fs-fuse/、fs/src/ext4_rs/examples/ | 宿主侧工具/示例，源码中可见 std | 依各自 Cargo.toml | 不要把 std::fs 等 API 直接移到内核或用户程序 |

配置要点：

- 仓库根目录没有 workspace Cargo.toml；不要在根目录盲目执行 cargo check，应带 --manifest-path，或使用现成 Makefile。
- rust-toolchain.toml 指定 nightly-2025-01-18，并显式列出 RISC-V target；LoongArch target 需要现场环境已经安装。
- os/cargo-config/config.toml 和 user/cargo-config/config.toml 为两个 crate 设置默认 target、链接脚本和 target feature。根 Makefile 的 cargo-config 会把它们复制到隐藏的 .cargo/ 目录。
- os/Makefile 当前把内核默认文件系统设为 MAIN_FS := ext4；默认 EXTRA_FEATURES 还包含若干性能/缓存 feature。不要只凭某次 cargo check 的默认 feature 判断最终镜像行为。
- RISC-V target 是 riscv64gc-unknown-none-elf；LoongArch target 是 loongarch64-unknown-none。Option/Result/迭代器本身与架构无关，但链接脚本、cfg(target_arch = ...) 和设备错误路径与架构有关。

### 1.2 离线前先准备、现场再验证

> 适用环境：宿主机；仓库根目录；只读检查，不构建。

~~~sh
rustup show active-toolchain
rustc --version
rustup target list --installed
rg -n "Option|Result|and_then|map_err|collect|flatten|FnMut|FnOnce" os/src fs/src user/src
~~~

> 适用环境：宿主机；仓库根目录；会生成/更新 os/.cargo、user/.cargo 配置，不会改动 Rust 源码。

~~~sh
make cargo-config
~~~

> 适用环境：宿主机；RISC-V 用户程序；离线依赖缓存、riscv64gc-unknown-none-elf target 和 nightly-2025-01-18 已存在。会写入 user/target/。

~~~sh
cargo build --offline \
  --manifest-path user/Cargo.toml \
  --target riscv64gc-unknown-none-elf \
  --release
~~~

> 适用环境：宿主机；LoongArch 用户程序；LoongArch target 与依赖缓存已存在。会写入 user/target/。

~~~sh
cargo build --offline \
  --manifest-path user/Cargo.toml \
  --target loongarch64-unknown-none \
  --release
~~~

> 适用环境：宿主机；RISC-V 内核；使用仓库当前 ext4、平台和缓存相关 feature；会写入 os/target/。若现场 Makefile 变动，优先照现场 os/Makefile 调整 feature。

~~~sh
cargo build --offline \
  --manifest-path os/Cargo.toml \
  --target riscv64gc-unknown-none-elf \
  --release \
  --no-default-features \
  --features 'ext4,platform-qemu-virt,net_perf_counters,mm_perf_counters,legacy-vdb-names,trap_context_cache,process_identity_cache,return_work_cache,current_task_cache'
~~~

> 适用环境：宿主机；使用仓库 Makefile 的完整单架构路径；离线时依赖、子模块、工具链和镜像缓存必须已准备。OFFLINE=1 只会跳过 os/Makefile 的 rustup/组件安装分支，不会替 Cargo 补齐缺失的 registry 包。

~~~sh
make -C user build ARCH=riscv64
make -C os build ARCH=riscv64 OFFLINE=1
make all BUILD_ARCH=rv SMP=1 KEEP_SDCARD=1
~~~

### 1.3 选型总表

| 问题 | 首选类型 | 现场写法 | 不要做什么 |
| --- | --- | --- | --- |
| 查找目录项，找不到是正常分支 | Option<T> | dir.find(name)、find(...) | 为了方便在 None 上 unwrap() |
| 分配页框/缓冲区，耗尽要给调用方 errno | Result<T, E> | bitmap.alloc(...).ok_or(FS_ERRNO::ENOMEM)? | 让 None 丢失“耗尽还是设备坏”的原因 |
| 读取 BPB/网络包，格式坏或 I/O 失败 | Result<T, E> | 边界检查后 ok_or(EINVAL)? | 只返回空结构或默认值继续解析 |
| 可选元数据，如 uid、ctime、FDT 指针 | Option<T> | node.uid().unwrap_or(default) | 把“字段缺失”伪装成真实 uid |
| 多步操作，每一步可能失败 | Result<T, E> + ? | let x = step()? | 层层复制成功分支，或中途 unwrap |
| 一批项全部解析，任一项失败就停止 | collect::<Result<Vec<_>, _>>() | 见第 5 节 | flatten() 把错误静默丢掉 |
| 一批项只保留成功项，错误确实无关 | filter_map(x => x.ok()) | 明确写出丢错意图 | 把它误当成错误处理 |

## 2. Option<T>：缺席不是错误码

### 2.1 形状与所有权

Option<T> 只有两个变体：Some(value) 与 None。它不携带错误原因，None 可能代表“没找到”“尚未准备”“资源耗尽”“缓存 miss”或“输入非法”，这些语义需要由 API 文档或更强的枚举区分。

| 写法 | 结果 | 是否消耗原值 |
| --- | --- | --- |
| match value { Some(x) => ..., None => ... } | 完整分支控制 | 是（除非 value 是引用） |
| value.as_ref() / as_mut() | Option<&T> / Option<&mut T> | 否 |
| value.map(f) | Option<U> | 是；闭包只需处理一个 T |
| value.and_then(f) | Option<U> | 是；f: T -> Option<U> |
| value.take() | 原变量变 None，返回旧值 | 不复制，转移值 |
| value.replace(new) | 存入新值，返回旧 Option<T> | 转移新值 |

现场最常用的组合顺序通常是：

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。以下是概念流程，不是可执行代码。

~~~text
取出/借用 → map 做局部转换 → filter 做条件筛选 → and_then 继续可能失败的步骤 → unwrap_or/ok_or 收口
~~~

### 2.2 Option 组合方法速查

| 方法 | 输入闭包/参数 | 典型用途 | 易错点 |
| --- | --- | --- | --- |
| map | FnOnce(T) -> U | Some(inode) 转 Some(ino) | 闭包返回普通值；返回 Option 会嵌套 |
| and_then | FnOnce(T) -> Option<U> | 查找链、解析链 | 用来压平 Option<Option<U>> |
| filter | FnOnce(&T) -> bool | 值存在且满足条件才保留 | 谓词拿到借用，不要移动 T |
| or | 另一个 Option<T> | 备选值 | 右侧表达式先执行，可能有无谓分配/锁 |
| or_else | FnOnce() -> Option<T> | 懒惰备选、fallback | 适合第二次资源探测 |
| and | 另一个 Option<U> | 两者都有才保留右值 | 右侧先执行；有副作用时慎用 |
| zip | Option<U> | 两个可选值同时存在才组成元组 | 任一 None 都是 None |
| xor | 另一个 Option<T> | 恰好一个存在 | 两者都 Some 也得到 None |
| flatten | Option<Option<T>> | 压平一层 | 不能自动解析 Result |
| as_ref / as_mut | 无 | 不消费地检查/变更内部值 | 后续闭包收到引用 |
| copied / cloned | 无 | 从 Option<&T> 复制/克隆 | copied 要求 T: Copy |
| unwrap_or | 默认值 | 便宜、无副作用默认值 | 默认表达式立即求值 |
| unwrap_or_else | FnOnce() -> T | 昂贵或依赖上下文的默认值 | 只在 None 时执行 |
| map_or | 默认值 + FnOnce(T)->U | 两分支都产出 U | 默认值立即求值 |
| map_or_else | 默认闭包 + 成功闭包 | 两个分支都可能昂贵 | 两个闭包返回类型相同 |
| ok_or | 错误值 | Option<T> 转 Result<T,E> | 错误值立即求值 |
| ok_or_else | FnOnce() -> E | 带上下文构造错误 | 只在 None 时构造 |
| inspect | FnOnce(&T) | 临时日志/计数且保留原 Option | 不要把业务转换塞进 inspect |
| get_or_insert_with | FnOnce() -> T | 懒惰初始化可选资源 | 会借用并可能改变原 Option |

### 2.3 Option 的现场模板

> 适用环境：core/no_std；内核态或用户态；RISC-V/LoongArch 均可。

~~~rust
fn lookup_ino(entries: &[(u64, &str)], wanted: &str) -> Option<u64> {
    entries
        .iter()
        .find(|(_, name)| *name == wanted)
        .map(|(ino, _)| *ino)
}

fn first_usable(entries: &[Option<u32>]) -> Option<u32> {
    entries
        .iter()
        .copied()
        .flatten()
        .find(|candidate| *candidate != 0)
}
~~~

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。as_ref 让 String 或 Arc 不被无意移动。

~~~rust
use alloc::string::String;

fn display_name<'a>(name: &'a Option<String>) -> &'a str {
    name.as_ref().map(String::as_str).unwrap_or("<unnamed>")
}

fn replace_name(name: &mut Option<String>, new_name: String) -> Option<String> {
    name.replace(new_name)
}
~~~

> 适用环境：core/no_std；需要把“找不到”转换成当前仓库风格的 errno；内核态/fs crate；两种架构均可。

~~~rust
use fs::errno::FS_ERRNO;

fn require_child(
    dir: &fs::Inode,
    name: &str,
) -> Result<alloc::sync::Arc<fs::Inode>, FS_ERRNO> {
    dir.find(name).ok_or(FS_ERRNO::ENOENT)
}
~~~

上面是“缺失本身就是 ENOENT”的情况。如果 None 可能表示多个原因，不能随便写 ok_or(ENOENT)；应让底层直接返回 Result，或先定义更具体的枚举。

> 适用环境：core/no_std；资源分配/设备初始化；内核态；RISC-V/LoongArch 均可。

~~~rust
fn alloc_block_or_errno(
    bitmap: &fs::easyfs::bitmap::Bitmap,
    device: &alloc::sync::Arc<dyn fs::BlockDevice>,
) -> Result<usize, fs::errno::FS_ERRNO> {
    bitmap
        .alloc(device)
        .ok_or(fs::errno::FS_ERRNO::ENOSPC)
}
~~~

> 适用环境：core/no_std；解析/资源链；内核态或用户态；RISC-V/LoongArch 均可。

~~~rust
fn parse_port(arg: Option<&str>, default: u16) -> u16 {
    arg.and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(default)
}

fn lookup_then_check<T>(value: Option<T>, valid: impl FnOnce(&T) -> bool) -> Option<T> {
    value.filter(valid)
}
~~~

filter 的闭包接收 &T，所以这里不会把 T 移出 Option。若下一步本身也可能失败，则用 and_then：

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn parse_nonzero(s: &str) -> Option<usize> {
    s.parse::<usize>().ok().filter(|value| *value != 0)
}

fn next_name<'a>(dir: &'a [(&'a str, bool)], name: &str) -> Option<&'a str> {
    dir.iter()
        .find(|(entry_name, is_dir)| *entry_name == name && *is_dir)
        .map(|(entry_name, _)| *entry_name)
}
~~~

### 2.4 Option 嵌套：and_then、flatten、transpose

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。以下是类型流转示意，不是可执行代码。

~~~text
Option<T> --map(|x| Option<U>)--> Option<Option<U>>
Option<T> --and_then(|x| Option<U>)--> Option<U>
~~~

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn nested_example(input: Option<&str>) -> Option<usize> {
    input
        .map(|s| s.parse::<usize>().ok())
        .flatten()
}

fn idiomatic_example(input: Option<&str>) -> Option<usize> {
    input.and_then(|s| s.parse::<usize>().ok())
}
~~~

Option<Result<T, E>> 和 Result<Option<T>, E> 表达的语义不同。需要“字段不存在不是错误，但字段存在且格式坏是错误”时，用 transpose：

> 适用环境：core/no_std；解析器；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    BadNumber,
}

fn parse_optional_number(raw: Option<&str>) -> Result<Option<u32>, ParseError> {
    raw.map(|s| s.parse::<u32>().map_err(|_| ParseError::BadNumber))
        .transpose()
}
~~~

记忆：

- Some("12") → Ok(Some(12))；
- None → Ok(None)；
- Some("bad") → Err(BadNumber)。

### 2.5 CosmOS 中的 Option 映射

| 源码位置 | 真实接口/模式 | 现场含义 |
| --- | --- | --- |
| [fs/src/easyfs/bitmap.rs](../../fs/src/easyfs/bitmap.rs:32) | Bitmap::alloc(...) -> Option<usize>；内部 iter().enumerate().find(...).map(...) | 从位图找第一个空闲 bit；没有空闲 bit 是 None |
| [fs/src/fat32/dir.rs](../../fs/src/fat32/dir.rs:123) | sfn_from_str(...) -> Option<SfnCreateInfo>；encode_component 用 ? 传播 None | 名字不能表示为 FAT 8.3 时正常失败，调用者可退回 LFN |
| [fs/src/fat32/dir.rs](../../fs/src/fat32/dir.rs:298) | assemble_lfn(...) -> Option<String>；Vec<Option<&LfnPart>> 后对 slot 使用 ? | LFN 顺序、校验和或槽位不完整时放弃组装 |
| [fs/src/fat32/inode.rs](../../fs/src/fat32/inode.rs:288) | find_in_dir 用 into_iter().find；find_by_name_in_dir 用 as_ref().map(...).unwrap_or(false) | 查目录项是可缺席查询，不应自动当作 I/O 错误 |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:290) | VfsNode::find/create/mkdir 的旧接口返回 Option | 后端旧 API 不能保留具体错误 |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:593) | Inode::create、mkdir 用 .ok() 包装 Result | 兼容旧调用者，但会有意丢失 errno |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:756) | mode/uid/gid/atime/... -> Option<_> | 元数据可能在某后端不存在；调用者应给默认值或显式分支 |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:917) | page cache 用 as_ref().and_then(...downcast().ok()) | 类型擦除后的缓存可能不存在或类型不匹配 |
| [os/src/bootinfo.rs](../../os/src/bootinfo.rs:103) | fdt_blob() -> Option<(usize, usize)>，用 then_some | FDT 地址/大小都非零时才暴露 |
| [os/src/drivers/net/virtio_net.rs](../../os/src/drivers/net/virtio_net.rs:106) | try_recv(...) -> Option<usize>，poll_receive()?、槽位 get_mut()?.take()? | 非阻塞接收没有包时是正常 None；设备 API 错误在此实现中也会收敛为 None |
| [fs/src/dentry_cache.rs](../../fs/src/dentry_cache.rs:48) | 自定义 DentryLookup::{Positive, Negative, Miss} | 当 None 无法区分“负缓存命中”和“缓存未命中”时，使用领域枚举 |

特别注意最后两项：try_recv 的 Option 语义是当前 API 的设计；如果竞赛题要求诊断设备错误，不要照抄这个丢错边界，应改为 Result<Option<usize>, Error> 或至少增加日志/状态码。

## 3. Result<T, E>：让错误沿调用栈保留下来

### 3.1 Result 组合方法速查

| 方法 | 输入闭包/参数 | 典型用途 | 易错点 |
| --- | --- | --- | --- |
| map | FnOnce(T) -> U | 成功值转换，错误原样保留 | 闭包不能改变错误类型 |
| map_err | FnOnce(E) -> F | errno/底层错误转换 | 只改错误，不改成功值 |
| and_then | FnOnce(T) -> Result<U, E> | 多步可能失败操作 | 闭包的错误类型必须与当前 E 相同；不同类型先 map_err |
| or_else | FnOnce(E) -> Result<T, F> | 按错误 fallback、重试或转错误 | 不是“忽略错误” |
| ok | 无 | Result<T,E> -> Option<T> | 无声丢失 E |
| err | 无 | 只取错误 | Ok 会变 None |
| as_ref / as_mut | 无 | 不消费地查看成功/错误引用 | 类型变为 Result<&T, &E> 等 |
| unwrap_or | 默认值 | 失败时便宜默认 | 错误值已经被丢弃 |
| unwrap_or_else | FnOnce(E) -> T | 用错误构造 fallback | 只在 Err 时执行 |
| inspect | FnOnce(&T) | 观察成功值 | 不适合承担业务副作用 |
| inspect_err | FnOnce(&E) | 记录错误后继续传播 | 仍然是原 Err |
| transpose | Result<Option<T>,E> | 调整可选字段 + 可失败解析的层次 | 先确认需要的形状 |
| flatten | Result<Result<T,E>,E> | 当前 pinned nightly 需要 result_flattening feature | 不把它当现场稳定模板；用 and_then(inner => inner) |

### 3.2 基础写法与 ?

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。错误类型可换成 FS_ERRNO、设备错误或题目自定义错误。

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Error {
    Missing,
    BadFormat,
}

fn read_value(input: Option<&str>) -> Result<u32, Error> {
    let text = input.ok_or(Error::Missing)?;
    text.parse::<u32>().map_err(|_| Error::BadFormat)
}

fn read_two(input: Option<&str>) -> Result<u32, Error> {
    let first = read_value(input)?;
    let doubled = first.checked_mul(2).ok_or(Error::BadFormat)?;
    Ok(doubled)
}
~~~

这段代码等价于“遇到 None/Err 立即从当前函数返回”；成功值才会绑定到下一行。? 的使用前提是：当前函数的返回类型能接住该残差。

> 适用环境：core/no_std；fs crate；内核态；RISC-V/LoongArch 均可。下面是仓库中 FS_ERRNO 设计的直接模板。

~~~rust
use crate::errno::FS_ERRNO;

fn checked_range(start: usize, len: usize) -> Result<usize, FS_ERRNO> {
    start
        .checked_add(len)
        .ok_or(FS_ERRNO::EINVAL)
}

fn require_inode(
    dir: &crate::vfs::Inode,
    name: &str,
) -> Result<alloc::sync::Arc<crate::vfs::Inode>, FS_ERRNO> {
    let child = dir.find(name).ok_or(FS_ERRNO::ENOENT)?;
    Ok(child)
}
~~~

### 3.3 map 与 map_err：分别变成功值和错误值

> 适用环境：core/no_std；fs/内核态；底层 ext4 错误需要变为 CosmOS FS_ERRNO。

~~~rust
use crate::errno::FS_ERRNO;

fn ext4_create_result(
    ext4: &mut Ext4Like,
    parent: u32,
    name: &str,
) -> Result<alloc::sync::Arc<Node>, FS_ERRNO> {
    let inode = ext4
        .create(parent, name)
        .map_err(FS_ERRNO::from)?;
    Ok(alloc::sync::Arc::new(Node::from(inode)))
}

struct Ext4Like;
struct Ext4Error;
struct Node;
impl Ext4Like {
    fn create(&mut self, _parent: u32, _name: &str) -> Result<u32, Ext4Error> { Ok(1) }
}
impl From<Ext4Error> for FS_ERRNO {
    fn from(_error: Ext4Error) -> Self { FS_ERRNO::EIO }
}
impl Node {
    fn from(_inode: u32) -> Self { Self }
}
~~~

真实代码在 [fs/src/ext4/mod.rs](../../fs/src/ext4/mod.rs:933) 中先用 map_err(FS_ERRNO::from)?，而在 [fs/src/vfs.rs](../../fs/src/vfs.rs:698) 中对写回错误记录 inode/offset/长度后再用 ? 传播。关键是“错误转换”要发生在 ? 之前。

### 3.4 and_then 与错误 fallback

> 适用环境：core/no_std；解析/资源获取；内核态或用户态；RISC-V/LoongArch 均可。

~~~rust
fn parse_and_check(s: &str) -> Result<u16, ParseError> {
    s.parse::<u16>()
        .map_err(|_| ParseError::BadNumber)
        .and_then(|value| {
            if value == 0 {
                Err(ParseError::Zero)
            } else {
                Ok(value)
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    BadNumber,
    Zero,
}
~~~

如果只是把成功值转换成普通值，用 map；如果闭包要返回 Result，用 and_then。如果要把错误切换为另一条完整路径，才考虑 or_else：

> 适用环境：core/no_std；设备/文件系统 fallback；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn read_from_cache_or_backend<T, E>(
    cache: Result<T, E>,
    backend: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    cache.or_else(|_cache_error| backend())
}
~~~

不要把 or_else 当成无条件吞错；若缓存损坏和缓存未命中不同，错误类型中应保留差别，或只对明确可恢复的错误 fallback。

### 3.5 当前仓库的 Result 边界

fs/src/vfs.rs 有一组值得现场照抄的兼容模式：

> 适用环境：fs/no_std；内核态；RISC-V/LoongArch 均可；使用仓库 FS_ERRNO。

~~~rust
// 旧后端只有 Option API：只能给一个保守的 EIO。
fn create_result(&self, name: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
    self.create(name).ok_or(FS_ERRNO::EIO)
}

// 新后端保留真实错误：先 map，再 ?，最后包装 VFS 节点。
fn create_result(&self, name: &str) -> Result<Arc<Inode>, FS_ERRNO> {
    let child = self.inner.create_result(name).map(|node| {
        Self::wrap(node)
    })?;
    Ok(child)
}
~~~

上面两个同名函数分别来自 trait 与 wrapper，不能在同一个 impl 中原样重复粘贴；它们展示的是两层 API 的形状。真正路径见 [fs/src/vfs.rs](../../fs/src/vfs.rs:290) 和 [fs/src/vfs.rs](../../fs/src/vfs.rs:597)。

## 4. ? 与错误传播：现场按返回类型排查

### 4.1 ? 到底要求什么

| 当前表达式 | 当前函数通常返回 | 失败动作 |
| --- | --- | --- |
| Option<T> | Option<U> | None 立即返回 |
| Result<T, E1> | Result<U, E2> 且 E2: From<E1> | Err(E1) 转成 Err(E2) 后返回 |
| Result<T, E> | Result<U, E> | 原错误返回 |
| Option<T> | Result<U, E> | 不能直接 ?；先 ok_or/ok_or_else |
| Result<T, E> | Option<U> | 不能直接 ?；若确实要丢错，先 ok() |

“? 不能用于该类型”“FromResidual/From 不满足”时，检查步骤固定为：

1. 把函数签名贴出来，看它到底返回 Option 还是 Result。
2. 看出错表达式的具体类型，不要只看变量名；rg -n 找定义或临时拆成一行。
3. Result 的错误类型若不同，检查是否存在 From<底层错误> for 目标错误。
4. 没有转换时在 ? 前使用 map_err(...)；语义是缺失时才用 ok_or(...)。
5. 最后再决定是否 .ok() 丢失错误；在内核边界通常不应这么做。

### 4.2 混合 Option/Result 的三种标准桥接

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
// 1. None 是一个明确的错误：Option -> Result。
let node = maybe_node.ok_or(FS_ERRNO::ENOENT)?;

// 2. 现有错误确实无关：Result -> Option；错误会被丢掉。
let maybe_number = text.parse::<usize>().ok();

// 3. 可选字段存在时还要验证：Option<Result<T, E>> -> Result<Option<T>, E>。
let parsed = raw.map(parse_one).transpose()?;
~~~

不要把 .ok() 与 ? 连在一起掩盖设备错误：

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。以下只展示错误会被有意丢弃的危险写法。

~~~rust
// 危险：I/O error、格式错误、资源耗尽都变成 None。
let child = backend.create(name).ok()?;

// 更好：在 API 边界保留错误。
let child = backend.create_result(name)?;
~~~

### 4.3 Result<(), E> 的资源操作模板

> 适用环境：core/no_std；内核态/fs；RISC-V/LoongArch 均可。

~~~rust
fn update_inode(node: &InodeLike, mode: u32) -> Result<(), FsError> {
    node.set_mode(mode)?;
    node.invalidate_cache();
    Ok(())
}

struct InodeLike;
struct FsError;
impl InodeLike {
    fn set_mode(&self, _mode: u32) -> Result<(), FsError> { Ok(()) }
    fn invalidate_cache(&self) {}
}
~~~

CosmOS 的 Inode::truncate、set_mode、set_owner、set_times 等函数就是这个形状：底层成功后更新缓存状态，再 Ok(())；底层失败由 ? 直接返回。

### 4.4 什么时候用 match 而不是一长串组合

需要清理已获得资源、打印不同错误、修改多个状态或对错误分支重试时，显式 match 往往更安全：

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
use core::convert::TryInto;

fn read_header(buf: &[u8]) -> Result<u32, ParseError> {
    let bytes = match buf.get(..4) {
        Some(bytes) => bytes,
        None => return Err(ParseError::TooShort),
    };

    match bytes.try_into() {
        Ok(array) => Ok(u32::from_le_bytes(array)),
        Err(_) => Err(ParseError::TooShort),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    TooShort,
}
~~~

这里 get(..4) 的 Option 表示切片范围不存在；转成 Result 后给解析器一个可检索的错误。

## 5. 迭代器：适配器、消费者与所有权

### 5.1 惰性链的骨架

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。以下是概念流程，不是可执行代码。

~~~text
集合/切片
  └─ iter / iter_mut / into_iter
       └─ map / filter / enumerate / zip / take / skip / flatten ...（惰性）
            └─ find / any / all / next / collect / fold / for_each ...（触发执行）
~~~

| 目标 | 常用写法 | 返回/副作用 | 复杂度提示 |
| --- | --- | --- | --- |
| 一项转换 | .map(f) | 新迭代器 | 不分配，直到消费者执行 |
| 过滤 | .filter(pred) | 新迭代器 | 谓词拿到元素引用 |
| 查第一项 | .find(pred) | Option<Item> | 找到即停止 |
| 查第一项并转换 | .find_map(f) | Option<U> | 把 filter + map + find 合成一段 |
| 找索引 | .position(pred) | Option<usize> | 找到即停止 |
| 任一/全部 | .any(pred) / .all(pred) | bool | 短路 |
| 加索引 | .enumerate() | (usize, Item) | 索引从 0，注意 offset 是否另有语义 |
| 配对 | .zip(other) | 元组迭代器 | 以较短者为止 |
| 展开嵌套迭代器 | .flat_map(f) / .flatten() | 单层元素 | Result::Err 也可能被 flatten 当空迭代器 |
| 累积 | .fold(init, f) | 一个值 | 不必收集到 Vec |
| 可失败累积 | .try_fold / .try_for_each | Result/Option | 第一个失败短路 |
| 收集 | .collect() | 由目标类型决定 | 常会分配 Vec |

### 5.2 iter、iter_mut、into_iter 先看清楚

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
let mut values = alloc::vec![1u32, 2, 3];

let borrowed: alloc::vec::Vec<u32> =
    values.iter().map(|value| *value + 1).collect();

values.iter_mut().for_each(|value| *value *= 2);

let consumed: alloc::vec::Vec<u32> =
    values.into_iter().map(|value| value + 10).collect();
// values 在 into_iter 后不能再使用。
~~~

常见元素类型：

- slice.iter() 的 Item 是 &T；slice.iter_mut() 的 Item 是 &mut T。
- Vec<T>.into_iter() 的 Item 是 T，会移动元素。
- &Vec<T>/&[T] 的 into_iter() 是借用迭代；为了消除 edition/类型歧义，现场直接写 .iter() 最稳。
- Option<T>、Result<T, E> 也可迭代：Some(x)/Ok(x) 产生一个元素，None/Err(_) 产生零个元素。这正是 flatten 丢掉 Err 的根源之一。

### 5.3 高概率适配器模板

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn selected_ids(items: &[(u64, bool)]) -> alloc::vec::Vec<u64> {
    items
        .iter()
        .enumerate()
        .filter(|(_, (_, enabled))| *enabled)
        .map(|(_, (id, _))| *id)
        .collect()
}

fn first_even(items: &[u32]) -> Option<(usize, u32)> {
    items
        .iter()
        .copied()
        .enumerate()
        .find(|(_, value)| *value % 2 == 0)
}
~~~

如果闭包参数被多层引用包住，先用 copied()/cloned() 简化类型，再写谓词，通常比堆叠 && 可读：

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn simple_filters<'a>(
    numbers: &[u32],
    names: &'a [&'a str],
) -> (Option<u32>, alloc::vec::Vec<&'a str>) {
    let first = numbers.iter().copied().find(|value| *value > 100);
    let names = names
        .iter()
        .copied()
        .filter(|name| !name.is_empty())
        .collect::<alloc::vec::Vec<_>>();
    (first, names)
}
~~~

filter 的谓词总是拿到 &Item。因此 numbers.iter().filter(|value| ...) 中 value 是 &&u32；加 copied() 后 Item=u32，谓词参数就只是 &u32。

### 5.4 find、find_map、map 的区别

> 适用环境：core/no_std；目录遍历/协议解析/资源扫描；内核态或用户态；RISC-V/LoongArch 均可。

~~~rust
struct Entry {
    name: &'static str,
    ino: u64,
}

fn search_examples<'a>(
    entries: &'a [Entry],
    wanted: &str,
) -> (Option<&'a Entry>, Option<u64>, alloc::vec::Vec<u64>) {
    // 只查找：返回原元素。
    let entry = entries.iter().find(|entry| entry.name == wanted);

    // 查找并生成另一种值：找到一项后停止。
    let ino = entries
        .iter()
        .find_map(|entry| (entry.name == wanted).then_some(entry.ino));

    // 先转换所有项，再由消费者决定是否停止/收集。
    let inos = entries
        .iter()
        .map(|entry| entry.ino)
        .collect::<alloc::vec::Vec<_>>();
    (entry, ino, inos)
}
~~~

find_map 特别适合把“条件 + 可选转换”写成一个闭包；如果闭包内部还有可失败的 Result，优先考虑显式 for/try_for_each，不要强行丢错误。

### 5.5 collect：目标类型决定语义

> 适用环境：core/no_std + alloc；内核态/用户态；RISC-V/LoongArch 均可。以下收集到 Vec 会分配内存。

~~~rust
use alloc::vec::Vec;

fn parse_all(raw: &[&str]) -> Result<Vec<u32>, ParseError> {
    raw.iter()
        .map(|text| parse_one(text))
        .collect::<Result<Vec<_>, ParseError>>()
}

fn keep_valid(raw: &[&str]) -> Vec<u32> {
    raw.iter()
        .filter_map(|text| parse_one(text).ok())
        .collect::<Vec<_>>()
}

fn parse_one(text: &str) -> Result<u32, ParseError> {
    text.parse::<u32>().map_err(|_| ParseError::BadNumber)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    BadNumber,
}
~~~

语义对比：

| 写法 | 遇到第一个错误 | 是否保留错误 |
| --- | --- | --- |
| collect::<Result<Vec<_>, _>>() | 立即停止 | 是，返回该 Err |
| filter_map(x => x.ok()).collect::<Vec<_>>() | 跳过该项 | 否，明确丢弃 |
| .flatten().collect::<Vec<_>>()，元素为 Result | Err 产生零项 | 否，最容易误用 |

类型推导失败时，把目标类型写在变量上比在链尾乱加 turbofish 更容易读：

> 适用环境：core/no_std + alloc；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn collect_with_annotation(
    raw: &[&str],
) -> Result<alloc::vec::Vec<u32>, ParseError> {
    let parsed: Result<alloc::vec::Vec<u32>, ParseError> =
        raw.iter().map(|s| parse_one(s)).collect();
    parsed
}

fn parse_one(text: &str) -> Result<u32, ParseError> {
    text.parse::<u32>().map_err(|_| ParseError::BadNumber)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    BadNumber,
}
~~~

### 5.6 flatten / flat_map：看清要展开的层

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
let nested = [Some(1u32), None, Some(3)];
let values: alloc::vec::Vec<u32> = nested.into_iter().flatten().collect();

let chunks = [[1u32, 2], [3, 4]];
let all: alloc::vec::Vec<u32> = chunks.into_iter().flatten().collect();

let words: alloc::vec::Vec<u8> = ["ab", "c"]
    .into_iter()
    .flat_map(|word| word.bytes())
    .collect();
~~~

下面这段虽然能编译，但在错误处理代码里经常是 bug：

> 适用环境：core/no_std + alloc；迭代器元素是 Result；内核态/用户态；RISC-V/LoongArch 均可。此处专门展示错误被静默丢弃的反例。

~~~rust
let values: alloc::vec::Vec<u32> = results.into_iter().flatten().collect();
// results: Iterator<Item = Result<u32, E>>
// Ok(x) 产生 x，Err(_) 产生零个元素；所有错误被静默丢弃。
~~~

要“展开成功值但保留第一个错误”，用 collect::<Result<...>>()；要逐项有副作用且失败立即停止，用 try_for_each。

### 5.7 fold、try_fold、try_for_each

> 适用环境：core/no_std；解析、校验、统计；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |sum, byte| sum + *byte as u32)
}

fn checked_sum(bytes: &[u8]) -> Result<u32, ParseError> {
    bytes.iter().try_fold(0u32, |sum, byte| {
        sum.checked_add(*byte as u32).ok_or(ParseError::Overflow)
    })
}

fn validate_all(values: &[u32]) -> Result<(), ParseError> {
    values.iter().try_for_each(|value| {
        if *value == 0 {
            Err(ParseError::Zero)
        } else {
            Ok(())
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseError {
    Overflow,
    Zero,
}
~~~

try_fold 的闭包要返回可短路的 Result/Option；不要在闭包里写一个与外层返回类型无关的 ?，否则会遇到残差类型不匹配。现场不确定时，显式 for 循环通常更容易让借用检查器和人都看懂。

### 5.8 CosmOS 中的迭代器映射

| 源码位置 | 真实模式 | 现场提醒 |
| --- | --- | --- |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:224) | ls().into_iter().map(...).collect()；内部 find(...).map(...).unwrap_or(0) | 目录项快照会物化 Vec；None 的 inode 用 0 是该接口约定，不是通用规则 |
| [fs/src/easyfs/inode.rs](../../fs/src/easyfs/inode.rs:102) | 先在锁内读出 (name, block_id, offset)，锁外 into_iter().map(...).collect() | 这是避免持锁做后续 inode 读取的好例子 |
| [fs/src/easyfs/bitmap.rs](../../fs/src/easyfs/bitmap.rs:40) | iter().enumerate().find(...).map(...) | 找到一个空闲字后短路，不扫描后续 word |
| [fs/src/fat32/dir.rs](../../fs/src/fat32/dir.rs:326) | (0..last_order).map(_ => None).collect() | 目标类型是 Vec<Option<&LfnPart>> |
| [fs/src/vfs.rs](../../fs/src/vfs.rs:250) | entries.iter().enumerate().skip(offset) | offset 是后端定义的目录项位置，不要自动当字节偏移 |
| [fs/src/dentry_cache.rs](../../fs/src/dentry_cache.rs:197) | values().filter(...).count() | 迭代器适合统计；仍要注意锁的生命周期 |
| [user/src/bin/sh.rs](../../user/src/bin/sh.rs:53) | split_whitespace().collect()、iter().map(...).collect()、iter_mut().for_each(...) | Vec<&str> 借用原命令行；转成 Vec<String> 后才可追加 \\0 |
| [user/src/bin/dns_probe.rs](../../user/src/bin/dns_probe.rs:44) | bytes.iter().copied().chain(core::iter::once(b'.')) | chain(once(...)) 可在 no_std 中补一个哨兵字节 |

## 6. 闭包与 Fn / FnMut / FnOnce

### 6.1 捕获方式

编译器根据闭包体实际使用方式选择捕获模式：

| 捕获方式 | 例子 | 对外部变量的影响 |
| --- | --- | --- |
| 不捕获 | closure x => x + 1 | 可退化成函数指针，最轻 |
| 共享借用 | closure x => x < limit | 外部 limit 仍由原作用域拥有 |
| 可变借用 | closure x => { total += x } | 外部变量必须 mut，闭包调用需要 mut 闭包绑定 |
| 移动捕获 | move closure => owned_string | 将值所有权移入闭包；常用于返回闭包/延迟初始化 |

move 只强制捕获方式，不等于闭包一定只能调用一次。一个 move 且只读取 Copy 值的闭包仍可能实现 Fn；是否实现 Fn/FnMut/FnOnce 最终取决于闭包体。

### 6.2 三个 trait 的关系

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。以下是 trait 关系示意，不是可执行代码。

~~~text
Fn  ⊂  FnMut  ⊂  FnOnce
~~~

更准确地说：实现 Fn 的闭包也能以 FnMut/FnOnce 方式调用；实现 FnMut 的闭包也能以 FnOnce 方式调用。API 若只调用一次，使用 FnOnce 最宽松；需要多次且不修改捕获环境，才要求 Fn。

| API/场景 | 常见 bound | 原因 |
| --- | --- | --- |
| Option::map、unwrap_or_else、ok_or_else | FnOnce | 最多调用一次，允许闭包消费捕获值 |
| Iterator::map、filter、for_each | FnMut | 可能调用任意多次，允许每次更新捕获状态 |
| 只读判断函数 | Fn | 可重复调用且不修改捕获环境 |
| CosmOS bootinfo::for_each_usable_memory_region | impl FnMut(PhysMemoryRegion) | 遍历多个内存片段，并允许回调维护计数/状态 |
| CosmOS Inode::get_or_insert_page_cache_state | F: FnOnce() -> Arc<T> | 初始化最多执行一次，允许延迟构造资源 |

### 6.3 三种闭包模板

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。

~~~rust
fn apply_twice<F: Fn(i32) -> i32>(f: F, value: i32) -> i32 {
    f(f(value))
}

fn below(limit: usize) -> impl Fn(usize) -> bool {
    move |value| value < limit
}
~~~

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。FnMut 闭包的绑定也必须是 mut。

~~~rust
fn sum(values: &[usize]) -> usize {
    let mut total = 0;
    values.iter().for_each(|value| total += *value);
    total
}
~~~

> 适用环境：core/no_std + alloc；内核态/用户态；RISC-V/LoongArch 均可。适合页缓存、设备队列等延迟初始化。

~~~rust
use alloc::sync::Arc;

fn run_once<F: FnOnce() -> Arc<u8>>(init: F) -> Arc<u8> {
    init()
}

fn make_resource() -> Arc<u8> {
    let owned = Arc::new(7u8);
    let lazy = move || owned;
    run_once(lazy)
}
~~~

### 6.4 闭包捕获与借用检查器

常见报错与方向：

- closure may outlive the current function：闭包保存了局部引用；若 API 要求闭包拥有数据，使用 move，或让数据的生命周期与闭包一致。
- cannot borrow ... as mutable：外部变量要 mut，闭包变量也要 mut；若正在用 iter()，需要 iter_mut() 才能修改元素。
- use of moved value：into_iter()、move 闭包或 Option::map 已消费值；改用 as_ref()、iter()、cloned()，或重新组织所有权。
- 闭包返回 impl Fn 失败：闭包体修改了捕获变量，先把返回 bound 放宽到 impl FnMut；若消费了捕获值则只能 impl FnOnce。

> 适用环境：core/no_std；内核态；RISC-V/LoongArch 均可。这个模式还展示了不要让惰性迭代器意外延长锁的借用。

~~~rust
fn collect_names(names: &[&str]) -> alloc::vec::Vec<alloc::string::String> {
    names
        .iter()
        .map(|name| alloc::string::String::from(*name))
        .collect()
}
~~~

如果迭代器闭包捕获了锁 guard、临时 buffer 或短生命周期 slice，迭代器被返回/保存后，借用也会随之延长。需要“锁内只取快照、锁外处理”的结构时，先 collect 成拥有数据的 Vec，释放 guard 后再遍历；EasyInode::ls 采用了这种两阶段结构。

## 7. 竞赛高频可套用模式

### 7.1 no_std IPv4 / 数字解析：Option 版

这是用户态 DNS 探针中真实存在的模式：checked_mul/checked_add 失败时 ? 返回 None，末尾通过 chain(core::iter::once(...)) 补一个点来统一收尾。

> 适用环境：user/no_std；用户态；RISC-V 或 LoongArch；本例不需要 alloc。

~~~rust
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut index = 0usize;
    let mut current = 0u16;
    let mut has_digit = false;

    for byte in s.as_bytes().iter().copied().chain(core::iter::once(b'.')) {
        match byte {
            b'0'..=b'9' => {
                has_digit = true;
                current = current
                    .checked_mul(10)?
                    .checked_add((byte - b'0') as u16)?;
                if current > 255 {
                    return None;
                }
            }
            b'.' => {
                if !has_digit || index >= 4 {
                    return None;
                }
                out[index] = current as u8;
                index += 1;
                current = 0;
                has_digit = false;
            }
            _ => return None,
        }
    }

    (index == 4).then_some(out)
}
~~~

适合“输入不合法就换默认值”的命令行参数。如果上层需要区分“字符非法”和“数值溢出”，把返回类型改成 Result<[u8; 4], ParseError>，不要继续用 Option。

### 7.2 no_std 二进制头解析：Result 版

> 适用环境：core/no_std；内核态或用户态；RISC-V/LoongArch 均可。展示 checked arithmetic、切片边界和 ?。

~~~rust
use core::convert::TryInto;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderError {
    TooShort,
    BadMagic,
    Overflow,
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, HeaderError> {
    let end = offset.checked_add(4).ok_or(HeaderError::Overflow)?;
    let raw = bytes.get(offset..end).ok_or(HeaderError::TooShort)?;
    let array: [u8; 4] = raw.try_into().map_err(|_| HeaderError::TooShort)?;
    Ok(u32::from_le_bytes(array))
}

fn parse_header(bytes: &[u8]) -> Result<(u32, u32), HeaderError> {
    let magic = read_u32_le(bytes, 0)?;
    if magic != 0x534f_4d43 {
        return Err(HeaderError::BadMagic);
    }
    let length = read_u32_le(bytes, 4)?;
    let end = 8usize
        .checked_add(length as usize)
        .ok_or(HeaderError::Overflow)?;
    if end > bytes.len() {
        return Err(HeaderError::TooShort);
    }
    Ok((magic, length))
}
~~~

仓库的 Fat32Bpb::read_from 就是同一思路：checked_mul/checked_add/checked_sub 后用 ok_or(FS_ERRNO::EINVAL)?，格式验证失败返回 errno，而不是 panic。

### 7.3 命令行参数：借用、解析、默认值

> 适用环境：user/no_std；用户态；RISC-V/LoongArch 均可。

~~~rust
fn parse_arg(argv: &[&str], index: usize, default: usize) -> usize {
    argv.get(index)
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(default)
}
~~~

这与 user/src/bin/mmap_test.rs 一致。argv.get 解决越界，parse(...).ok() 把格式错误映射为 None，最后才统一使用默认值。若“参数存在但非法”应报错，就不要 unwrap_or，改为 Result。

### 7.4 VFS 查找：先决定缺失语义，再决定 API

> 适用环境：fs/no_std；内核态；RISC-V/LoongArch 均可；示例使用仓库 FS_ERRNO 与 Arc。

~~~rust
use alloc::sync::Arc;
use crate::errno::FS_ERRNO;
use crate::vfs::Inode;

fn open_child(parent: &Inode, name: &str) -> Result<Arc<Inode>, FS_ERRNO> {
    let child = parent.find(name).ok_or(FS_ERRNO::ENOENT)?;
    if !child.is_dir() {
        return Err(FS_ERRNO::ENOTDIR);
    }
    Ok(child)
}
~~~

如果底层已经有 create_result/mkdir_result/write_at_result，直接调用带 Result 的版本。VFS 的旧 create/mkdir 只是兼容接口，.ok() 会把 ext4 的 ENOSPC、ENOTDIR 等全部变成 None。

### 7.5 资源获取：Option、Result 与清理

#### 非阻塞接收：无数据是正常分支

> 适用环境：内核态；no_std；RISC-V/LoongArch；与仓库 VirtIO wrapper 的 try_recv 相同语义。

~~~rust
// 推荐的新接口形状：Ok(None) 是当前无包，Err 是设备失败。
fn poll_once(
    device: &VirtioLike,
    out: &mut [u8],
) -> Result<Option<usize>, DeviceError> {
    if device.has_device_error() {
        return Err(DeviceError::ReceiveFailed);
    }
    Ok(device.try_recv(out))
}

struct VirtioLike;
struct DeviceError;
impl VirtioLike {
    fn has_device_error(&self) -> bool { false }
    fn try_recv(&self, _out: &mut [u8]) -> Option<usize> { None }
}
impl DeviceError {
    const ReceiveFailed: Self = Self;
}
~~~

真实仓库的 VirtIONetDevice::try_recv 返回 Option<usize>，并在内部对若干底层 Result 使用 .ok()?；这是当前 API 的有意丢错边界。若题目要求诊断设备错误，不要照抄，改为 Result<Option<usize>, Error> 或增加明确的错误状态。

#### 多个资源逐步获取：失败时回收已经成功的部分

> 适用环境：core/no_std；内核态/用户态；RISC-V/LoongArch 均可。资源类型和错误类型是占位名。

~~~rust
fn acquire_all(count: usize) -> Result<alloc::vec::Vec<Resource>, Error> {
    let mut acquired = alloc::vec::Vec::new();
    for _ in 0..count {
        match acquire_one() {
            Ok(resource) => acquired.push(resource),
            Err(error) => {
                for resource in acquired.drain(..) {
                    release(resource);
                }
                return Err(error);
            }
        }
    }
    Ok(acquired)
}

struct Resource;
struct Error;
fn acquire_one() -> Result<Resource, Error> { Ok(Resource) }
fn release(_resource: Resource) {}
~~~

在真实内核中，Drop guard、Option::take() 或显式 cleanup 函数通常比在复杂迭代器闭包里做回收更容易审计。

### 7.6 page cache 类型擦除：and_then + ok

> 适用环境：fs/no_std；内核态；RISC-V/LoongArch；展示仓库真实 page-cache API 的组合方式。

~~~rust
fn page_cache_state<T: core::any::Any + Send + Sync>(
    inode: &crate::vfs::Inode,
) -> Option<alloc::sync::Arc<T>> {
    inode.page_cache_state::<T>()
}
~~~

真实实现见 fs/src/vfs.rs:917：先从锁保护的 Option<Arc<dyn Any + Send + Sync>> 借用，再 Arc::clone，最后 downcast::<T>().ok()。这里的 .ok() 只是在“查询的类型不是 T”这一层做有意的 Option 化；不是所有错误都应该如此处理。

### 7.7 目录遍历：map、find 与锁边界

> 适用环境：fs/no_std；内核态；RISC-V/LoongArch；简化的目录快照例子。

~~~rust
fn visible_regular_inodes(
    entries: alloc::vec::Vec<(alloc::string::String, FileType, alloc::sync::Arc<Inode>)>,
) -> alloc::vec::Vec<u64> {
    entries
        .into_iter()
        .filter(|(_, file_type, _)| *file_type == FileType::Regular)
        .map(|(_, _, inode)| inode.ino())
        .collect()
}

#[derive(PartialEq, Eq)]
enum FileType {
    Regular,
    Directory,
}
struct Inode;
impl Inode {
    fn ino(&self) -> u64 { 0 }
}
~~~

若目录后端只提供 ls() -> Vec<_>，先拿快照再处理；不要让一个遍历闭包同时持有文件系统锁、访问另一个 inode 并触发可能再次加锁的查找。CosmOS EasyFS 的两阶段 ls 已把这个边界写在 fs/src/easyfs/inode.rs:102。

## 8. 常见坑：错误现象 → 原因 → 检查步骤 → 修复方向

| 错误现象 | 原因 | 检查步骤 | 修复方向 |
| --- | --- | --- | --- |
| the ? operator can only be used in a function that returns ... | Option/Result 容器和函数返回类型不匹配 | 查看函数签名；把表达式拆成 let tmp = ... 看类型 | Option 函数返回 None；Result 函数用 ok_or/map_err 后再 ? |
| the trait From<...> is not implemented | ? 要把底层错误转成目标错误，但没有 From | 查看 Result 的两个 E；搜 impl From、map_err | 在 ? 前 map_err(TargetError::from) 或显式 match |
| expected Option, found Result | 混用了 .ok()/?，或 API 层次改变 | 检查函数签名、.ok()、ok_or、transpose 的调用位置 | 需要错误就保留 Result；只在边界有意 .ok() |
| expected Result, found Option | find/get 的缺失值没有 errno | 判断 None 的业务语义 | ok_or(ENOENT)、ok_or_else(...)，或改 API 返回 Result |
| 组合后出现 Option<Option<T>> | map 闭包返回了 Option<T> | 查看推导类型 | 改用 and_then 或链尾 flatten() |
| flatten() 后错误项消失 | 迭代 Result 时 Err 的 IntoIterator 是空 | 搜 flatten 前的 Item 类型 | collect::<Result<Vec<_>, _>>()；错误无关才 filter_map |
| borrow of moved value | into_iter、map、move 闭包或 take 消费了值 | 找最近一次消费点 | iter、as_ref、as_mut、cloned，或移动后不再使用 |
| cannot borrow as mutable | 修改捕获变量却没有 mut，或用了不可变迭代器 | 看闭包是否写外部状态；看 iter/iter_mut | mut 闭包、mut 外部变量、iter_mut；必要时改 FnMut |
| closure may outlive current function | 返回/保存的闭包借用了局部变量 | 看闭包是否保存 &local | move 转移拥有的数据，或增加明确生命周期 |
| expected Fn, found FnMut/FnOnce | API bound 比闭包实际能力更严格 | 看是否修改/消费捕获值 | 只调用一次用 FnOnce；允许改状态用 FnMut |
| collect 类型推导失败 | 不知道目标集合或错误类型 | 查看 Iterator::Item | 写 let 目标类型或 turbofish |
| 代码“没有执行” | 只构造惰性迭代器，没有消费者 | 搜链尾是否有 for/find/collect/fold | 加消费者；需要副作用优先 for |
| 内核随机 panic / guest 退出 | 对磁盘、FDT、设备、用户输入用了 unwrap | 搜 unwrap；查看 panic 位置 | 外部数据用 Option/Result；只对已验证不变量 unwrap |
| None 无法判断是 miss 还是负缓存 | 多个业务状态压成 Option | 查缓存 API 文档和调用者分支 | 定义 Positive/Negative/Miss 等领域枚举 |
| String/Vec 找不到或 std unresolved | no_std 代码误用 std 或遗漏 alloc | 看 crate 顶部和 imports | extern crate alloc；use alloc::string::String/vec::Vec |
| 宿主小程序能跑，内核不能链接 | 示例依赖 std、系统调用或默认 allocator | 搜 use std、Cargo.toml | 改用 core；需要堆时确认 alloc 与 allocator |
| RISC-V 能编译，LoongArch 失败 | target、链接脚本或 cfg 分支不齐 | rustup target list；看 Makefile/config | 分架构编译，用 cfg 隔离汇编/地址常量 |
| cargo 去联网或找不到包 | 缓存/registry/target 不完整；OFFLINE=1 不等于 Cargo --offline | 检查命令和 Cargo cache | 离线前预热；使用 cargo ... --offline |
| unwrap_or(expensive()) 产生无关分配/锁 | eager fallback 已经执行 | 看默认值是否有副作用 | 改 unwrap_or_else；同理 ok_or_else/map_or_else |
| 目录枚举 deadlock 或借用过长 | 惰性闭包捕获锁 guard | 看 guard 作用域和迭代器是否逃逸 | 锁内复制最小快照，drop guard 后遍历 |
| Option::take 后后续访问为空 | take 是移动，不是借用 | 看 take 后是否访问原槽位 | 先保存返回值；明确是否放回 Some |

## 9. no_std 现场注意点

### 9.1 导入清单

| 需求 | 写法 |
| --- | --- |
| Option/Result/基本迭代器 | 通常由 core prelude 提供；不确定时用 core::option::Option、core::result::Result |
| Vec | use alloc::vec::Vec；crate 顶部 extern crate alloc |
| String | use alloc::string::String |
| Arc | use alloc::sync::Arc |
| 固定数组/切片 | 优先 [T; N]、&[T]，不需要 allocator |
| 迭代器哨兵 | core::iter::once(value) |
| TryInto | 编译器提示缺 trait 时 use core::convert::TryInto |
| 格式化 | core::fmt；不要默认使用 std::fmt |

no_std 不等于完全不能分配：CosmOS 的 os、fs、user 都显式接入 alloc，但使用 Vec/String 仍会触发堆分配。早期启动、异常路径、锁内热点和固定大小协议头更适合固定数组或手写 for。

### 9.2 panic、unwrap 和外部数据

CosmOS 内核的 panic handler 会打印位置后关闭系统；用户库的分配失败也会 panic。因此：

- FDT、磁盘 BPB、目录项、网络包、用户指针都是外部/可损坏输入：先边界检查，再 Option/Result。
- unwrap 只适合已经通过结构不变量证明不会失败的情况；现场修题时确认输入不是用户可控。
- unwrap_or(0)/unwrap_or_default() 会掩盖问题；日志、errno 或统计需要保留时，显式 match 更好。
- Result<(), E> 不是“没有返回值就不用处理错误”；它正是系统调用、写回、同步、权限更新等操作的正常接口。

### 9.3 锁、分配与迭代器

迭代器链本身通常不分配，但 collect::<Vec<_>>()、map 中构造 String/Arc、flat_map 产生动态数据都会分配。内核现场审查时逐层问：

1. 这个链何时执行？消费者在哪里？
2. 执行时是否还持有 spin/sleep mutex？
3. 闭包是否捕获 guard、用户指针或短生命周期 slice？
4. 中途 Err/None 时已经拿到的资源由谁释放？
5. 分配失败是 None、panic 还是 Result::Err？是否和调用层契约一致？

### 9.4 RISC-V / LoongArch 标注规则

Option/Result 代码一般可以标“两个架构均可”，但以下代码不能泛化：

- cfg(target_arch = "riscv64")/cfg(target_arch = "loongarch64") 下的全局汇编、CSR、地址常量、trap 结构；
- user/cargo-config 的 RISC-V +f,+d target feature 与 LoongArch 链接参数；
- VirtIO transport 设备初始化与 QEMU 参数；
- 直接解引用 FDT、页表、用户指针的 unsafe 路径。

文档中的 generic 代码若没有设备/汇编/地址依赖，可以在两种架构复用；实际粘贴到架构分支前仍需跑相应 target 构建。

## 10. CosmOS 文件路径与函数映射索引

| 主题 | 先看文件/函数 | 你要观察什么 |
| --- | --- | --- |
| 位图资源分配 | [fs/src/easyfs/bitmap.rs](../../fs/src/easyfs/bitmap.rs:32) Bitmap::alloc | find 找空闲 word，map 算 bit，None 表示没有空闲位 |
| EasyFS 目录查找 | [fs/src/easyfs/inode.rs](../../fs/src/easyfs/inode.rs:50) find_inode_id、:137 find | 目录扫描用 Option，命中后 map 包装 Arc<dyn VfsNode> |
| FAT BPB 验证 | [fs/src/fat32/bpb.rs](../../fs/src/fat32/bpb.rs:28) Fat32Bpb::read_from | checked arithmetic + ok_or(FS_ERRNO::EINVAL)? |
| FAT 8.3/LFN 解析 | [fs/src/fat32/dir.rs](../../fs/src/fat32/dir.rs:123) sfn_from_str；:301 assemble_lfn | Option 失败传播、Vec<Option<_>>、slot?、字符串分配 |
| FAT 目录查找 | [fs/src/fat32/inode.rs](../../fs/src/fat32/inode.rs:288) | into_iter().find、as_ref().map、unwrap_or(false) |
| VFS 双 API | [fs/src/vfs.rs](../../fs/src/vfs.rs:290) trait；:544/:593 wrapper | 旧 Option 接口与新 Result 接口的兼容边界 |
| VFS 结果传播 | [fs/src/vfs.rs](../../fs/src/vfs.rs:597) create_result；:698 write_at_result | map/map_err 后 ?，写回失败保留 errno |
| VFS 可选元数据 | [fs/src/vfs.rs](../../fs/src/vfs.rs:756) mode/uid/gid；:842 时间 | 缺席字段与真实错误的区分 |
| page cache 状态 | [fs/src/vfs.rs](../../fs/src/vfs.rs:917) page_cache_state；:949 get_or_insert_page_cache_state | and_then(downcast.ok()) 与 FnOnce 延迟初始化 |
| dentry cache | [fs/src/dentry_cache.rs](../../fs/src/dentry_cache.rs:48) DentryLookup；:89 lookup | Positive/Negative/Miss 不可压成单个 Option |
| ext4 目录查找 | [fs/src/ext4/mod.rs](../../fs/src/ext4/mod.rs:700) lookup_child_meta | Result 的 as_ref 观察、成功 map、后端 metadata 校正 |
| ext4 创建/写入 | [fs/src/ext4/mod.rs](../../fs/src/ext4/mod.rs:933)、:1113 | .ok() 兼容旧 API；map_err(FS_ERRNO::from)? 保留错误 |
| VirtIO 接收 | [os/src/drivers/net/virtio_net.rs](../../os/src/drivers/net/virtio_net.rs:38) try_new、:106 try_recv | Result::ok()? 的有意丢错边界、槽位 get_mut()?.take()? |
| FDT/启动信息 | [os/src/bootinfo.rs](../../os/src/bootinfo.rs:103)、:267、:305 | 可选 FDT 来源、Option 解析、FnMut 内存区域回调 |
| 用户态 DNS | [user/src/bin/dns_probe.rs](../../user/src/bin/dns_probe.rs:26) 到 :115 | Option 解析、checked arithmetic、chain(once(...))、? |
| 用户态 shell | [user/src/bin/sh.rs](../../user/src/bin/sh.rs:19) 到 :78 | position、split_whitespace、map/collect、iter_mut/for_each、Option<String> 重定向 |

## 11. 官方知识离线索引

注意：Option 的 flatten 与 Iterator 的 flatten 在当前 pinned nightly 可用；Result::flatten 仍需要 result_flattening unstable feature，本资料不建议在 CosmOS 现场启用。相同错误类型的 Result 嵌套可用 and_then(|inner| inner) 显式压平。

这些是稳定的官方文档入口；有网时应保存到离线资料包，不要在决赛现场依赖网络。正文的结论已按本仓库 no_std 和工具链约束重新组织，并非照抄教程。

- [core::option::Option](https://doc.rust-lang.org/core/option/enum.Option.html)：组合方法、transpose、flatten、懒惰 fallback。
- [core::result::Result](https://doc.rust-lang.org/core/result/enum.Result.html)：map_err、and_then、collect 相关 trait 实现。
- [Iterator trait](https://doc.rust-lang.org/core/iter/trait.Iterator.html)：适配器、消费者、fold、try_fold、collect。
- [Rust Reference：Closures](https://doc.rust-lang.org/reference/types/closure.html)：捕获方式、move、闭包类型。
- [Rust Reference：? operator](https://doc.rust-lang.org/reference/expressions/operator-expr.html#the-question-mark-operator)：错误/残差传播规则。
- [The Rust Book：Closures](https://doc.rust-lang.org/book/ch13-01-closures.html) 与 [Iterators](https://doc.rust-lang.org/book/ch13-02-iterators.html)：适合赛前复习，但示例中的 std 代码要换成 core/alloc。

## 12. 现场 checklist

### 写代码前

- [ ] 我是在 os、fs 还是 user crate？文件顶部是否 #![no_std]？
- [ ] 当前目标是 riscv64gc-unknown-none-elf 还是 loongarch64-unknown-none？是否碰到架构专属代码？
- [ ] “没有值”是正常缺席、缓存 miss、资源耗尽，还是必须报告的 errno？据此选 Option 或 Result。
- [ ] 函数签名先定好：Option<T>、Result<T, E>、Result<Option<T>, E> 还是 Option<Result<T, E>>？

### 写组合链时

- [ ] 闭包返回普通值用 map；返回 Option/Result 用 and_then。
- [ ] 只在 None/Err 时做昂贵 fallback，用 *_or_else；不要误用 eager 版本。
- [ ] 每条惰性迭代器链末尾有消费者；检查 find/collect 是否应短路。
- [ ] iter 是借用、iter_mut 是可变借用、into_iter 是移动；确认链后原容器是否还要用。
- [ ] collect 的目标类型明确；Result 批处理不能用 flatten 偷丢错误。
- [ ] ? 前的错误类型能否通过 From 转换？不行就 map_err。
- [ ] Option::take/闭包 move 后，原变量是否还会被访问？

### 交付/调试前

- [ ] 外部数据路径没有无理由的 unwrap；失败时错误/errno/日志可定位。
- [ ] 资源获取中途失败会释放已获得资源；锁 guard 不会被惰性迭代器带出锁区。
- [ ] 需要 Vec/String 时已导入 alloc，且该 crate 确实初始化了 allocator。
- [ ] 先跑 rustc --version、target 检查和对应 Makefile；不要从仓库根目录执行无 manifest 的 Cargo 命令。
- [ ] 离线构建命令带 --offline，并确认缓存完整；OFFLINE=1 只影响 Makefile 的工具安装分支。
- [ ] RISC-V 与 LoongArch 若都在交付范围内，分别编译至少一次；不要把一个架构的 cfg 结果当成另一个架构事实。
- [ ] 最后用 git diff --check -- docs/final_prepare/03-rust-option-result-iterators.md 检查 Markdown 尾随空格与冲突标记，并确认没有修改职责范围外的文件。
