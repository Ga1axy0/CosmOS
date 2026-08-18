# 12. 二进制静态分析：objdump、readelf、addr2line 与 ELF 定位

这篇文档面向现场断网环境。目标不是记住所有命令，而是建立一条稳定的故障定位链：

```text
故障地址/符号
    -> 判断它属于哪一个 ELF、哪一种地址空间、哪一个架构
    -> readelf 检查 ELF 头、段、节、符号、重定位
    -> nm/readelf 找符号范围
    -> addr2line 找源文件和行号
    -> objdump 反汇编故障指令及其邻域
    -> 回到 Rust/汇编/页表/加载器代码验证假设
```

这里的“静态分析”指只观察已经生成的 ELF、裸二进制、符号表、DWARF 和反汇编，不依赖程序正在运行。它特别适合分析：

- QEMU 日志中的 `sepc`、`stval`、`scause`、`ERA`、`BADV` 或普通 PC；
- 内核启动早期跳转错误、链接地址和加载地址不一致；
- `panic`、非法指令、页错误、异常返回、上下文切换和陷阱入口；
- 用户程序的入口点、`PT_LOAD` 映射、`PT_INTERP`、PIE/load bias 和动态重定位；
- “源代码看起来正确，但实际执行的指令不是想象中的那条”这类问题。

> 核心原则：**优先分析未剥离的 ELF；裸 `.bin` 只适合加载，不适合符号和源代码定位。**

---

## 1. 先分清仓库里的几种产物

### 1.1 内核产物

`os/Makefile` 当前的关键变量是：

| 架构 | Rust target | 内核 ELF（推荐分析） | 裸二进制（主要用于 RISC-V 加载） |
| --- | --- | --- | --- |
| RISC-V | `riscv64gc-unknown-none-elf` | `os/target/riscv64gc-unknown-none-elf/<mode>/os` | `os/target/riscv64gc-unknown-none-elf/<mode>/os.bin` |
| LoongArch | `loongarch64-unknown-none` | `os/target/loongarch64-unknown-none/<mode>/os` | 可能由构建流程额外生成，但分析仍优先使用 ELF |

其中 `<mode>` 通常是 `debug` 或 `release`。`debug` ELF 通常保留更多 DWARF 信息，适合 `addr2line -C` 和 `objdump -S`；`release` 经过优化后可能内联、合并或删除函数，源代码行号不一定直观。

`os/Makefile` 的构建目标会保留 `KERNEL_ELF`，而 `build`/`fast-run` 还会执行类似下面的转换：

```text
objcopy KERNEL_ELF --strip-all -O binary KERNEL_BIN
```

所以：

- `os/.../os` 是 ELF，有 ELF 头、段、节、符号，可能有 DWARF；
- `os/.../os.bin` 是从 ELF 中剥离元数据后得到的连续字节流，没有入口地址、符号名和源代码行号；
- QEMU 加载 `.bin` 时需要另行指定加载地址；
- 看到一个 `kernel-rv` 或 `kernel-la` 文件时，不要凭文件名判断格式，先执行 `file`。

仓库根目录的顶层 `Makefile` 还会把内核 ELF 复制为 `kernel-rv` 或 `kernel-la`，而 `os/Makefile` 的底层 RISC-V 流程明确使用 `KERNEL_BIN`。这两个层次的文件名和加载方式不要混为一谈。

### 1.2 用户程序产物

`user/Makefile` 对每个 `user/src/bin/*.rs` 构建出一个无后缀的 Cargo ELF，然后额外生成：

```text
user/target/<target>/<mode>/<app>       # Cargo 生成的 ELF
user/target/<target>/<mode>/<app>.elf   # 保留的 ELF 副本
user/target/<target>/<mode>/<app>.bin   # --strip-all 后的裸二进制
user/target/<target>/<mode>/<app>.asm   # rust-objdump -S 生成的反汇编
```

`user/build/elf/`、`user/build/bin/`、`user/build/asm/` 还可能保存一份便于打包的副本。用户态故障地址应该使用**同一轮构建生成的用户 ELF**，不能拿内核 ELF 或另一轮构建的 `.elf` 去做 `addr2line`。

注意：文件名带 `.elf` 不保证一定保留符号。当前用户程序可能因为 release profile 或打包流程已经是 stripped ELF；先用 `file`、`readelf -SW` 检查是否有 `.symtab`/`.debug_line`。需要源行定位时，优先用 `MODE=debug` 生成的未剥离 ELF，并确认它与运行中的用户程序来自同一轮构建。

例如可以在赛前分别准备两套用户程序分析产物：

```sh
make -C user ARCH=riscv64 MODE=debug all
make -C user ARCH=loongarch64 MODE=debug all
```

### 1.3 LoongArch 启动器

LoongArch QEMU 流程还会使用：

```text
bootloader/loongarch64-direct/target/loongarch64-unknown-none/release/loongarch64-direct-boot
```

如果 PC 落在启动器而不是内核，必须对这个启动器 ELF 做分析；对 `os/.../os` 做 `addr2line` 会得到 `??:0` 或完全错误的函数。

### 1.4 第一个动作：确认文件身份

```sh
file os/target/riscv64gc-unknown-none-elf/debug/os
file os/target/riscv64gc-unknown-none-elf/debug/os.bin
file user/target/riscv64gc-unknown-none-elf/release/init

readelf -hW os/target/riscv64gc-unknown-none-elf/debug/os
```

重点看：

- 是 `ELF 64-bit` 还是普通 `data`；
- `Machine` 是 RISC-V 还是 LoongArch；
- `Type` 是 `EXEC`、`DYN` 还是其他类型；
- `Entry point` 是否和当前架构、当前链接脚本及启动方式相符。

---

## 2. 工具选择：先检查环境，不要假设命令名

### 2.1 CosmOS 采用的反汇编入口

当前仓库的 `os/Makefile` 和 `user/Makefile` 使用：

```sh
rust-objdump --arch-name=riscv64 ...
rust-objdump --arch-name=loongarch64 ...
```

这通常来自 Rust 的 `cargo-binutils`/LLVM 工具链。常见准备方式是：

```sh
rustup target list --installed
rustup component list --installed | rg 'rust-src|llvm-tools'
cargo binutils --version
rust-objdump --version
rust-objdump --help | less
```

`os/Makefile` 的 `env` 目标会检查或安装 `cargo-binutils`、`rust-src` 和 `llvm-tools-preview`。比赛前应在断网前完成安装，并实际执行一次 RV 和 LA 的反汇编。

### 2.2 `rust-readelf`、`rust-addr2line` 不一定存在

工具名字会因主机发行版、`cargo-binutils` 版本和 Rust 组件而不同。不要把下面这些名字当成必然存在：

```text
rust-readelf
rust-addr2line
llvm-readelf
llvm-addr2line
```

先查：

```sh
for tool in rust-objdump readelf addr2line nm strings file \
            llvm-objdump llvm-readelf llvm-addr2line \
            riscv64-unknown-elf-objdump riscv64-unknown-elf-addr2line \
            loongarch64-unknown-elf-objdump loongarch64-unknown-elf-addr2line
do
    command -v "$tool" || true
done
```

可用工具通常分为三类：

| 任务 | 首选 | 备用和注意事项 |
| --- | --- | --- |
| ELF 头、段、节、符号、重定位 | `readelf`、`nm` | `llvm-readelf`、`llvm-nm`；这些信息大多与架构无关，但文件仍必须是正确的 ELF |
| RISC-V/LoongArch 反汇编 | 仓库使用的 `rust-objdump --arch-name=...` | 对应架构的 GNU cross `objdump`；主机 `/usr/bin/objdump` 可能不支持目标架构 |
| 地址到源代码 | 对应架构的 `addr2line` 或能识别该 ELF 的 `llvm-addr2line` | 主机 `addr2line` 常能读 ELF，但若报 unsupported architecture，应换 cross 工具 |
| 快速识别 | `file` | `readelf -hW` 是最终依据 |
| 字符串和符号搜索 | `strings`、`nm` | 必要时使用 `readelf -sW` 查看完整符号绑定和可见性 |

主机 `objdump` 即使能打开 ELF，也可能无法正确反汇编 RV/LA 指令。对于反汇编，**架构选项和工具实现必须匹配**；不要因为命令没有报错就认为结果可靠。

### 2.3 检查 Rust 工具链里的 LLVM 工具

如果 `llvm-tools-preview` 已安装，可以先定位实际文件：

```sh
SYSROOT="$(rustc --print sysroot)"
rustc --print sysroot
rustc --print target-libdir --target riscv64gc-unknown-none-elf
find "$SYSROOT/lib/rustlib" -type f \( \
    -name 'llvm-objdump' -o -name 'llvm-readelf' -o -name 'llvm-addr2line' \
\) -print 2>/dev/null
```

实际目录因 Rust 版本和主机 triple 而异。现场资料包中应记录“命令的绝对路径”或把工具目录加入 `PATH`，不要临场猜路径。

---

## 3. `readelf`：先读 ELF 的骨架

### 3.1 ELF header：文件是什么

```sh
ELF=os/target/riscv64gc-unknown-none-elf/debug/os
readelf -hW "$ELF"
```

主要字段：

| 字段 | 含义 | 分析用途 |
| --- | --- | --- |
| `Class` | `ELF32` 或 `ELF64` | CosmOS 当前两套目标都应是 64 位 |
| `Data` | 小端或大端 | 通常是 little endian |
| `Type` | `EXEC`、`DYN` 等 | 判断固定地址镜像、PIE 或共享对象 |
| `Machine` | 目标 ISA | 防止用错 RV/LA ELF |
| `Entry point address` | ELF 入口地址 | 判断启动入口和链接脚本是否匹配 |
| `Start of program headers` | 程序头表偏移 | `readelf -l` 的来源 |
| `Start of section headers` | 节头表偏移 | `readelf -S` 的来源 |
| `Number of program headers` | 段数量 | 加载器实际主要消费这些表项 |
| `Number of section headers` | 节数量 | 链接、调试和符号分析使用 |

**段（program header）和节（section header）不是一回事。**

- 加载器/QEMU 更关心 `PT_LOAD` 等 program header；
- 链接器、符号工具和调试信息更常按 section header 组织；
- 一个 `PT_LOAD` 可能包含多个节；
- stripped 裸 `.bin` 两者都没有。

### 3.2 Program headers：运行时怎样装入内存

```sh
readelf -lW "$ELF"
```

重点观察 `PT_LOAD`：

| 字段 | 含义 |
| --- | --- |
| `Offset` | 该段在 ELF 文件中的起始偏移 |
| `VirtAddr` | 该段的链接虚拟地址 |
| `PhysAddr` | 物理加载地址提示，固件/加载器可能使用 |
| `FileSiz` | 文件中实际存在的字节数 |
| `MemSiz` | 运行时占用的字节数，通常不小于 `FileSiz` |
| `Flg` | `R`、`W`、`E` 权限 |
| `Align` | 对齐要求，通常与页大小有关 |

典型推理：

- `MemSiz > FileSiz` 的尾部通常对应 `.bss`，加载时应清零；
- `R E` 段应包含代码，`R W` 段应包含可写数据；
- `VirtAddr` 与 QEMU/bootloader 实际装载地址不一致时，要考虑 load bias、物理别名或启动阶段分页状态；
- `PT_INTERP` 表示程序需要运行时解释器；内核静态 ELF 通常不会有用户态那样的解释器路径；
- `PT_DYNAMIC`、`PT_TLS`、`PT_PHDR`、`PT_GNU_STACK` 等是否存在，要结合用户程序加载器的实现判断，不能只看节名。

文件偏移和链接虚拟地址的基本换算是：如果文件偏移 `f` 落在某个 `PT_LOAD` 内，且该段满足：

```text
p_offset <= f < p_offset + p_filesz
```

那么对应的链接地址约为：

```text
link_va = p_vaddr + (f - p_offset)
runtime_va = link_va + load_bias
```

这条公式用于把十六进制编辑器或 QEMU trace 中的文件偏移转成 `addr2line` 能使用的地址。对于固定地址内核，`load_bias` 通常是 0；对于 `ET_DYN`/PIE，必须根据实际映射地址计算。

### 3.3 Section headers：代码和数据如何被链接

```sh
readelf -SW "$ELF"
```

常见节：

| 节 | 典型内容 |
| --- | --- |
| `.text` | 可执行代码 |
| `.rodata` | 只读常量、字符串、vtable 等 |
| `.data` | 已初始化的可写数据 |
| `.bss` | 未初始化、运行时清零的数据 |
| `.symtab` / `.strtab` | 完整符号表及其字符串 |
| `.dynsym` / `.dynstr` | 动态链接使用的符号表 |
| `.rela.dyn`、`.rela.plt` | 重定位记录；具体名称取决于架构和 ABI |
| `.debug_info`、`.debug_line` 等 | DWARF 调试信息 |
| `.eh_frame` | 异常展开信息；当前内核链接脚本会丢弃它 |
| `.riscv.attributes` | RISC-V 属性；当前 RISC-V 内核链接脚本会丢弃它 |

当前内核链接脚本把 `.text.trampoline` 输入节放入 `.text` 输出节，并提供 `strampoline` 等链接符号。因此 `readelf -SW` 未必会单独列出一个名为 `.text.trampoline` 的输出节；需要结合符号和反汇编范围查找。

### 3.4 Symbol table：函数和对象在哪里

```sh
readelf -sW "$ELF"
readelf -sW "$ELF" | rg '(_start|strampoline|__alltraps|__restore|__tlb_refill|trap_handler|switch)'
nm -anC "$ELF" | less
nm -anC "$ELF" | rg '(_start|trap|switch|page|elf)'
```

`readelf -sW` 可以看到：

- 符号值（`Value`）和大小（`Size`）；
- `FUNC`、`OBJECT`、`SECTION`、`NOTYPE` 等类型；
- `LOCAL`、`GLOBAL`、`WEAK` 等绑定；
- `UND` 表示未定义、需要其他对象或动态链接解决的符号。

`nm -nC` 的 `-n` 按地址排序，`-C` 尝试 demangle C++/Rust 风格名字。Rust 符号很长时，`nm -anC` 比直接在反汇编中盯着 mangled name 更适合搜索。

注意：

- `.bin` 没有符号表，`nm` 对它没有意义；
- 用户目录中的 `.elf` 也可能已经 stripped，扩展名本身不能证明它有 DWARF；
- `readelf -s` 只看得到仍然保留的符号；
- `release` 并不等于“没有符号”，但优化可能让函数内联、拆分、合并或尺寸变化；
- 若只剩 `.dynsym`，它通常远不如完整 `.symtab` 适合内核级定位。

### 3.5 Relocations、dynamic tags 和 notes

```sh
readelf -rW "$ELF"
readelf -dW "$ELF"
readelf -nW "$ELF"
```

适合回答：

- 这个地址是否还需要运行时重定位？
- 程序是否是 PIE/共享对象？
- 是否存在 `PT_INTERP` 对应的动态链接器？
- 某个全局符号、函数指针或 GOT/PLT 项为什么不是最终地址？
- ELF 中有没有 build-id、ABI note 或架构属性？

静态内核链接完成后可能几乎没有动态信息；这不是命令失败，而是镜像本身没有该类元数据。用户态 `ET_DYN`、动态链接器和 PIE 程序则应重点检查 `.dynamic`、`.dynsym`、`.rela.*` 以及 `PT_INTERP`。

---

## 4. `objdump`/`rust-objdump`：把地址变成指令

### 4.1 仓库推荐的基本命令

```sh
# RISC-V 内核
rust-objdump --arch-name=riscv64 -d \
    os/target/riscv64gc-unknown-none-elf/debug/os > /tmp/cosmos-rv.text.asm

# LoongArch 内核
rust-objdump --arch-name=loongarch64 -d \
    os/target/loongarch64-unknown-none/debug/os > /tmp/cosmos-la.text.asm

# 混合源代码、行号和汇编；需要 ELF 保留 DWARF
rust-objdump --arch-name=riscv64 -d -S -C \
    os/target/riscv64gc-unknown-none-elf/debug/os | less
```

`os/Makefile` 当前默认 `DISASM ?= -x`，所以：

```sh
make -C os disasm ARCH=riscv64 MODE=debug DISASM='-d -S'
make -C os disasm ARCH=loongarch64 MODE=debug DISASM='-d -S'
```

会先编译内核，再调用对应 `rust-objdump`。这个目标会进入 `less`；现场若想保存输出，应直接调用工具重定向到文件，或使用 `disasm-vim` 的生成路径。

### 4.2 `-d`、`-D`、`-S`、`-x` 的区别

| 选项 | 作用 | 适用场景 |
| --- | --- | --- |
| `-d` | 反汇编被标记为可执行的节 | 首选，噪声较少 |
| `-D` | 尝试反汇编所有节 | 怀疑节标志错误、分析特殊汇编或裸数据时使用 |
| `-S` | 把源代码与汇编交错显示 | 需要 DWARF 和可访问的源文件 |
| `-C` | demangle 符号名 | Rust/C++ 符号较长时使用；若版本不支持，查看 `--help` |
| `-x` | 显示 ELF header、program headers、section headers、symbols 等扩展信息 | 快速总览；等价信息可用 `readelf` 分开看 |

对一个已知故障函数，优先缩小范围：

```sh
rust-objdump --arch-name=riscv64 -d -C \
    --disassemble-symbols=some_function "$ELF"

rust-objdump --arch-name=riscv64 -d \
    --start-address=0x80201234 \
    --stop-address=0x80201300 "$ELF"
```

不同 LLVM/GNU 版本对长选项、`--disassemble-symbols` 和架构专用选项的支持程度不同。命令不接受时，先运行：

```sh
rust-objdump --help | rg -n 'disassemble|start-address|stop-address|demangle|section|arch'
```

### 4.3 反汇编时要看什么

看到一条故障指令后，至少同时看：

1. 指令地址和前后 5～15 条指令；
2. 分支目标和调用目标；
3. 访问内存的基址寄存器、偏移和访问宽度；
4. 栈指针变化和保存/恢复寄存器的顺序；
5. 是否处于陷阱入口、页表切换、上下文切换或返回路径；
6. 编译器是否将 Rust 中的一次访问优化成了多条指令、原子指令或函数调用。

**RISC-V 的指令长度不一定都是 4 字节。** 如果启用 `C` 扩展，压缩指令可能是 2 字节；不要看到一个返回地址就机械地减 4。LoongArch 常规指令按 4 字节编码，但异常地址仍应以真实 ELF 反汇编为准。

### 4.4 对裸 `.bin` 的限制

`.bin` 没有 ELF 的地址信息。直接对它运行：

```sh
rust-objdump --arch-name=riscv64 -d kernel.bin
```

可能无法得到正确结果，或者得到从地址 0 开始的“看似合理”的伪反汇编。更可靠的做法是：

- 尽量保留并分析原始 ELF；
- 如果只有裸二进制，先确认它的实际装载地址、是否包含 boot 段、是否被拼接过；
- 使用对应架构的 GNU `objdump` 的 `-b binary`、架构选项和 `--adjust-vma`，并把命令及参数记录下来；
- 不要对一个按多个段拼接、带 padding 或经过压缩的镜像，假设整个文件是单一连续代码段。

例如，只有在对应 GNU 工具明确支持目标架构并且已知加载地址时，RISC-V 裸文件才可以尝试：

```sh
riscv64-unknown-elf-objdump -D -b binary -m riscv:rv64 \
    --adjust-vma=0x80200000 kernel.bin
```

LoongArch 的 `-m` 名称因 binutils 版本不同而可能变化；不要照抄 RISC-V 的 `-m`。优先保留 LoongArch ELF。

---

## 5. `addr2line`：从 PC 找到源文件和行号

### 5.1 基本用法

```sh
ELF=os/target/riscv64gc-unknown-none-elf/debug/os
addr2line -e "$ELF" -f -C 0x80201234

# 一次解析多个地址；-i 可显示内联调用链（工具支持时）
printf '%s\n' 0x80201234 0x80201240 0x80201258 | \
    addr2line -e "$ELF" -f -C -i
```

常用选项：

| 选项 | 作用 |
| --- | --- |
| `-e FILE` | 指定用于解析的 ELF |
| `-f` | 同时显示函数名 |
| `-C` | demangle 符号名 |
| `-i` | 显示内联调用栈（若实现支持） |
| `-p` | 更紧凑、可读的单行格式（若实现支持） |

若 `addr2line` 报不支持架构，换成对应 cross 工具；若输出 `??:0`，按下面顺序排查：

1. ELF 是否选错（内核/用户程序、RV/LA、旧构建/新构建）；
2. 地址是否是 ELF 的链接虚拟地址，而不是文件偏移、物理地址或另一种地址别名；
3. ELF 是否被 strip，是否仍有 `.debug_line`；
4. release 优化是否让函数被内联或删除；
5. 运行时是否对 `ET_DYN` 程序加了 load bias；
6. 源代码路径是否存在，或 DWARF 中记录的是另一台机器的路径。

### 5.2 地址必须处于同一个坐标系

`addr2line` 不接受“随便一个看起来像地址的数字”。它需要的是**与 ELF 符号和 DWARF 一致的链接地址**。

常见地址类型：

| 地址 | 含义 | 能否直接给 `addr2line` |
| --- | --- | --- |
| `sepc`/`ERA` | 异常发生时的 PC | 如果它与对应 ELF 的 VMA 相同，通常可以 |
| `ra`/返回地址 | 调用者返回位置 | 通常可以，但可能落在调用指令之后，需结合反汇编 |
| `stval`/`BADV` | 访问失败的虚拟地址 | 不是代码地址，不能拿去找源行 |
| QEMU trace 的 PC | CPU 当前执行地址 | 需确认是否包含 load bias 或物理/虚拟别名 |
| 文件偏移 | ELF 文件中的 byte offset | 不能直接用，先按 `PT_LOAD` 换算 |
| 裸 `.bin` 偏移 | raw 文件的 byte offset | 不能直接用，需知道镜像布局和加载地址 |

### 5.3 RISC-V 内核的物理入口和高半内核

当前 `os/src/linker.ld` 的关键常量是：

```text
PHYS_BASE     = 0x80200000
KERNEL_OFFSET = 0xffffffc000000000
VIRT_BASE     = KERNEL_OFFSET + PHYS_BASE
ENTRY(_start)
```

链接脚本把最早的 `.boot` 放在物理地址 `0x80200000`，之后的主要内核代码链接到高半 `VIRT_BASE`，并用 `AT(...)` 保持连续的物理加载布局。因此同一个内核启动过程可能出现两类 PC：

- 关闭分页或仍在物理启动段时，PC 接近 `0x80200000`；
- 切换到高半映射后，PC 应接近链接脚本计算出的 `VIRT_BASE + offset`。

实际数值以 `readelf -hW`、`readelf -lW` 和 `nm -n` 为准。不要把 `0x80200000` 的物理加载地址简单加到所有高半 PC 上，也不要把 `stval` 当作代码地址。

### 5.4 LoongArch 内核的物理入口和 cached alias

当前 `os/src/linker-loongarch64.ld` 的关键常量是：

```text
VIRT_BASE       = 0x9000000090000000
PHYS_BASE       = 0x90000000       # 普通 QEMU/direct boot 情况
KERNEL_OFFSET   = VIRT_BASE - PHYS_BASE
ENTRY(_start)
```

LoongArch 的 `.boot` 保持在 `PHYS_BASE`，后续代码通过 `AT(ADDR(...) - KERNEL_OFFSET)` 保持物理加载地址和高位 VMA 的关系。 `os/Makefile` 的 QEMU 参数使用：

```text
-device loader,file=$(KERNEL_ELF),addr=0x90000000
```

Nebula 启动路径可能通过 `NEBULA_BOOT` 改变 `PHYS_BASE`；分析板卡问题时必须以实际使用的链接脚本配置和 `readelf -lW` 为准。

### 5.5 PIE 和 load bias

对于 `ET_EXEC`，很多地址是固定链接地址；对于 `ET_DYN`/PIE，程序通常先被映射到某个基址，再使用：

```text
runtime_pc = link_pc + load_bias
link_pc    = runtime_pc - load_bias
```

`addr2line` 通常应接收 `link_pc`。如果用户程序被装在 `USER_PIE_BASE`，而异常日志记录的是运行时地址，就先减去实际 load bias，再执行：

```sh
addr2line -e user-program.elf -f -C -i 0x<runtime_pc_minus_load_bias>
```

当前 `os/src/mm/elf_loader.rs` 区分 `ET_EXEC`、`ET_DYN`、`PT_INTERP` 和动态链接器；遇到用户程序地址时，先用 `readelf -hW` 看 `Type`，再用 `readelf -lW` 看 `PT_LOAD`/`PT_INTERP`，不要凭扩展名猜测是否需要 bias。

---

## 6. 一套可重复的地址定位流程

假设日志给出：

```text
trap: pc=0xffffffc080234568 badv=0x0000000000000010 cause=load page fault
```

### Step 1：确认构建产物

```sh
ELF=os/target/riscv64gc-unknown-none-elf/debug/os
file "$ELF"
sha256sum "$ELF"
readelf -hW "$ELF" | rg 'Class|Machine|Type|Entry'
```

如果日志来自 release QEMU，不能使用 debug ELF；如果日志来自 LoongArch，也不能继续使用 RISC-V 路径。

### Step 2：确认 PC 是否落在可执行段

```sh
readelf -lW "$ELF"
```

找到满足下面条件的 `PT_LOAD`：

```text
VirtAddr <= pc < VirtAddr + MemSiz
```

并且该段的 `Flg` 包含 `E`。若 PC 不落在任何可执行段：

- 可能选错 ELF；
- 可能 PC 是物理别名而 ELF 给出的是高半 VMA；
- 可能 PC 属于 bootloader；
- 可能栈或寄存器已损坏；
- 可能 QEMU trace、异常框架和日志使用了不同地址定义。

### Step 3：解析函数和源行

```sh
addr2line -e "$ELF" -f -C -i 0xffffffc080234568
nm -anC "$ELF" | rg ' [Tt] | [Ww] ' | less
```

如果知道函数名，可以用：

```sh
readelf -sW "$ELF" | rg 'target_function'
```

对照符号的 `Value` 和 `Size`，确认 PC 是否确实位于该函数范围内。`addr2line` 只给一个源行时，不代表该行只有一条机器指令；优化和宏展开会让多个指令共享源行。

### Step 4：反汇编 PC 邻域

```sh
rust-objdump --arch-name=riscv64 -d -C \
    --start-address=0xffffffc080234500 \
    --stop-address=0xffffffc080234620 "$ELF"
```

检查：

- 故障 PC 是 load、store、间接跳转还是普通算术；
- 访问地址如何由寄存器和立即数组成；
- 分支是否跳到了错误的地址；
- `ra`、`sp`、保存寄存器是否符合函数序言；
- 是否正好处于 `__alltraps`、`__restore`、页表切换或上下文切换代码。

### Step 5：把 `badv/stval` 当作数据地址分析

`badv=0x10` 或 `stval=0x10` 表示指令访问了虚拟地址 `0x10`，它通常暗示空指针加字段偏移，但不能直接对 `0x10` 执行 `addr2line`。要从反汇编中的寄存器值判断是哪一个指针为空，再回到 Rust 代码审查所有权、锁、生命周期和用户指针校验。

### Step 6：记录结论和证据

最小证据应包括：

```text
架构：riscv64 / loongarch64
构建模式：debug / release
ELF 路径和 sha256：...
ELF Machine/Type/Entry：...
故障 PC：...
故障数据地址：...
addr2line：...
符号范围：...
反汇编窗口：...
```

这样可以避免“源文件已经改过，但日志来自旧内核”导致的错误结论。

---

## 7. CosmOS 中最值得优先分析的代码区域

### 7.1 启动入口和链接布局

```sh
readelf -sW "$ELF" | rg '(_start|_start_high|skernel|stext|etext|ekernel)'
nm -anC "$ELF" | rg '(_start|_start_high|skernel|stext|etext|ekernel)'
```

对照：

- `os/src/linker.ld`；
- `os/src/linker-loongarch64.ld`；
- `os/src/arch/riscv/boot.S` 等启动汇编；
- `os/src/arch/loongarch64/` 的入口和页表初始化；
- `os/Makefile` 中 `KERNEL_ENTRY_PA`、`QEMU_BOOT_ARGS` 和 `OBJDUMP`。

重点不是只看 `_start` 的数值，而是确认：ELF 入口、QEMU loader 地址、boot 段 LMA、分页开启后的 VMA 以及第一次跳转目标彼此一致。

### 7.2 陷阱入口、返回和上下文切换

```sh
readelf -sW "$ELF" | rg '(__alltraps|__restore|__trap_from_kernel|__tlb_refill|trap_handler|switch_context)'

rust-objdump --arch-name=riscv64 -d -C "$ELF" | \
    rg -n -C 20 '__alltraps|__restore|switch_context|sret'

rust-objdump --arch-name=loongarch64 -d -C "$ELF" | \
    rg -n -C 20 '__alltraps|__trap_from_kernel|__tlb_refill|ertn'
```

当前代码中：

- RISC-V 共享陷阱帧包含通用寄存器、`sstatus`、`sepc`、内核 hart/栈信息、处理函数地址以及浮点状态；其布局必须与 `trap.S` 的保存/恢复偏移严格一致；
- LoongArch 有普通异常入口和软件管理 TLB refill 入口，`ERA`、`BADV`、`ESTAT` 等 CSR 的意义与 RISC-V 不同；
- 任何手写汇编修改，都应同时检查 Rust `#[repr(C)]` 结构布局、`size_of`/字段偏移、栈对齐以及返回指令。

静态分析的典型问题是：Rust 结构加了一个字段，汇编仍使用旧偏移；或者入口保存了寄存器但恢复顺序错误。反汇编能验证“机器码实际访问了栈上的哪个偏移”，但不能单独证明偏移对应哪个 Rust 字段，后者还要对照源代码和布局断言。

### 7.3 页表、TLB 和地址转换

遇到页错误时，按此顺序看：

1. `sepc`/`ERA` 指向的真实指令；
2. `stval`/`BADV` 的访问地址；
3. 反汇编中访问宽度和读写方向；
4. 当前地址空间、页表根和权限；
5. TLB 刷新、页表切换和陷阱返回前后的顺序。

架构对照：

| 目的 | RISC-V | LoongArch |
| --- | --- | --- |
| 异常 PC | `sepc` | `ERA` |
| 异常原因 | `scause` | `ESTAT` 的 `ecode/esubcode` |
| 错误地址 | `stval` | `BADV` |
| 页表根/地址空间控制 | `satp` | `PGDL/PGDH` 等页表相关 CSR，配合地址窗口和页表配置 |
| 返回异常 | `sret` | `ertn` |
| TLB/页表同步 | `sfence.vma` 等 | `invtlb`、`ibar`、`dbar` 等，具体以当前实现和手册为准 |

### 7.4 ELF 加载器和用户进程

对于用户程序，先看 ELF 的 program headers，再看 `os/src/mm/elf_loader.rs` 如何使用它们：

```sh
APP=user/target/riscv64gc-unknown-none-elf/release/some_app.elf
file "$APP"
readelf -hW "$APP"
readelf -lW "$APP"
readelf -SW "$APP"
readelf -sW "$APP" | rg '(_start|main|__libc_start_main|vdso|dynamic)'
readelf -rW "$APP"
readelf -dW "$APP"
```

审查重点：

- `PT_LOAD` 的 `p_vaddr`、权限、文件大小和内存大小；
- `PT_INTERP` 是否指向当前 rootfs 中存在的动态链接器；
- `ET_EXEC`、静态 PIE、普通 PIE 和动态链接器自身的 `ET_DYN` 行为；
- `phdr_vaddr` 是否位于某个 load segment 内；
- `.bss` 是否在 `p_memsz - p_filesz` 范围内被清零；
- 入口地址是否落在可执行段；
- 发生用户态异常时，使用的是哪个用户 ELF 和哪个 load bias。

对于 `exec`/`fork`/动态链接器问题，`readelf -lW` 的内容通常比 `readelf -SW` 更接近加载器真正需要的输入；不要只因为 `.text` 节存在，就推断运行时一定映射正确。

---

## 8. RISC-V 与 LoongArch 的静态分析差异速查

| 项目 | RISC-V | LoongArch |
| --- | --- | --- |
| Rust target | `riscv64gc-unknown-none-elf` | `loongarch64-unknown-none` |
| 仓库反汇编命令 | `rust-objdump --arch-name=riscv64` | `rust-objdump --arch-name=loongarch64` |
| 普通 QEMU 内核加载 | `KERNEL_BIN`，地址 `0x80200000` | `KERNEL_ELF`，loader 地址 `0x90000000` |
| 链接脚本物理基址 | `PHYS_BASE = 0x80200000` | 普通 QEMU 为 `PHYS_BASE = 0x90000000` |
| 高半/缓存别名 | `VIRT_BASE = KERNEL_OFFSET + PHYS_BASE` | `VIRT_BASE = 0x9000000090000000` |
| 陷阱 PC | `sepc` | `ERA` |
| 错误地址 | `stval` | `BADV` |
| 原因寄存器 | `scause` | `ESTAT` |
| 用户系统调用指令 | `ecall` | `syscall 0` |
| 异常返回 | `sret` | `ertn` |
| 指令宽度 | 基本 32 位，启用 `C` 时也有 16 位压缩指令 | 常规指令 32 位 |
| 反汇编风险 | 把压缩指令误当 4 字节，或误用错误 ABI/扩展 | 用 RISC-V 工具解码，或忽略 cached alias/bootloader |

表中的加载地址是当前仓库 `os/Makefile`/链接脚本的事实基线，不是架构的普遍规则。切换到板卡、bootloader 或新的链接脚本后，重新执行 `readelf -hW/-lW`。

---

## 9. 常见分析场景与命令模板

### 9.1 “某个符号到底链接到哪里？”

```sh
nm -anC "$ELF" | rg 'target_symbol'
readelf -sW "$ELF" | rg 'target_symbol'
```

若二者都找不到：

- 名字可能已被 Rust mangling，先用 `nm -an` 搜索模块名片段；
- 函数可能被内联或优化掉；
- ELF 可能被 strip；
- 符号在另一个 crate/启动器 ELF 中。

### 9.2 “这个地址属于哪个函数？”

```sh
addr2line -e "$ELF" -f -C -i 0x<PC>
nm -anC "$ELF" | less
rust-objdump --arch-name=<riscv64-or-loongarch64> -d -C \
    --start-address=0x<PC_MINUS_WINDOW> \
    --stop-address=0x<PC_PLUS_WINDOW> "$ELF"
```

若符号表中有函数起始地址 `F` 和大小 `N`，先检查：

```text
F <= PC < F + N
```

若 `N=0`，不能只靠大小判断，需要看下一个符号地址和反汇编。

### 9.3 “为什么源代码行和指令对不上？”

```sh
readelf --debug-dump=decodedline "$ELF" | less
addr2line -e "$ELF" -f -C -i 0x<PC>
rust-objdump --arch-name=<arch> -d -S -C "$ELF" | less
```

常见原因：

- release 优化和内联；
- 一个源表达式对应多条指令；
- 指令被重排，故障点附近的源行不是执行顺序的直观表示；
- `PC` 是 call/branch 的返回位置；
- 使用了不匹配的 ELF；
- DWARF 中的源路径在当前机器不存在。

### 9.4 “异常 PC 是返回地址，是否应该减 4？”

不能机械处理。返回地址可能指向调用之后的下一条指令，而 RISC-V 还可能有 2 字节压缩指令。正确做法是：

1. 先对原始地址运行 `addr2line`；
2. 用反汇编确认它前面是 `jal`/间接调用还是普通指令；
3. 再检查 `PC-2`、`PC-4` 等候选位置，但只把它们作为诊断，不要修改日志原值；
4. 结合栈回溯和 `ra` 保存规则判断真实调用点。

LoongArch 的指令通常按 4 字节，但同样不能脱离 ABI、返回语义和实际反汇编盲目减法。

### 9.5 “为什么 QEMU 执行到了不应该执行的地址？”

```sh
readelf -hW "$ELF"
readelf -lW "$ELF"
nm -anC "$ELF" | rg '_start|trap|entry|restore'
rust-objdump --arch-name=<arch> -d -C "$ELF" > /tmp/current.asm
```

按下面的关系检查：

```text
ELF entry
    -> bootloader/QEMU loader address
    -> boot 段物理地址
    -> 开启分页/地址窗口后的跳转目标
    -> 运行时 PC
```

常见根因：

- 把 ELF 当 raw binary，或反过来；
- `-device loader` 地址与链接脚本 `PHYS_BASE` 不一致；
- RISC-V 物理入口和高半 VMA 混用；
- LoongArch 物理地址、cached alias 和 UEFI/direct boot 路径混用；
- QEMU 运行的是旧的 `kernel-rv`/`kernel-la`；
- 反汇编分析的是 debug ELF，而 QEMU 启动的是 release ELF。

### 9.6 “用户程序为什么加载失败？”

```sh
APP=.../some_app.elf
readelf -hW "$APP"
readelf -lW "$APP"
readelf -rW "$APP"
readelf -dW "$APP"
```

然后对照 `os/src/mm/elf_loader.rs` 的检查路径：魔数、ELF type、program header 边界、`PT_LOAD`、`PT_INTERP`、load bias、权限和 BSS 清零。不要只检查 `main` 是否存在；加载器从未直接依赖 Rust 的 `main` 作为 ELF 入口。

---

## 10. 与 QEMU/GDB 的配合

静态工具回答“这段字节和地址在 ELF 中是什么”；GDB/QEMU 回答“运行时寄存器此刻是多少”。二者应使用同一个 ELF。

当前 `os/Makefile` 的调试目标会把：

```text
GDB -ex 'file KERNEL_ELF' ... -ex 'target remote localhost:1234'
```

连接到 QEMU 的 `-s -S`。架构默认值为：

```text
RISC-V    riscv64-unknown-elf-gdb    set arch riscv:rv64
LoongArch loongarch64-unknown-elf-gdb
```

典型配合方式：

```gdb
info registers
x/16i $pc
disassemble /m some_function
info line *0x<pc>
bt
```

现场不一定有可用 GDB，因此要预先生成：

```sh
rust-objdump --arch-name=riscv64 -d -S -C \
    os/target/riscv64gc-unknown-none-elf/debug/os > /tmp/cosmos-rv-debug.asm

rust-objdump --arch-name=loongarch64 -d -S -C \
    os/target/loongarch64-unknown-none/debug/os > /tmp/cosmos-la-debug.asm

nm -anC os/target/riscv64gc-unknown-none-elf/debug/os > /tmp/cosmos-rv-debug.nm
readelf -lW os/target/riscv64gc-unknown-none-elf/debug/os > /tmp/cosmos-rv-debug.phdr
```

不要把 QEMU `-d in_asm` 的日志当成带符号的反汇编。它记录的是运行时执行流，符号和源行仍需用匹配的 ELF 后处理。

---

## 11. 离线前应准备的资料和操作习惯

### 11.1 工具自检脚本思路

比赛前在“断网模拟环境”执行：

```sh
command -v rust-objdump
command -v rust-objcopy
command -v readelf
command -v addr2line
command -v nm
command -v strings
command -v file

rust-objdump --arch-name=riscv64 --version
rust-objdump --arch-name=loongarch64 --version
```

再对两套 debug ELF 各执行一次 `file`、`readelf -hW`、`readelf -lW`、`nm -an` 和 `rust-objdump -d`。工具能启动不代表目标架构选项正确，必须看到实际指令输出。

### 11.2 保留哪些文件

至少保留：

- RV 和 LA 的 debug 内核 ELF；
- RV 和 LA 的 release 内核 ELF；
- 当前比赛镜像对应的 `kernel-rv`/`kernel-la`；
- 用户程序的 `.elf`，不要只保留 `.bin`；
- LoongArch direct bootloader ELF；
- 生成的 `.asm`、`nm`、`readelf -lW` 输出；
- 源码、链接脚本和构建命令的版本记录；
- Rust target、binutils/cargo-binutils、QEMU、GDB 的版本。

如果磁盘空间有限，优先保留 ELF 和源码；`.asm`、`nm`、`readelf` 输出都可以在有工具时重新生成，但现场没有对应工具时，预生成文本很有价值。

### 11.3 版本和来源必须绑定

建议把下面的信息写入每份离线输出文件的开头或旁边的 `.meta` 文件：

```text
git revision: ...
target: riscv64gc-unknown-none-elf / loongarch64-unknown-none
mode: debug / release
command: ...
ELF sha256: ...
rustc: ...
rust-objdump: ...
readelf: ...
```

最危险的静态分析错误不是命令报错，而是命令成功地分析了错误版本的 ELF。

---

## 12. 常见失败现象速查

| 现象 | 优先怀疑 | 处理 |
| --- | --- | --- |
| `file` 显示 `data` | 这是裸 `.bin` | 换回同一构建生成的 ELF |
| `readelf: Not an ELF file` | 文件名/路径错、镜像被截断或是 raw 文件 | `file`、`ls -l`、重新定位构建产物 |
| `objdump` 报 unsupported architecture | 主机工具不支持 RV/LA | 使用 `rust-objdump --arch-name=...` 或对应 cross 工具 |
| 反汇编全是乱码 | 架构参数错、从错误偏移开始、把数据当代码 | 检查 `Machine`、`PT_LOAD` 权限和 ELF 入口 |
| `addr2line` 输出 `??:0` | strip、无 DWARF、地址坐标系错或 ELF 不匹配 | 用 debug ELF，检查 `.debug_line`、load bias 和 hash |
| 函数名找不到 | 符号被删/内联/名字 mangled/在另一个 ELF | `nm -an`、`readelf -sW`、检查启动器和用户 ELF |
| `addr2line` 行号很奇怪 | release 优化或 PC 在返回地址/内联边界 | `-i`，查看 PC 邻域和调用指令 |
| PC 不在任何 `PT_LOAD` | ELF 选错、物理/虚拟地址混用或栈损坏 | 查链接脚本、QEMU loader、bootloader 和运行模式 |
| 符号地址和 QEMU trace 相差固定基址 | PIE/load bias 或高半映射 | 用 `link_pc = runtime_pc - bias` |
| RISC-V 附近地址差 2 字节 | 压缩指令 `C` | 不要固定减 4，查看真实反汇编 |
| LA 分析到启动器函数而不是内核 | PC 属于 `loongarch64-direct-boot` | 对启动器 ELF 重新执行 `addr2line` |

---

## 13. 现场最小检查清单

### 拿到一个地址时

- [ ] 我知道地址来自内核、用户进程、bootloader 还是 QEMU trace。
- [ ] 我知道它是 PC、返回地址、虚拟错误地址还是文件偏移。
- [ ] 我确认架构是 RISC-V 还是 LoongArch。
- [ ] 我选的是同一轮、同一模式、同一 commit 生成的 ELF。
- [ ] `file` 和 `readelf -hW` 的 `Machine/Type/Entry` 正确。
- [ ] `readelf -lW` 能解释该地址属于哪个 `PT_LOAD`。
- [ ] `addr2line -f -C -i` 已执行，并记录了原始输出。
- [ ] `nm -anC` 或 `readelf -sW` 已确认函数范围。
- [ ] 已对 PC 前后窗口反汇编，而不是只看一条指令。
- [ ] 已区分 `stval/BADV` 这种数据地址和 `sepc/ERA` 这种代码地址。

### 改动陷阱/汇编/加载器时

- [ ] Rust 结构布局和汇编偏移仍然一致。
- [ ] RISC-V 是否可能遇到压缩指令；LoongArch 是否走了正确的 CSR/异常返回路径。
- [ ] ELF 的 `PT_LOAD`、入口、权限、BSS 范围和实际加载器行为一致。
- [ ] PIE/`ET_DYN` 的 load bias 已明确。
- [ ] QEMU 实际启动的 ELF/bin 与我分析的文件 hash 一致。

---

## 14. 一页命令速查

```sh
# 身份和 ELF 头
file "$ELF"
readelf -hW "$ELF"

# 运行时装载布局
readelf -lW "$ELF"

# 节、符号、重定位、动态信息、notes
readelf -SW "$ELF"
readelf -sW "$ELF"
readelf -rW "$ELF"
readelf -dW "$ELF"
readelf -nW "$ELF"

# 符号排序和搜索
nm -anC "$ELF" | less
nm -anC "$ELF" | rg 'pattern'

# 源行定位
addr2line -e "$ELF" -f -C -i 0x<pc>

# CosmOS 双架构反汇编
rust-objdump --arch-name=riscv64 -d -S -C "$ELF"
rust-objdump --arch-name=loongarch64 -d -S -C "$ELF"

# 缩小到 PC 附近；选项以 rust-objdump --help 为准
rust-objdump --arch-name=<arch> -d \
    --start-address=0x<begin> --stop-address=0x<end> "$ELF"

# 生成离线文本
rust-objdump --arch-name=<arch> -d -S -C "$ELF" > current.asm
readelf -lW "$ELF" > current.phdr
nm -anC "$ELF" > current.nm
```

---

## 15. 本仓库定位锚点和延伸资料

优先阅读当前仓库中的这些文件：

- `os/Makefile`：目标架构、工具名、内核产物、QEMU 加载地址、GDB 入口；
- `user/Makefile`：用户 ELF、`.bin`、`.asm` 的生成方式；
- `os/src/linker.ld`：RISC-V 的物理基址、高半 VMA、`PT_LOAD` 和启动节；
- `os/src/linker-loongarch64.ld`：LoongArch 的物理地址、cached alias 和启动节；
- `os/src/arch/riscv/`：RISC-V 启动、陷阱、上下文和汇编；
- `os/src/arch/loongarch64/`：LoongArch 启动、异常、TLB 和上下文；
- `os/src/mm/elf_loader.rs`：用户 ELF 的 program header、load bias、解释器和重定位处理；
- `os/src/mm/memory_set.rs`：用户地址空间和 ELF 映射后的页表布局；
- `Makefile`：仓库根目录的 `kernel-rv`、`kernel-la`、QEMU 和镜像包装流程。

联网时可下载为本地 HTML/PDF，断网后查阅：

- GNU Binutils 总文档：<https://sourceware.org/binutils/docs/binutils.html>
- GNU `objdump`：<https://sourceware.org/binutils/docs/binutils/objdump.html>
- GNU `readelf`：<https://sourceware.org/binutils/docs/binutils/readelf.html>
- GNU `addr2line`：<https://sourceware.org/binutils/docs/binutils/addr2line.html>
- System V ABI ELF 规范入口：<https://refspecs.linuxfoundation.org/elf/>
- DWARF 调试标准入口：<https://dwarfstd.org/>

最后再强调一次：**先确认 ELF 身份和地址坐标系，再使用工具；先分析 ELF，再分析 `.bin`；先看 `PT_LOAD` 和符号，再凭反汇猜测。**
