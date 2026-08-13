# QEMU、GDB、反汇编与现场调试速查

本篇的命令以当前 CosmOS 的根 Makefile 和 os/Makefile 为准。先用 make -n 查看实际展开结果；镜像、工具链、QEMU 版本和 bootloader 路径可能由环境变量覆盖。调试的目标不是“看到很多日志”，而是把一次失败缩小到：架构入口、异常寄存器、调用函数、数据结构不变量和最小修复。

## 1. 先确认目标和产物

| 架构 | kernel target | QEMU/GDB | kernel load |
| --- | --- | --- | --- |
| RISC-V | riscv64gc-unknown-none-elf | qemu-system-riscv64 / riscv64-unknown-elf-gdb | 0x80200000 |
| LoongArch | loongarch64-unknown-none | qemu-system-loongarch64 / loongarch64-unknown-elf-gdb | 0x90000000 |

    rustc -Vv
    rustup target list --installed
    qemu-system-riscv64 --version
    qemu-system-loongarch64 --version
    command -v riscv64-unknown-elf-gdb
    command -v loongarch64-unknown-elf-gdb
    git status --short

如果只有一架构工具链，先不要改代码；记录缺失工具并确认评测机提供什么。

## 2. 构建和运行

### 2.1 内核目录的最小命令

RISC-V：

    make -C os kernel ARCH=riscv64 MODE=debug
    make -C os run ARCH=riscv64 MODE=debug
    make -C os run-trace ARCH=riscv64 MODE=debug
    make -C os gdbserver ARCH=riscv64
    make -C os gdbclient ARCH=riscv64

LoongArch：

    make -C os kernel ARCH=loongarch64 MODE=debug
    make -C os bootloader ARCH=loongarch64
    make -C os run ARCH=loongarch64 MODE=debug
    make -C os run-trace ARCH=loongarch64 MODE=debug
    make -C os gdbserver ARCH=loongarch64
    make -C os gdbclient ARCH=loongarch64

注意：os/Makefile 的 debug 目标会启动 tmux；无法使用 tmux 时用 gdbserver/gdbclient 分两个终端。

### 2.2 根 Makefile 的完整运行

    make run-comp-rv
    make run-comp-la
    make fast-run
    make fast-run-la
    make fast-run-trace
    make fast-run-la-trace

这些目标可能依赖预先构建的 kernel、用户程序和磁盘镜像。先执行：

    make -n run-comp-rv
    make -n run-comp-la
    make -n fast-run
    make -n fast-run-la

不要在比赛中盲目执行 clean；它会删除 stamp、磁盘镜像或构建结果。只清理明确确认的目标。

## 3. QEMU 日志和 trace

当前 Makefile 的 run-trace 使用：

    -d int,in_asm -D qemu.log

含义：

- int：记录中断/异常；
- in_asm：记录翻译前执行的 guest 指令；
- -D qemu.log：把日志写入文件，避免串口混在一起。

典型流程：

    make -C os run-trace ARCH=riscv64
    rg -n "exception|interrupt|page|trap|illegal|TLB" qemu.log
    less qemu.log

QEMU log 通常非常大。先取故障时间点前后少量上下文，再对照 sepc/ERA 和反汇编，不要从文件头通读。

LoongArch 的 QEMU 参数还包括：

    -machine virt -cpu la464
    -kernel bootloader/loongarch64-direct/.../loongarch64-direct-boot
    -device loader,file=kernel-la,addr=0x90000000

RISC-V 由 RustSBI/bootloader 和 loader 装载 kernel.bin；两者的“kernel ELF 地址”和“kernel binary 地址”不要混淆。

## 4. GDB 启动与连接

QEMU GDB stub 的标准语义是 -s 开启 localhost:1234，-S 让 CPU 在连接前暂停。CosmOS 的 gdbserver 目标已经组合了这两个参数：

终端一：

    make -C os gdbserver ARCH=riscv64

终端二：

    make -C os gdbclient ARCH=riscv64

LoongArch 只替换 ARCH 和 GDB：

    make -C os gdbserver ARCH=loongarch64
    make -C os gdbclient ARCH=loongarch64

如果手动启动：

    qemu-system-riscv64 ... -s -S
    riscv64-unknown-elf-gdb os/target/riscv64gc-unknown-none-elf/debug/os

在 GDB：

    target remote :1234
    info registers
    x/16i $pc
    x/32gx 0x80200000
    continue

LoongArch 使用对应的 loongarch64-unknown-elf-gdb，架构不匹配会表现为寄存器名称、反汇编或 remote packet 错误。

## 5. GDB 高频命令

    file os/target/.../debug/os
    target remote :1234
    set pagination off
    info registers
    p/x $pc
    x/20i $pc
    x/32gx $sp
    x/16wx 0x地址
    bt
    frame 0
    info locals
    disassemble /m 函数名
    break 函数名
    hbreak *0x地址
    watch 变量
    awatch 变量
    stepi
    nexti
    continue
    delete
    detach

RISC-V 重点：

    p/x $sepc
    p/x $scause
    p/x $stval
    p/x $sstatus
    p/x $satp

具体 GDB 是否把 CSR 暴露为伪寄存器取决于版本；如果不可用，以 CosmOS trap 日志为准，或在 Rust/汇编中显式打印。

LoongArch 重点通常从日志看 ERA、BADV、ESTAT、ECFG、PGDL、BADI；GDB 对 CSR 的名称支持也依赖版本。先断在 Rust 的 trap handler 或汇编符号，再检查普通寄存器和内存中的 frame。

## 6. 地址定位

遇到日志中的 PC：

    rg -n "sepc|ERA|pc|fault" serial.log qemu.log
    rust-addr2line -e os/target/.../debug/os -f -C 0x地址
    rust-objdump -S --arch-name=riscv64 os/target/.../debug/os | less

LoongArch：

    rust-addr2line -e os/target/loongarch64-unknown-none/debug/os -f -C 0x地址
    rust-objdump -S --arch-name=loongarch64 os/target/loongarch64-unknown-none/debug/os

如果地址是 kernel virtual address、trampoline 或经过 link/load bias 的用户 ELF 地址，先确认它属于哪个 ELF 和哪个地址空间。用户 PIE 的 PC 可能需要减 load bias；内核 PC 通常直接查内核 ELF。

常见误区：

- 使用 release ELF 给 debug QEMU 做符号解析；
- 用 stripped .bin 查符号；
- 把用户 sepc 当内核地址；
- 只看 fault address，不看 faulting instruction；
- 没有根据架构/目标目录选择正确的 ELF。

## 7. 从异常日志到根因

### 7.1 RISC-V page fault

    读取 scause：load/store/instruction page fault
    → stval 是 fault VA，sepc 是 fault instruction
    → 打印当前 pid/tid、satp/ASID、VMA、PTE flags
    → 对照访问类型与 PTE 的 R/W/X/U
    → 检查是否是 lazy allocation/file mapping/COW/TLB stale

### 7.2 LoongArch page fault/refill

    读取 ESTAT 的 ECODE/ESUBCODE
    → BADV 是 fault VA，ERA 是指令地址
    → 先排除 TLBRENTRY/PWCL/PGDL/refill 循环
    → 再检查 PTE bits、PTE_P/V/W/D/A、GNR/GNX、PLV
    → 检查 invtlb/dbar/ibar 和页生命周期

### 7.3 syscall 返回错误

    用户 wrapper
    → trap frame 的 syscall number/args
    → os/src/trap/mod.rs
    → os/src/syscall/mod.rs 和子模块
    → 用户指针翻译/页 fault
    → 设置 a0 返回值、更新 PC、restore/ertn/sret

打印 syscall id、六个参数、返回值、pid/tid、架构和 PC；注意负 errno 在 Rust 中常是 isize，而 trap frame 保存槽是 usize。

## 8. 早期启动卡死

按顺序判断：

1. 串口是否有任何输出？没有则看 entry、链接地址、栈、BSS 清零。
2. 是否打印 bootstrap hart？没有则看 hart id、架构入口和 bootstrap election。
3. 卡在 memory？看 bootinfo、frame allocator、页表根、TLB/DMW。
4. 卡在 devices/network/fs？临时关闭一个子系统或增加阶段日志。
5. 卡在 scheduler？看 initproc、run queue、timer、中断 enable。
6. 多 hart 才卡？用 SMP=1 验证，再看 BOOT_DONE、IPI 和每 hart 页表。

日志建议包含固定阶段标签：

    [boot][memory] ...
    [boot][trap] ...
    [boot][task] ...
    [boot][fs] ...

不要在早期尚未初始化的 logger、heap、锁或文件系统路径中调用依赖它们的调试代码。

## 9. 死锁与 livelock

表现：

- QEMU 仍运行但串口无输出；
- 一个 hart 100% 自旋，其他 hart停在等待；
- GDB 的 PC 长时间位于 lock()/compare_exchange 循环；
- SMP=1 正常，SMP>1 卡住。

处理：

    Ctrl-C
    info threads
    thread apply all bt
    info registers
    x/12i $pc

再记录每把锁的获取前/获取后日志、hart、pid、锁顺序。不要直接加大 timeout；先确认是否丢失唤醒、持自旋锁睡眠或 TLB shootdown 等待者未被 IPI。

## 10. 反汇编和汇编 ABI

修改 trap.S/switch.S 后必须检查：

- 保存/恢复寄存器集合；
- 栈指针对齐；
- Rust frame 字段偏移；
- clobber 和输入输出寄存器；
- noreturn/ertn/sret 路径；
- RISC-V 的 sscratch/satp 与 LoongArch 的 CSR_SAVE/PGDL；
- FP/LSX 高半部是否保存。

    rust-objdump -d --arch-name=riscv64 kernel.elf | less
    rust-objdump -d --arch-name=loongarch64 kernel.elf | less

把 GDB 停下的 PC 附近指令与源代码、trap frame 中保存的 PC 同时看；不要只看 Rust 行号。

## 11. 现场调试工作流

    保存 git status/git diff
    → 记录架构、SMP、镜像、mode、feature
    → 复现一次并保存完整串口日志
    → 找第一次异常，而不是最后一次 panic
    → 打印最小状态：PC、fault addr、cause、pid/tid、root/token、锁/队列
    → 修改一个假设
    → 用 make -C ... check 或最小 build 验证
    → SMP=1/另一架构/压力测试做回归

## 12. 常见失败表

| 现象 | 原因候选 | 快速检查 |
| --- | --- | --- |
| QEMU 命令找不到文件 | target/镜像/bootloader 路径错 | make -n、ls 具体路径 |
| GDB 无符号 | ELF 不匹配或 strip | file、readelf、确认 MODE=debug |
| 连接 GDB 立刻断 | QEMU 已退出/端口被占 | ps、端口、QEMU stderr |
| 断点不命中 | PC 不在该 ELF、地址重定位 | info files、x/i、load bias |
| trap 后重复 fault | trampoline/frame/栈不可访问 | cause、fault VA、当前 root |
| 只在 release 失败 | UB/汇编 clobber/竞态 | debug 对比、sanity log、栈帧 |
| 只在 LoongArch 失败 | CSR、TLB、ABI、LSX | 08 篇逐项对照 |
| 只在 SMP 失败 | lock/order/TLB/IPI/lifetime | 06 篇和 thread apply all bt |

## 13. 现场 checklist

- [ ] 记住两架构的 target、QEMU、GDB、kernel load address。
- [ ] 能用 make -n 确认实际命令，不依赖网络或记忆中的路径。
- [ ] 能启动 gdbserver/gdbclient，查看寄存器、PC、栈和反汇编。
- [ ] 能从 RISC-V/LoongArch 异常寄存器定位到 VMA/PTE/调用链。
- [ ] 能区分 debug ELF、release ELF、stripped binary 和用户 ELF。
- [ ] 每次只改一个层次，保留日志和可回退版本。

## 参考

- QEMU GDB usage：<https://qemu.readthedocs.io/en/master/system/gdb.html>
- QEMU invocation：<https://qemu.readthedocs.io/en/master/system/invocation.html>
- CosmOS：根 Makefile、os/Makefile、os/src/arch/riscv/trap.rs、os/src/arch/loongarch64/trap.rs、os/src/klog.rs。
