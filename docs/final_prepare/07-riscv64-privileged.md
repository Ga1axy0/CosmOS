# RISC-V 64 位现场速查：特权级、陷阱、Sv39 与 TLB

本文只讨论当前 CosmOS 的 `riscv64` 路径，按仓库实际源码给出现场定位、命令和故障排查。进程/ELF 的共用逻辑直接看 `os/src/task`、`os/src/mm` 和 `os/src/syscall`；当前目录中没有把 09/11 当作必需前置资料。LoongArch 不要套用本文 CSR 名称、陷阱返回或 TLB 指令，文末给出不可混用的对照。

## 1. 当前仓库基线

| 项目 | CosmOS 当前实现 |
| --- | --- |
| Rust target | riscv64gc-unknown-none-elf |
| QEMU | qemu-system-riscv64 -machine virt |
| 内核入口物理地址 | 0x80200000（os/Makefile） |
| 地址转换 | os/src/arch/riscv/paging.rs 的 Sv39Paging |
| VA/PA 抽象 | VA 39 位、PA 56 位、三级页表、每级 9 bit index |
| 根 token | satp.MODE=8（Sv39）+ ASID + root PPN |
| trap 入口 | stvec；用户入口映射到 TRAMPOLINE，内核入口 __trap_from_kernel |
| 主要文件 | os/src/arch/riscv/{trap.rs,trap.S,paging.rs,hart.rs} |

os/src/hal/traits.rs 把架构操作抽象成 PagingArch、TrapMachine、HartId、TrapContextAbi 等 trait。共用代码不要直接读写 satp/sstatus，除非确实位于 RISC-V 模块。

## 2. 特权与关键 CSR

现场最常见的是 M/S/U 三层中的 S/U：用户程序运行 U-mode，内核运行 S-mode，QEMU 启动链和 SBI 可能先经过 M-mode。

| CSR | 作用 | 故障时看什么 |
| --- | --- | --- |
| stvec | S-mode trap 入口地址/模式 | 是否指向当前地址空间可执行的 trampoline；Direct/Vectored 是否匹配 |
| sepc | trap 前 PC | 出错指令、ecall 返回点 |
| scause | 异常/中断原因，最高位区分 interrupt | 是 page fault、ecall、timer 还是外部中断 |
| stval | 异常附加值，page fault 通常是 fault VA | 用户非法地址、坏指令信息 |
| sstatus | S-mode 状态，含 SIE/SPIE/SPP/FS | 返回 U/S、是否开中断、FP 状态 |
| sscratch | trap 入口交换临时寄存器 | 用户栈和用户 trap context 指针交换 |
| satp | 地址转换模式、ASID、root PPN | 当前页表 root 是否正确，token 是否含 ASID |
| sie/sip | 中断使能/挂起 | timer/software/external 是否开启/清除 |

sstatus 返回用户的关键逻辑：进入 trap 时硬件保存旧 SIE 到 SPIE、旧特权级到 SPP；sret 根据 SPP/SPIE 恢复。CosmOS 创建用户 frame 时在 RiscvTrapContextAbi::new_user_frame 中设置 SPP=User、SIE=false、SPIE=true，不能只改 sepc/sp 而忘记状态位。

## 3. Trap 入口的实际路径

### 3.1 用户到内核

    用户执行 ecall / 访问非法页 / timer interrupt
    → 硬件写 scause、sepc、stval，跳到 stvec
    → TRAMPOLINE 中 __alltraps
    → csrrw sp, sscratch, sp
    → 将 x1、x3、x4、x5..x31 保存到用户页映射的 TrapContext
    → 保存 sstatus、sepc、用户 sp、FP 状态
    → 恢复 kernel hart id、kernel_sp、trap_handler
    → trap_handler / trap::trap_handler
    → 调度、系统调用或 page fault
    → __restore
    → 必要时切换 satp，恢复 frame，sret

对应汇编是 os/src/arch/riscv/trap.S。TrapContext 的字段顺序与汇编偏移是 ABI，改 Rust 结构体必须同步汇编。

### 3.2 当前是否每次 syscall 切换 satp

当前代码注释明确：内核 text、direct map、kernel stack 等在每个进程 root 可访问的共享区域，普通 trap 入口保持当前 root；只有真正的 task/address-space switch 才在 __restore 中安装新 token。早期实验 feature 仍可能有诊断路径，不能只根据某次 benchmark 推断生产路径。

排查 syscall 时先确认：

1. 当前 satp/ASID 是否属于正在运行的进程；
2. kernel mapping 是否真的在该 root 中存在且无 U 权限；
3. trap context、trampoline、kernel stack 的虚拟地址是否在当前 root 可访问；
4. 任务切换后是否执行 sfence.vma 或项目封装的 flush。

## 4. Trap cause 对照

RiscvTrapMachine::read_trap_cause 当前映射：

| RISC-V cause | CosmOS TrapCause |
| --- | --- |
| UserEnvCall | UserSyscall |
| StorePageFault | StorePageFault |
| LoadPageFault | LoadPageFault |
| InstructionPageFault | InstructionPageFault |
| StoreFault/LoadFault/InstructionFault | 对应 fault |
| IllegalInstruction | IllegalInstruction |
| SupervisorTimer | TimerInterrupt |
| SupervisorSoft | SoftwareInterrupt |
| SupervisorExternal | ExternalInterrupt |

发生 page fault 时至少打印：hart、pid/tid、scause、stval、sepc、当前 token、访问类型（读/写/执行）、VMA 和 PTE flags。

## 5. Sv39 页表计算

Sv39 使用 4 KiB page、39 位规范虚拟地址、三级页表，每一级 9 位：

    VA[38:30] → level 0 index
    VA[29:21] → level 1 index
    VA[20:12] → level 2 index
    VA[11:0]  → page offset

os/src/arch/riscv/paging.rs：

- VA_BITS = 39
- PA_BITS = 56
- LEVELS = 3
- INDEX_BITS = 9
- ROOT_TOKEN_MODE = 8
- PTE 中 PPN 从 bit 10 开始

RISC-V canonical 地址规则仍重要：高位必须按 VA 宽度符号扩展。CosmOS 的 normalize_virt_addr_input 会截取低 39 位用于索引；这不是说任意截断后的地址都可以作为合法用户地址，范围检查仍由上层完成。

### 5.1 PTE flags

| flag | 含义 |
| --- | --- |
| V | entry valid |
| R | readable |
| W | writable |
| X | executable |
| U | user accessible |
| G | global translation |
| A | accessed |
| D | dirty |

RISC-V leaf PTE 的 R/W/X 组合有约束；例如 W=1,R=0 是保留/非法组合，代码生成 flags 时不要只按“写就加 W”猜测，跟随 hal::make_pte 和当前 PTEFlags 逻辑。

### 5.2 页表遍历模板

    检查 VA 范围和页对齐
    → 根据 VPN[2]/VPN[1]/VPN[0] 逐级读取 PTE
    → 中间 entry 必须有效且是 non-leaf
    → leaf 检查 V/R/W/X/U
    → PPN + page offset 得到 PA
    → 若 leaf 缺失，交给 page-fault/VMA 逻辑

不要在中间页表 entry 上复用 leaf 权限语义；CosmOS 的架构实现通过 PageTableEntry 和 PagingArch 处理 PPN/flags。

## 6. satp、ASID 与 TLB

satp 布局是 MODE、ASID、PPN。CosmOS 在 Sv39Paging 中用 bit 44 开始的 16 bit 区域放 ASID，并通过 probe_address_space_id_mask 探测实现位数；硬件 WARL 意味着写入值不一定原样保留。

常见操作：

    csrw satp, token
    sfence.vma x0, x0       # 全局/本 hart flush
    sfence.vma x0, asid     # 指定 ASID
    sfence.vma va, asid     # 指定 VA + ASID

当前 os/src/arch/riscv/paging.rs 的实现有三个容易踩的点：

- numeric zero 作为寄存器操作数与使用 x0 通配符不同；不要把值 0 直接写进带 x0 语义的汇编模板；
- 小范围 flush 可以逐页，较大范围当前 QEMU 路径会回退到按 ASID flush；
- satp 是 per-hart，bootstrap hart 激活内核空间不等于 secondary hart 已激活，所以 main.rs 的 secondary 路径调用 mm::activate_kernel_space()。

TLB 排障不能只看 PTE：确认 PTE 写入、flush 发生在哪个 hart、远端 shootdown 是否完成，以及旧物理页是否已回收。

## 7. 系统调用寄存器 ABI

当前 RiscvTrapContextAbi 读写：

| 语义 | 寄存器/保存数组 |
| --- | --- |
| syscall number | a7 / x17 |
| args 0..5 | a0..a5 / x10..x15 |
| return value | a0 / x10 |
| PC | sepc |
| SP | sp / x2 |
| TLS | tp / x4 |
| syscall instruction | ecall，长度 4 |

共用 os/src/trap/mod.rs 先读取 cx.syscall_nr()/cx.syscall_args()，调用 syscall()，再写回返回值。sepc 应在用户 syscall 成功进入内核时前进 4，signal/restart 路径另有规则；排查时看当前 trap_handler，不要在汇编和 Rust 两处重复加 PC。

## 8. Hart、中断和 FP 状态

os/src/arch/riscv/hart.rs：

- hart id 存在 tp，RiscvHartId::current() 用 mv ..., tp 读取；
- sstatus.SIE 控制 supervisor interrupts；
- wfi 用于等待中断；
- FS 不是普通开关：用户第一次使用 F/D 时可能从 Off 触发处理；trap frame 保存/恢复 FP registers 和 fcsr。

如果只在浮点测试中出现 NaN/跨进程污染，检查：

1. trap.S 是否按 FS 状态保存 FP；
2. fcsr 是否也保存，不能只保存 32 个寄存器；
3. sstatus.FS 返回前是否恢复；
4. 调度/阻塞/迁移是否使 live FP 状态与 task backing store 不一致。

## 9. 常见故障表

| 日志/现象 | 优先检查 |
| --- | --- |
| scause=8 | 用户 ecall；检查 a7、sepc 前进和返回值 |
| scause=12/13/15 | instruction/load/store page fault；看 stval、VMA、PTE |
| trap 入口立刻再次 fault | stvec/trampoline 不可执行，或 trap context/栈不在当前页表 |
| 返回用户后立即 fault | sstatus.SPP/SPIE、sepc、sp、token/ASID、用户 PTE |
| 只在切换进程后 fault | 新 root 尚未安装/flush，或共享 kernel mapping 缺失 |
| 只在 SMP 失败 | 远端 TLB shootdown、per-hart CSR、任务/root 生命周期 |
| 只有 release 失败 | UB、汇编 clobber、未对齐、优化依赖的竞态 |

最小 kernel trap 日志可参考 os/src/arch/riscv/trap.rs 的 log_kernel_trap_frame，它会打印 sepc/scause/stval/sstatus/satp。

## 10. 现场命令

在仓库根目录：

    make -C os kernel ARCH=riscv64 MODE=debug
    make -C os run ARCH=riscv64 MODE=debug
    make -C os gdbserver ARCH=riscv64
    make -C os gdbclient ARCH=riscv64

根 Makefile 的 run-comp-rv/fast-run 还会准备完整磁盘；具体目标依赖本机镜像。追踪启动：

    make -C os run-trace ARCH=riscv64
    # 或直接给 QEMU 添加：-d int,in_asm -D qemu.log

## 11. 现场 checklist

- [ ] 能从 scause/stval/sepc/satp 判断异常类别、地址、指令和地址空间。
- [ ] 能手算 Sv39 三层 VPN index 和 page offset。
- [ ] 能写出 satp/sfence.vma 的正确操作，并区分 wildcard 与 numeric zero。
- [ ] 能从 a7/a0..a5 找到系统调用号和参数。
- [ ] 修改 trap frame/汇编时同步检查 Rust 偏移、栈对齐和寄存器保存集合。
- [ ] 用 SMP=1 先定位功能错误，再用多 hart 验证 TLB/调度竞态。

## 官方资料与仓库定位

- RISC-V Privileged Architecture：<https://docs.riscv.org/reference/isa/priv/priv-index.html>
- RISC-V supervisor-level 章节：<https://docs.riscv.org/reference/isa/priv/supervisor.html>
- CosmOS：os/src/arch/riscv/trap.rs、trap.S、paging.rs、hart.rs、os/src/mm/tlb_shootdown.rs、os/src/hal/traits.rs。
