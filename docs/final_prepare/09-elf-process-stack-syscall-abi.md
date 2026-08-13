# ELF、进程地址空间、用户栈与系统调用 ABI

这篇把“用户程序从 ELF 文件到第一次运行，再到一次系统调用返回”的完整链路串起来。CosmOS 支持 file-backed/lazy VMA、PIE/static PIE、shebang、线程、vfork 共享地址空间、信号和两种架构的 trap frame。现场改题时，必须从 ABI 边界向下追，不要只改 syscall 分发函数。

## 1. ELF 最小模型

| 结构 | 作用 |
| --- | --- |
| ELF header | magic、class、机器架构、类型、入口点、program header 表 |
| Program header | 运行时需要的 segment，尤其是 PT_LOAD/PT_INTERP/PT_DYNAMIC |
| Section header | 链接/调试信息；加载器通常不依赖它 |
| PT_LOAD | 文件内容映射到进程虚拟地址，含 R/W/X 权限和 memsz/filesz |
| PT_INTERP | 动态链接器路径 |
| PT_DYNAMIC | 动态链接元数据 |

装载 PT_LOAD 的核心不变量：

    p_offset % p_align == p_vaddr % p_align
    filesz <= memsz
    文件区间不溢出
    vaddr + memsz 不溢出
    映射权限来自 PF_R/PF_W/PF_X
    filesz 之外的 memsz 区域必须是零（BSS）

CosmOS 入口是 os/src/mm/elf_loader.rs；当前实现还处理边界页、文件中间完整页的 page-cache-backed mapping、lazy anonymous/BSS 页和部分 PIE relocation。不要用“整个文件读进一个 Vec 再复制”的旧思路覆盖当前路径。

## 2. exec 端到端调用链

    用户 user/src/syscall.rs 的 exec/execve
    → RISC-V ecall 或 LoongArch syscall 0
    → arch TrapContextAbi 读取 syscall number/args
    → os/src/trap/mod.rs
    → os/src/syscall/process.rs
    → 路径、shebang、权限检查
    → task/process.rs::ProcessControlBlock::exec
    → resolve_init_image/load_process_image
    → MemorySet/ElfLoadInfo/elf_loader
    → 新用户 stack、trap context、VMA、auxv
    → 关闭 FD_CLOEXEC，更新 exec metadata
    → 设置用户 PC/SP/arg0/arg1
    → trap return

exec 成功后替换当前进程 image，不创建新 pid；旧地址空间必须在新 image 足以运行、错误可回滚或状态已安全切换后才能释放。并发 exec/exit/多线程进程有 exec_in_progress 和 sibling termination 等状态，修改时先读 task/process.rs 的 guard/锁顺序。

## 3. 地址空间布局

实际边界以 os/src/mm/memory_set.rs、config.rs 和 linker scripts 为准，不要只套 Linux 地址。概念上包括：

    低地址 ELF text/rodata/data
    → BSS/匿名区/heap（brk）
    → mmap 区域、共享内存、文件映射
    → 用户 stack（向低地址增长）
    → trap context/trampoline 特殊映射
    → kernel shared/high/direct mappings（按架构不同）

每个 VMA 至少要记录：

- start/end，且 end 不溢出；
- R/W/X/U 权限；
- anonymous/file-backed/shared/private；
- inode、file offset、映射长度；
- lazy/dirty/COW 等状态；
- 与 unmap/truncate/page-cache/TLB 的关联。

用户页表不能把内核可写页误设 U；内核共享映射必须按架构实现，不能把 RISC-V high-half 规则复制到 LoongArch DMW/heap subtree。

## 4. PT_LOAD 到页的映射

    计算 segment file_start/file_end/mem_end
    → 检查 p_offset/p_vaddr 对齐关系
    → 取第一个/最后一个边界页
    → 对完整文件页建立 file-backed VMA/page-cache 描述
    → 边界页复制有效文件字节
    → filesz 到 memsz 的尾部清零
    → 剩余 BSS 页按匿名 lazy mapping
    → 设置页面权限和 VMA 权限

常见 bug：

- 只复制 filesz，忘了同页 BSS 尾部必须为零；
- 把 p_vaddr 当页对齐值，丢掉页内偏移；
- p_offset 和 vaddr 的对应偏移不一致；
- W/X/U 权限在 PTE 和 VMA 不一致；
- exec 失败后半安装的页表/VMA/文件引用没有回滚；
- file-backed page fault 读到 EOF 仍当作有效数据；
- PIE load bias 加法溢出或对所有 ET_DYN 一律使用同一策略。

## 5. 用户栈 argv/envp/auxv

当前 task/process.rs 中的 init_user_stack_from_strings/init_user_stack 会把字符串和指针写入用户栈，并构造：

    argc
    argv[0..argc]
    NULL
    envp[0..envc]
    NULL
    auxv (type, value)*
    AT_NULL

写栈的一般顺序：

    预先收集字符串字节
    → 从 stack_top 向低地址放字符串
    → 对齐 sp
    → 放 env/argv 指针数组
    → 放 argc 和 auxv
    → 每次 checked_sub/checked_mul
    → 将最终 sp 写进 TrapContext

必须保证：

- 每个指针都指向当前进程用户空间；
- 字符串包含 NUL，长度和拷贝范围合法；
- sp 按目标 ABI 对齐；
- AT_PAGESZ 等 auxv 值与当前 PAGE_SIZE 一致；
- exec 后旧 argv/envp 不留在已回收页；
- 用户 libc 对栈布局的期待与内核构造一致。

## 6. 用户 trap frame 和系统调用 ABI

### 6.1 RISC-V

| 语义 | 寄存器 |
| --- | --- |
| number | a7/x17 |
| args | a0..a5/x10..x15 |
| return | a0/x10 |
| PC | sepc |
| SP | sp/x2 |
| TLS | tp/x4 |
| instruction | ecall，4 bytes |

入口保存到 RiscvTrapContextFrame；返回前设置 a0 和 sepc，再由 __restore/sret。当前 trampoline 普通路径保持当前 process root，真正 task/address-space switch 时换 satp。

### 6.2 LoongArch

| 语义 | 当前实现 |
| --- | --- |
| number | TrapContext 中的 a7 对应槽 |
| args/return | a0..a5/a0 |
| PC | ERA |
| instruction | syscall 0，4 bytes |
| return | ertn |
| page root | PGDL token |

细节在 os/src/arch/loongarch64/trap.rs 的 LoongArchTrapContextAbi/LoongArchSyscallAbi。clone 参数排列和 signal/mcontext 布局要按当前 musl ABI，不要套 RISC-V。

## 7. 用户指针与 Pod

系统调用参数中的 usize 只是用户虚拟地址数字。安全读取必须：

    检查 ptr/len 溢出和用户范围
    → 按当前进程 page table 翻译每页
    → 检查 Read/Write/Exec 权限
    → 必要时处理 page fault
    → 转成多个 kernel-visible buffer
    → 复制/操作
    → 写回前再次确认语义和长度

使用 os/src/syscall/utils.rs 的 translated_byte_buffer_with_access、translated_process_byte_buffer_with_access、translated_str、read_pod_from_user、write_pod_to_user 等函数。

Pod 只能给布局固定、按任意字节读取安全的纯数据类型；不能给含 Vec/String/Arc/引用的对象。写用户结构体前确认 repr(C)、字段宽度、对齐、padding、架构 ABI 和信息泄漏。

## 8. fork/clone/vfork

当前 task/process.rs 的 clone_process 可能处理：

- 复制/共享 MemorySet；
- parent/child trap frame；
- child 的 syscall 返回值设为 0；
- file descriptor table 和 cwd；
- TLS、parent_tid/child_tid；
- signal/exit signal；
- vfork shared MM 状态；
- TLB shootdown 和延迟回收；
- 线程发布前的 task/PCB 资源。

fork 的最小语义：

    父进程在 syscall 后得到 child pid
    子进程从同一 syscall 返回但 a0=0
    子 PC 是父 sepc 的下一条指令
    子 sp/TLS/资源按 clone flags 决定

不要只复制 PCB：页表 root、trap context、kernel stack、FD 引用、线程组和 TLB 都是可运行状态的一部分。子 task 发布到 scheduler 前必须完成 trap frame 最终修补，否则 SMP 下可能提前运行。

vfork/共享 MM 特别危险：父可能等待 child exec/exit；child exec 成功时要恢复父 trap frame、合并共享 MM 期间变化、释放/延迟释放正确的页表 frame，并唤醒父。参考 vfork_shared_state、release_vfork_parent 和 finish_vfork_shared_mm。

## 9. wait/exit/zombie

exit 通常要：

    记录 exit code/reason
    → 标记 zombie/停止 sibling task
    → 关闭/释放资源
    → 通知 parent/wait queue
    → 唤醒等待者
    → waitpid 读取状态
    → reap PCB/页表/内核栈

不要在“标记退出”后立刻释放仍可能被 trap return、run queue、TLB 或其他 hart 观察的对象。is_zombie、vfork_released、wait_exit_queue、deferred reclaim 是不同状态，不能互换。

## 10. 系统调用闭环

用户 wrapper 在 user/src/syscall.rs，内核编号和 dispatch 在 os/src/syscall/mod.rs：

    user constant/wrapper
    → arch trap
    → os/src/trap/mod.rs
    → os/src/syscall/mod.rs dispatch
    → 专属实现
    → 用户指针/权限/errno
    → TrapContextAbi 写 a0/返回 PC
    → restore/ertn/sret

| 类别 | 典型 syscall | 内核文件 |
| --- | --- | --- |
| 文件 | openat/read/write/close/stat | syscall/fs.rs |
| 内存 | brk/mmap/munmap/madvise | syscall/mman.rs |
| 进程 | exit/clone/execve/waitpid | syscall/process.rs |
| 线程 | thread_create/gettid/waittid | syscall/thread.rs |
| 调度 | yield/scheduler/affinity | syscall/sched.rs |
| 信号 | sigaction/sigprocmask/sigreturn/kill | syscall/signal.rs + signal/ |
| IPC/同步 | pipe/futex/mutex/semaphore/condvar | syscall/sync.rs + sync/ |
| 网络 | socket/bind/connect/sendmsg/recvmsg | syscall/net.rs + net/ |

## 11. 故障表

| 现象 | 优先检查 |
| --- | --- |
| 程序入口立即 page fault | ELF load bias、PT_LOAD 对齐、entry mapping、PC 权限 |
| 数据段值错/BSS 非零 | filesz/memsz、边界页复制、清零范围 |
| 动态程序启动失败 | PT_INTERP、interpreter、auxv、PIE relocation |
| 参数乱码 | 栈字符串 NUL、argv 指针、sp 对齐、用户页写权限 |
| syscall 参数读成 0/坏地址 | ABI 寄存器槽、用户指针翻译、跨页 |
| fork child 不返回/返回父 pid | child trap frame 发布前是否设置 a0=0 |
| exec 后旧地址仍可访问 | MemorySet teardown、TLB flush、旧 root/frame 生命周期 |
| wait 永久睡眠 | zombie 状态发布、入队/唤醒线性化、parent 关系 |
| 只在 LoongArch 失败 | ERA/ertn/PGDL、syscall 编码、mcontext、TLB refill |
| 只在 RISC-V 失败 | sepc/satp/sscratch、Sv39/PTE、sfence.vma |

## 12. 现场调试模板

发生进程异常时记录：

    arch, hart, pid/tid
    user pc/sepc/ERA
    fault va/stval/BADV
    syscall number + a0..a5
    address-space token/satp/PGDL + ASID
    VMA start/end/perm/kind/file offset
    PTE bits/flags/PPN
    current task state/on_rq/on_cpu

然后沿调用链只回答一个问题：这是装载错误、地址转换错误、权限错误、trap ABI 错误、资源生命周期错误，还是并发发布错误？

## 13. 现场 checklist

- [ ] 能解释 PT_LOAD 的 filesz/memsz、权限、对齐、BSS 和 lazy page。
- [ ] 能手写 argv/envp/auxv 栈布局并检查溢出/对齐。
- [ ] 能从 user wrapper 追到 arch trap、syscall dispatch 和 errno。
- [ ] 能区分用户地址、内核地址、物理地址、页号和 token。
- [ ] fork/exec/exit/wait 同时检查 trap frame、FD、页表、TLB、wait queue。
- [ ] 两架构分别确认 syscall instruction、PC CSR、页表 token、返回指令。

## 参考与仓库定位

- ELF specification：<https://refspecs.linuxfoundation.org/elf/elf.pdf>
- CosmOS：os/src/mm/elf_loader.rs、os/src/mm/memory_set.rs、os/src/task/process.rs、os/src/trap、os/src/syscall、user/src/syscall.rs、os/src/linker.ld、os/src/linker-loongarch64.ld。
