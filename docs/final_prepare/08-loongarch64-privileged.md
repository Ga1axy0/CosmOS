# LoongArch64 现场速查：CSR、异常、TLB 与 CosmOS

这篇只讨论当前 CosmOS 的 LoongArch64 路径。LoongArch 的异常入口、CSR、TLB 维护和启动方式与 RISC-V 不是同一套命名/语义，不能把 RISC-V 的 satp、scause、sret、sfence.vma 直接翻译过来。共用进程/页表抽象见 09/11，RISC-V 对照见 07。

## 1. 当前仓库事实

| 项目 | 当前实现 |
| --- | --- |
| kernel target | loongarch64-unknown-none |
| user target | loongarch64-unknown-none |
| QEMU | qemu-system-loongarch64 |
| machine/cpu | -machine virt -cpu la464 |
| kernel load address | 0x90000000（os/Makefile） |
| boot mode | 默认 direct；使用 bootloader/loongarch64-direct |
| paging abstraction | LoongArchPaging，39-bit VA、48-bit PA、三级索引、每级 9 bit |
| root token | 物理 root page address，当前写入 CSR.PGDL |
| 主要文件 | os/src/arch/loongarch64/{trap.rs,trap.S,paging.rs,hart.rs} |
| 平台文件 | os/src/platform/loongarch/qemu_virt、bootloader/loongarch64-direct |

构建配置入口是 os/Makefile 和根 Makefile。ARCH 接受 loongarch64、la64、la；架构分支通过 target_arch = loongarch64 选择。

## 2. 特权和地址转换模式

LoongArch 使用 PLV（Privilege Level）表示特权级，当前内核/用户代码可按 PLV0/用户 PLV3 的概念理解；实际入口状态由 CRMD、PRMD 等 CSR 保存和恢复。地址访问还涉及 direct mapping window（DMW）和 mapped address translation mode：

    用户/内核虚拟地址
    → 若落在 DMW 窗口，按直接映射规则得到物理地址
    → 否则由页表硬件 walker 查询内存中的页表
    → TLB 缓存翻译结果

CosmOS 的内核 text、data、stack 使用 LoongArch 的 DMW 访问路径；可增长的 kernel heap 仍需要共享的低地址页表 subtree。检查页表题时不要假设“所有内核地址都和用户页表一样显式映射”。

## 3. 当前代码使用的关键 CSR

常量定义在 os/src/arch/loongarch64/trap.rs：

| CSR | 编号 | 用途 |
| --- | ---: | --- |
| CRMD | 0x0 | 当前运行模式、IE 等控制位 |
| PRMD | 0x1 | 异常前的 CRMD 状态，ERTN 返回依据 |
| EUEN | 0x2 | FP/LSX 等扩展单元使能 |
| ECFG | 0x4 | 中断使能配置 |
| ESTAT | 0x5 | 中断挂起、异常码/子码 |
| ERA | 0x6 | 异常前 PC |
| BADV | 0x7 | 坏地址/故障虚拟地址 |
| BADI | 0x8 | 某些场景的坏指令信息 |
| EENTRY | 0xc | 普通异常/中断入口 |
| ASID | 0x18 | 地址空间标识 |
| PGDL | 0x19 | 低地址页表根 |
| PWCL/PWCH | 0x1c/0x1d | 硬件 page-walk 配置 |
| STLBPS | 0x1e | STLB page size |
| TLBRENTRY | 0x88 | TLB refill 异常入口 |
| TLBREHI | 0x8e | refill 页大小等信息 |
| CPUID | 0x20 | 当前 CPU/hart id |
| TCFG/TICLR | 0x41/0x44 | timer 配置/清 pending |

常见控制位：

- CRMD.IE：本地中断使能；os/src/arch/loongarch64/hart.rs 使用它实现 disable_irqs/enable_irqs。
- EUEN.FPEN：FP 单元；EUEN.SXEN：LSX 单元。QEMU la464 的 glibc 优化路径可能使用 LSX，关闭 SXEN 会得到 LSXDIS。
- ECFG：timer、software/IPI、external 等中断使能。
- ESTAT：高位/指定字段含 ECODE 和 ESUBCODE；异常处理先读它再解码。

不要凭 RISC-V 经验寻找 SIE、SPIE、SPP 或 scause；这里的返回指令是 ertn，PC 是 ERA，坏地址是 BADV。

## 4. 异常/中断入口

### 4.1 用户 trap 路径

    用户执行 syscall 或发生异常
    → 硬件保存 CRMD 到 PRMD，保存 PC 到 ERA，异常原因进入 ESTAT
    → 跳到 EENTRY 计算出的 trampoline 地址
    → __alltraps 在 CSR_SAVE 中交换用户 sp 与 trap context 指针
    → 保存 ra、tp、通用寄存器、ERA/PRMD、FP、LSX 高半部
    → 取当前 task 的 kernel hart id、kernel sp、trap_handler
    → 进入共用 trap_handler
    → __restore 恢复寄存器和 CSR
    → ertn 返回用户

Rust frame 是 LoongArchTrapContextFrame，布局为：

    r[32], prmd, era, kernel_hartid, kernel_pgdl, kernel_sp,
    trap_handler, f[32], fcsr, fp_high[32]

它与 os/src/arch/loongarch64/trap.S 的字节偏移严格绑定。新增字段时必须同步保存/恢复宏和所有 size/alignment 断言。

### 4.2 kernel trap

__trap_from_kernel 在 trap.S 中分配 256 字节栈帧，保存 ra、a0-a7、t0-t8，调用 trap_from_kernel，再按相同偏移恢复，最后 ertn。内核异常排查时如果返回后寄存器损坏，优先检查这段保存集合和 Rust 函数是否额外使用了未保存的寄存器。

## 5. ESTAT exception code 映射

当前 Rust 代码使用：

| ECODE | 当前 CosmOS 含义 |
| ---: | --- |
| 0x0 | interrupt，结合 ESTAT/ECFG 解码 |
| 0x1 | PIL，load page fault |
| 0x2 | PIS，store page fault |
| 0x3 | PIF，instruction page fault |
| 0x4 | PME，权限/页相关 fault，按 store fault 处理 |
| 0x8 | ADE，地址错误；子码 ADEF/ADEM 区分指令/数据 |
| 0xb | SYS，用户 syscall |
| 0xd | INE，非法指令 |
| 0xf | FPD，FP disabled/unavailable |
| 0x10 | LSXDIS |
| 0x11 | LASXDIS |

Rust 入口是 LoongArchTrapMachine::read_trap_info：读取 ESTAT、ECFG、BADV，拆出 ECODE/ESUBCODE，然后生成共用 TrapCause。未知 cause 会记录 ESTAT、ECFG、ECODE、ESUBCODE、BADV、BADI、ERA、PGDL。

一个 fault 日志至少要带：

    hart, pid/tid, ESTAT, ECFG, ECODE, ESUBCODE,
    ERA, BADV, BADI, PGDL, 当前用户 PC/访问类型

## 6. TLB refill 和硬件 page walker

LoongArch 的 TLB refill 是当前双架构最容易混淆的部分。os/src/arch/loongarch64/trap.S 把 __tlb_refill 放在 .text.trampoline 的 4 KiB 对齐起始处：

    读取硬件选择的 PGD
    → lddir ... 2：root/Dir2 到中间目录
    → lddir ... 1：中间目录到 leaf page table
    → ldpte ... 0/1：加载两个半页 TLB entry
    → tlbfill
    → ertn

set_kernel_trap_entry 会写入：

- EENTRY：内核普通 trap 入口；
- TLBRENTRY：__tlb_refill；
- PWCL：PTbase=12、PTwidth=9、Dir1base=21、Dir1width=9、Dir2base=30、Dir2width=9；
- PWCH=0，表示当前使用三级设置；
- STLBPS/TLBREHI page size=12，即 4 KiB。

如果 TLB refill 一进入就失败，检查 .text.trampoline 的 4 KiB 对齐、TLBRENTRY 地址、PWCL 位字段、页表物理地址是否可被 walker 读取、非叶目录项是否真的是下一级页表物理地址。

## 7. LoongArchPaging 的 PTE 语义

os/src/arch/loongarch64/paging.rs 的关键常量：

| 位/字段 | 含义 |
| --- | --- |
| PTE_V bit 0 | valid |
| PTE_D bit 1 | dirty |
| PTE_PLV_USER bits 2..3 | 用户 PLV 权限 |
| PTE_MAT_CC | cacheable coherent memory type |
| PTE_G bit 6 | global |
| PTE_P bit 7 | physical page present |
| PTE_W bit 8 | writable |
| PTE_A bit 10 | accessed |
| PTE_GNX bit 62 | no execute |
| PTE_GNR bit 61 | no read |

当前抽象：

- PA_BITS=48、VA_BITS=39、PPN_BITS=36；
- root token = root_ppn << 12，activate 时写 CSR.PGDL；
- activate_token 顺序是 dbar 0 → csrwr PGDL → csrwr ASID=0 → invtlb 0x00 → ibar 0；
- flush_tlb 使用 dbar 0、invtlb 0x00、ibar 0；
- make_pte 会自动加 P/V/MAT_CC；W 同时设置 D；U 设置 PLV_USER；不含 R/X 时设置 GNR/GNX；
- directory entry 只放下一级表物理地址，不复用 leaf permission bits；
- normalize_leaf_flags 会保证 A，写页保证 D；
- trap context 使用 R|W，不要给它 X。

与 RISC-V 最大差别：这里 PTE 位布局和硬件 walker 语义完全不同，不能把 RISC-V 的 PPN << 10、V/R/W/X/U bit 直接复制。

## 8. 地址空间 token 与 TLB

当前 LoongArch paging 实现把 PGDL 作为根 token，activate 会把 ASID 清零并全局 invalidation。共用 trait 仍保留 address_space_id/with_address_space_id 接口，但架构实现可以忽略 ASID；读代码时不要假定两个架构的 token 数值可互换。

任务切换时：

    比较当前 PGDL 与新 token
    → 若不同，写 PGDL
    → invtlb
    → 必要的 dbar/ibar
    → 恢复新 trap frame

如果退出/exec 后偶发访问旧页：

1. 打印当前 PGDL、目标 token 和当前 task；
2. 检查新 root 的共享 kernel heap entry；
3. 确认 invtlb 发生在写 PGDL/PTE 后；
4. 确认旧 root/frame 没有在某个 hart 仍使用时 drop。

## 9. Hart、timer、IPI 和扩展状态

os/src/arch/loongarch64/hart.rs：

- LoongArchHartId::current() 读取 CSR.CPUID；
- timer 用 rdtime.d 读时间，用 TCFG/TICLR 设置/清 pending；
- TCFG.InitVal 必须使用 4 的倍数倒计时；
- wait_for_interrupt 使用 idle 0；
- enable_fp 同时打开 EUEN.FPEN 和 EUEN.SXEN。

FP/LSX 状态：

- trap frame 保存 f[32]、fcsr；
- LSX 向量寄存器的低 64 位与 f[] 别名，高 64 位由 fp_high[32] 保存；
- trap.S 使用 vstelm.d/vinsgr2vr.d 保存恢复高半部；
- 只保存 f[] 不保存 fp_high 会导致向量程序跨 syscall/调度后数据损坏。

## 10. LoongArch syscall ABI

当前 LoongArchTrapContextAbi 与 legacy Linux/musl 约定配合：

| 语义 | 当前代码 |
| --- | --- |
| syscall instruction | syscall 0，长度 4 |
| syscall number | trap frame 的寄存器数组中对应 a7 |
| arguments | a0..a5，通过 TrapContextAbi 统一读取 |
| return | a0 |
| PC | ERA；syscall 后前进 4 |
| clone | LoongArchSyscallAbi::decode_clone_args 按 flags, stack, parent_tid, child_tid, tls |

用户 sigreturn trampoline 的 8 字节机器码是 ori a7, zero, 139；syscall 0。不要使用 RISC-V 的 ecall 字节或 sepc 名称。

## 11. QEMU 与构建命令

内核目录：

    make -C os kernel ARCH=loongarch64 MODE=debug
    make -C os bootloader ARCH=loongarch64
    make -C os run ARCH=loongarch64 MODE=debug
    make -C os gdbserver ARCH=loongarch64
    make -C os gdbclient ARCH=loongarch64

根 Makefile 的完整测试运行通常是：

    make run-comp-la
    make fast-run-la
    make fast-run-la-trace

QEMU 关键参数是：

    qemu-system-loongarch64 -machine virt -cpu la464
        -kernel bootloader/loongarch64-direct/.../loongarch64-direct-boot
        -device loader,file=kernel-la,addr=0x90000000
        -nographic -smp 1

路径和镜像名会由 Makefile 变量覆盖；先用 make -n 查看实际展开命令。LoongArch GDB 是 loongarch64-unknown-elf-gdb，不能拿 riscv64-unknown-elf-gdb 连接。

## 12. 与 RISC-V 的快速对照

| 语义 | RISC-V | LoongArch |
| --- | --- | --- |
| 当前 PC | sepc | ERA |
| fault 地址 | stval | BADV |
| cause | scause | ESTAT ECODE/ESUBCODE |
| trap entry | stvec | EENTRY |
| user return | sret | ertn |
| page root | satp | PGDL/PGDH 等 |
| TLB flush | sfence.vma | invtlb，配合 dbar/ibar |
| trap frame 临时 CSR | sscratch | CosmOS 使用 CSR_SAVE |
| timer idle | wfi | idle 0 |
| page fault refill | 软件 trap 处理 | TLBRENTRY + lddir/ldpte/tlbfill |

## 13. 常见故障→检查步骤

| 现象 | 优先检查 |
| --- | --- |
| 开机即异常 | direct bootloader、kernel load address、EENTRY/EENTRY 对齐、DMW |
| TLB refill 循环 | TLBRENTRY 4 KiB 对齐、PWCL、PGDL、目录项物理地址 |
| 普通 page fault 识别错误 | ESTAT ECODE/ESUBCODE 解码，BADV 是否读取 |
| syscall 返回 PC 不对 | ERA 是否加 4，syscall 机器码是否 4 字节 |
| 用户返回后寄存器变坏 | trap.S 保存集合、CSR_SAVE、frame offset、ertn 前恢复顺序 |
| FP 程序 NaN/跨进程污染 | fcsr、f[32]、fp_high[32]、EUEN.FPEN/SXEN |
| 只有 glibc/musl memcpy 失败 | EUEN.SXEN、LSX 高半部保存 |
| 改 PTE 后仍访问旧页 | PGDL/ASID、invtlb、dbar/ibar、远端 TLB shootdown |
| 只有多 hart 失败 | CPUID、IPI enable、每 hart PGDL/TLB、延迟回收 |

## 14. 现场 checklist

- [ ] 能从 ESTAT、ERA、BADV、BADI、PGDL 判断异常类别和地址。
- [ ] 记住普通 trap 用 EENTRY，TLB refill 用 TLBRENTRY。
- [ ] 能写出三级页表的 PWCL 索引和 4 KiB page offset。
- [ ] 能区分 DMW、mapped mode 和 TLB refill。
- [ ] 改 trap frame 时同步检查 trap.S 的保存偏移和 FP/LSX 状态。
- [ ] 改 PTE 时通过 LoongArchPaging，不复制 RISC-V PTE bit。
- [ ] 用 make -n 核对 bootloader、kernel load address、QEMU/GDB 架构。

## 官方资料与仓库定位

- LoongArch 文档总入口：<https://loongson.github.io/LoongArch-Documentation/README-EN.html>
- Volume 1 Basic Architecture：<https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html>
- CosmOS：os/src/arch/loongarch64/trap.rs、trap.S、paging.rs、hart.rs、os/src/platform/loongarch、bootloader/loongarch64-direct。
