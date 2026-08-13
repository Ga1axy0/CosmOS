# CosmOS 项目地图、启动流程与现场改题入口

这篇是现场的总索引。先用它找到“应该从哪里改”，再查对应专题。当前仓库不是一个只含教学进程的最小内核：它包含双架构、SMP、页缓存、网络、信号、线程、文件系统和大量性能/回归 probe。遇到题目时，先沿调用链确认已有不变量，不要从零重写子系统。

## 1. 顶层目录

| 路径 | 责任 | 现场入口 |
| --- | --- | --- |
| os/ | 内核 crate | 内存、trap、任务、syscall、fs、网络、驱动 |
| user/ | 用户库和应用 | syscall wrapper、链接脚本、测试程序 |
| fs/ | 文件系统库/后端 | easyfs/fat32/ext4 适配和块存储 |
| fs-fuse/ | 宿主机打包工具 | 生成用户/文件系统镜像 |
| bootloader/ | 启动器，特别是 LoongArch direct boot | LA kernel 加载 |
| scripts/ | 测试/性能/运行脚本 | 复现和 benchmark |
| docs/ | 现有问题调查 | page cache、syscall latency 等历史结论 |
| vendor/ | 本地依赖 | riscv、smoltcp 等，断网可读 |

内核模块：

    os/src/main.rs
    ├── arch/              架构特有 trap、页表、switch、hart
    ├── hal/               架构/平台 trait 和统一地址/PTE 操作
    ├── platform/          qemu virt、SBI、设备连线
    ├── mm/                frame、heap、page table、VMA、ELF、TLB
    ├── trap/              共用 trap dispatch、syscall entry、返回
    ├── task/              PCB/TCB、fork/exec/exit、用户资源
    ├── sched/             processor、runqueue、调度策略
    ├── syscall/           syscall 编号、用户指针、各类实现
    ├── fs/                inode、page cache、pipe、procfs、tmpfs
    ├── sync/              spin/sleep mutex、semaphore、futex、condvar
    ├── drivers/           virtio、block、net、chardev
    ├── net/               socket、UDP/TCP/Unix/AF_ALG
    └── signal/             signal action、wait、signal delivery

## 2. 构建分层

根 Makefile 负责选择 ARCH、构建用户程序/镜像、调用 os 和 QEMU；os/Makefile 负责内核 target、bootloader、QEMU/GDB。常用目标：

    make user-apps
    make -C user build ARCH=riscv64
    make -C user build ARCH=loongarch64
    make -C os kernel ARCH=riscv64
    make -C os kernel ARCH=loongarch64
    make run-comp-rv
    make run-comp-la
    make gdbserver
    make gdbclient

默认：

- root Makefile TARGET 默认 riscv64gc-unknown-none-elf；
- os/Makefile ARCH 默认 riscv64；
- RISC-V 内核 ELF 在 os/target/riscv64gc-unknown-none-elf/MODE/os；
- LoongArch 内核 ELF 在 os/target/loongarch64-unknown-none/MODE/os；
- 用户目标与链接脚本分别位于 user/src/linker.ld、user/src/linker-loongarch64.ld。

如果只改了 Rust 文件，仍要确认目标 feature、链接脚本、user image 和 kernel image 是否更新；只启动旧镜像会造成“代码改了但行为没变”的假象。

## 3. 启动时序

### 3.1 架构入口到 rust_main

RISC-V：

    os/src/arch/riscv/entry.asm/entry.rs
    → 设置早期栈、hart id、FDT 指针
    → 调用 rust_main(hart_id, fdt_ptr)

LoongArch：

    bootloader/loongarch64-direct
    → loader 把 kernel 放到 0x90000000
    → os/src/arch/loongarch64/entry.S/entry.rs
    → 调用 rust_main

共用入口在 os/src/main.rs：

    hal::init_with_hartid(hart_id)
    → try_claim_bootstrap_hart
    → 赢家 first_hart_main
    → 其他 hart secondary_hart_main

### 3.2 bootstrap hart

当前 first_hart_main 顺序：

    clear_bss()
    → BOOT_BSS_READY.store(0, Release)
    → bootinfo::init(fdt_ptr)
    → trap::init()
    → mm::init()
    → klog::init()
    → detect_hart_count()
    → drivers::init()
    → platform::init()
    → net::init()
    → fs::init()
    → timer::init_realtime_offset_from_rtc()
    → platform::start_secondary_harts()
    → init_local_hart()
    → task::add_initproc()
    → drivers::block::start_workers()
    → fs::start_page_cache_workers()
    → BOOT_DONE.store(true, Release)
    → sched::run_tasks()

顺序本身是重要不变量：mm 之前不能依赖已经初始化的堆/页表；fs 之前不能访问完成挂载的 root；scheduler 之前必须有 initproc 和本地 timer/trap。

### 3.3 secondary hart

    wait_for_bootstrap()
    → mm::activate_kernel_space()
    → init_local_hart()
    → sched::run_tasks()

BOOT_BSS_READY/BOOT_DONE 是原子发布点，不能改成普通 bool。satp/PGDL 是每 hart 状态，bootstrap 已激活页表不代表 secondary 已激活。

## 4. 架构分层地图

共用层通过 os/src/hal/traits.rs 依赖：

- HartId：current、init、interrupt enable/disable、idle/wfi；
- PagingArch：VA/PA 位数、token、PTE、TLB flush、VPN index；
- TrapMachine：cause、fault address、return to user、syscall instruction length；
- TrapContextAbi：frame layout、GPR、PC/SP/TLS/syscall args/return、signal/FP；
- InterruptControl：timer、external、software/IPI；
- SyscallAbi：clone 参数等 ABI 差异。

RISC-V 实现：

    os/src/arch/riscv/mod.rs
    ├── paging.rs  Sv39、satp/ASID、sfence.vma
    ├── trap.rs    scause/stval/sepc、TrapContext、syscall ABI
    ├── trap.S     __alltraps/__restore、FP 保存
    ├── switch.S   上下文切换
    └── hart.rs    tp、sstatus、wfi

LoongArch 实现：

    os/src/arch/loongarch64/mod.rs
    ├── paging.rs  PGDL、PTE、invtlb、hardware walker
    ├── trap.rs    ESTAT/ERA/BADV、TrapContext、syscall ABI
    ├── trap.S     TLB refill、CSR_SAVE、FP/LSX 保存、ertn
    ├── switch.S   上下文切换
    └── hart.rs    CPUID、CRMD、TCFG/TICLR、idle

架构题优先改对应目录和 trait，不要在共用 mm/task 里散落 target_arch 判断。

## 5. 内存管理调用链

### 5.1 初始化

    mm::init
    → frame_allocator::init_frame_allocator
    → heap_allocator::init_heap
    → KERNEL_SPACE.lock().activate()
    → asid::init()
    → init_kernel_heap_mapping
    → init_heap_virtual_window

关键文件：

- address.rs：PhysAddr/PhysPageNum/VirtAddr/VirtPageNum、范围和 direct map；
- frame_allocator.rs：物理页分配、回收、reclaim；
- heap_allocator.rs：内核堆和虚拟 heap window；
- page_table.rs：PageTable、PageTableEntry、翻译、root frame；
- memory_set.rs：MemorySet、VMA、用户/内核空间、映射/回收；
- elf_loader.rs：ELF segment、file-backed/lazy page、PIE relocation；
- asid.rs：地址空间 ID；
- tlb_shootdown.rs：本地/远端 TLB 失效和延迟释放。

### 5.2 page fault

    arch TrapMachine 读 cause/fault address
    → trap/mod.rs 识别用户/内核和访问类型
    → MemorySet/VMA 查找
    → anonymous/file/COW/page-cache fault handler
    → 分配或取得 frame
    → 安装 PTE
    → 本地/远端 TLB shootdown
    → 返回用户重试 faulting instruction

修改 page fault 时同时检查：权限、跨页、文件末尾/BSS、引用计数、页表锁、TLB、失败回滚。

## 6. 任务、进程和调度地图

    用户 trap/syscall
    → os/src/trap/mod.rs
    → os/src/syscall/process.rs 或 thread.rs/sched.rs
    → task/process.rs 的 PCB
    → task/task.rs 的 TCB/TaskUserRes/TrapContext
    → sched/processor.rs、runqueue.rs
    → arch switch.S
    → 下一 task 的 trap context
    → restore → user

PCB 关注地址空间、文件表、信号、父子关系、wait queue、exit/vfork/exec 状态；TCB 关注 kernel stack、trap frame、线程状态、CPU/队列标志和用户资源。

典型操作入口：

- 创建新镜像：ProcessControlBlock::new/spawn；
- fork/clone/vfork：ProcessControlBlock::clone_process；
- exec：ProcessControlBlock::exec；
- exit：syscall/process.rs 和 task/process.rs 的退出/回收；
- wait：wait queue、zombie、parent notification；
- 调度：sched::schedule、run_tasks、processor current task；
- trap context：os/src/trap/context.rs 与架构 TrapContextAbi。

fork/exec 题最容易漏的是：子任务发布前设置返回值、父页表降权/COW、文件描述符引用、TLS、用户栈、trap frame、vfork 共享地址空间和 TLB。

## 7. ELF、用户栈和系统调用地图

ELF：

    execve syscall
    → resolve path/shebang
    → elf_loader::load / load_process_image
    → parse PT_LOAD/PIE/interpreter
    → MemorySet::insert_framed_area/file-backed VMA
    → 映射 trampoline/trap context/user stack
    → init_user_stack_from_strings
    → TrapContext::app_init_context
    → return to user entry

用户栈通常向低地址增长；进程代码在 task/process.rs 初始化 argv、envp、auxv。修改栈布局必须同步用户 libc/musl 期待的 ABI。

系统调用：

    user/src/syscall.rs wrapper
    → ecall（RISC-V）或 syscall 0（LoongArch）
    → arch TrapContextAbi 读 syscall number/args
    → os/src/trap/mod.rs
    → os/src/syscall/mod.rs dispatch
    → 对应子模块
    → translated_* 用户指针检查/复制
    → TrapContextAbi 写 a0/返回 PC
    → restore/ertn/sret

用户指针优先查 os/src/syscall/utils.rs：translated_byte_buffer_with_access、translated_process_byte_buffer_with_access、read_pod_from_user、write_pod_to_user。

## 8. 文件系统和 page cache

    fs::init
    → inode/rootfs/devfs/procfs/sysfs
    → root overlay/real fs
    → drivers block device
    → page cache
    → inode/file description
    → syscall fs.rs

关键文件：

- fs/inode.rs：inode、mount、路径、root/dev/proc/sys；
- fs/page_cache.rs：文件页缓存、fault/readahead/reclaim/writeback；
- fs/pipe.rs：pipe 和 wait queue；
- fs/tmpfs.rs/procfs.rs/sysfs.rs/devfs.rs：虚拟文件系统；
- drivers/block、virtio：块设备；
- fs/ 与 fs-fuse/：后端和镜像。

page cache 修改要同时考虑：文件截断、映射 VMA、脏页、引用计数、用户写权限、TLB、回收和锁顺序。

## 9. 同步和中断边界

os/src/sync：

    spin.rs              短临界区/可能关中断
    mutex.rs             内核互斥
    sleep_mutex.rs       可睡眠临界区
    fs_sleep_mutex.rs    文件系统专用睡眠锁
    semaphore.rs         计数资源
    futex.rs             用户/内核等待
    condvar.rs           条件等待
    deadlock.rs          锁关系/检测

新增锁前记录：锁顺序、能否睡眠、是否允许中断、是否跨 hart、是否会访问用户页/文件页/页表。不要持自旋锁调用可能触发 page fault、I/O、调度或长时间分配的函数。

## 10. 典型改题入口

| 题型 | 先看 | 再看 |
| --- | --- | --- |
| 新 syscall | user/src/syscall.rs、os/src/syscall/mod.rs | arch trap ABI、utils.rs、测试 app |
| 用户指针 | os/src/syscall/utils.rs | memory_set/page fault、errno |
| fork/clone | syscall/process.rs、task/process.rs | task、MM、fd、TLS、TLB |
| exec/ELF | mm/elf_loader.rs、task/process.rs | linker.ld、user stack、interpreter |
| mmap/brk | syscall/mman.rs、memory_set.rs | VMA、page fault、unmap/reclaim |
| 页表/TLB | hal/traits.rs、arch/*/paging.rs | page_table、tlb_shootdown、memory_set |
| trap/中断 | trap/mod.rs、arch/*/trap.rs | trap.S、timer、platform |
| 调度 | sched/processor.rs/runqueue.rs | task/task.rs、switch.S、wait queue |
| 文件系统 | syscall/fs.rs、fs/inode.rs | page_cache、drivers、锁 |
| 网络/socket | syscall/net.rs、net/ | drivers/net、smoltcp、poll |
| 信号 | signal/、arch/*/trap.rs | user ABI、ucontext、wait/exit |
| 性能题 | docs/、perf_probe.rs、scripts/ | 先建立 baseline，再改一条路径 |

## 11. 测试程序入口

用户测试在 user/src/bin：

- fork_reclaim、mmap_test、brk：内存/进程；
- tlb_shootdown_probe、page_cache_link_test：TLB/page cache；
- fp_context_probe、float：FP/上下文；
- sched_fifo_test、sched_cfs_test、time_smp_probe：调度/SMP；
- ppoll_pipe_lost_wakeup、socketpair_*、io_blocking_bench：等待/IPC/阻塞；
- fstest/fstest2/tmpfs_test：文件系统；
- tcp_*、dns_probe、remote_shell：网络；
- initproc、sh、bash：启动和用户环境。

新增回归测试时尽量做成独立 user/src/bin 程序，使用 user/src/syscall.rs 已有 wrapper；失败时输出阶段、返回值、errno、pid/tid 和架构。

## 12. 修改前/后检查表

### 修改前

- [ ] git status/git diff，确认不覆盖已有用户改动。
- [ ] 找到测试入口和当前调用链。
- [ ] 写出数据结构不变量、锁顺序、地址空间/生命周期规则。
- [ ] 确认 RISC-V/LoongArch 是否都必须通过。

### 修改中

- [ ] 先改最小路径；每次只改变一个层次。
- [ ] 借用错误优先改作用域/数据结构，不直接增加 unsafe。
- [ ] 地址/长度/页号都做溢出和范围检查。
- [ ] PTE 改动同步考虑 TLB；共享对象改动同步考虑 SMP。
- [ ] trap frame/ABI/汇编字段同步修改。

### 修改后

- [ ] cargo check/build 目标架构正确。
- [ ] cargo fmt --check/git diff --check。
- [ ] RISC-V 和 LoongArch 至少各做一次 dry-run 或构建。
- [ ] SMP=1 通过后再用 SMP>1。
- [ ] 记录真实命令、日志和未解决假设。

## 13. 一张总调用图

    用户程序
      ↓ syscall/trap
    arch trampoline
      ↓
    os/src/trap
      ├── syscall → os/src/syscall → fs/mm/task/net/sync
      ├── page fault → mm/memory_set/page_cache/elf
      ├── timer → sched/task
      ├── external IRQ → platform/drivers/net/block
      └── signal/return → signal + arch ABI
      ↓
    schedule/context switch
      ├── task/PCB/TCB
      ├── mm token/TLB
      └── arch switch.S + restore
      ↓
    用户继续执行

## 参考

- CosmOS 启动：os/src/main.rs、os/src/arch/*/entry*、os/src/platform
- 统一抽象：os/src/hal/traits.rs
- 进程/内存/系统调用：os/src/task/process.rs、os/src/mm、os/src/syscall、os/src/trap
- 两架构：os/src/arch/riscv、os/src/arch/loongarch64
