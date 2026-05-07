# Page Cache / mmap 当前状态与 TODO

## 已支持

### 1. inode 级统一 PageMapping

- 普通文件现在通过 inode 上唯一的 `PageMapping` 统一管理缓存页。
- `OSInode` 的普通 `read/write/sync` 已经尽量收敛到 `PageMapping`。
- `PageMapping` 已支持：
  - 按文件页号查找缓存页
  - 缺页时分配并装入缓存页
  - 脏页集合管理
  - 基于 page cache 的写回
  - 简化回收

### 2. file-backed mmap 已接入 page cache

- file-backed `mmap` 不再在 `mmap` 系统调用里直接把文件内容拷到用户页。
- `mmap` 现在只登记 VMA 元数据。
- 真正装页发生在 page fault 时：
  - `MAP_SHARED` 直接映射 page cache 页
  - `MAP_PRIVATE` 支持 Linux 风格的首次只读接入 page cache

### 3. MAP_SHARED 第一阶段 dirty 跟踪

- 对 `MAP_SHARED | PROT_WRITE`：
  - 初次读 fault 时先只读映射
  - 第一次写 fault 时触发 write-notify
  - 写 fault 时立即把 page cache 页标脏
- 当前使用 sticky dirty：
  - 只要该页仍被映射，写回后暂不清掉 dirty
  - 避免后续写入绕过脏页通知

### 4. MAP_PRIVATE 第一阶段 Linux-like COW

- 首次读/执行 fault：
  - 直接把 page cache 页以只读方式映射进页表
  - 不立即物化私有页
- 首次写 fault：
  - 分配私有页
  - 从 page cache 页拷贝数据
  - 把 PTE 从 page cache 页切换到私有页

### 5. fork() 私有页 COW

- `fork()` 后：
  - 已经物化到 `data_frames` 的私有页会降成父子共享只读页
  - 后续写入走 COW fault
- 对 file-backed 的 direct cache page：
  - `fork()` 现在也会直接继承现成映射
  - `MAP_PRIVATE` 继承时保持只读
  - `MAP_SHARED` 在 sticky dirty 语义下保留父进程当前 `W` 状态

### 6. file-backed fault 的 EOF/SIGBUS

- 整页落在 EOF 之后时：
  - 不再错误映射成零页
  - 直接返回 `SIGBUS`
- 尾页仍保留 EOF 后补零语义

### 7. 用户态 TLB shootdown 与延迟回收

- 已维护每个用户地址空间当前装载的 hart 掩码。
- 以下用户页表修改路径已经接入地址空间 shootdown：
  - `munmap`
  - `exec`
  - 进程退出
  - `brk` 收缩/归零
  - 线程用户资源释放
  - private COW 物理页替换
  - `mprotect`
  - `fork()` 后父页表降权
- 拆映射/替换物理页时会先把旧页对象放入 `UserReleaseBatch`，再通过 `DeferredUserReclaim::flush_then_release()` 在锁外完成远端 TLB shootdown 后释放。
- deferred 路径的本地 `sfence.vma` 已统一到 `MemorySet::finish_deferred_page_table_edit()`。

### 8. kernel stack deferred recycle

- kernel stack 释放时不立即把页框归还 allocator。
- 释放的 kernel stack VA 区间与页框会进入 deferred 状态。
- 后续复用命中 deferred VA 时，通过 global shootdown 刷新所有 online hart，再统一回收 deferred frame。

### 9. truncate / ftruncate 与 mmap 失效

- `truncate/ftruncate` 缩小时，会先失效相关 file-backed 用户映射，再释放 page cache 页。
- file-backed `mmap` 成功后会登记 inode 到进程的弱引用反向映射。
- `fork()` 继承 file-backed VMA 后会补登记子进程。
- truncate 缩小时会：
  - 查找曾映射该 inode 的进程
  - 扫描进程当前 file-backed VMA
  - 清除新 EOF 之后已经 present 的 PTE
  - 清零已 present 尾页中 EOF 之后的字节
  - 锁外执行 TLB shootdown
  - shootdown 完成后释放旧 page cache/private page 引用
- 未 present 的 EOF 外页不需要 truncate 当场拆 PTE，后续 fault 会按新的文件长度拒绝装页。

### 10. exec() 旧地址空间 teardown

- `exec()` 现在会先把旧 `memory_set` 摘出来，再显式 `recycle_data_pages_deferred()`
- 避免旧的 shared/file-backed 映射绕过 teardown，并保证旧页释放发生在 shootdown 之后。

### 11. 调试日志

- 已在以下关键路径加入 DEBUG 日志：
  - file-backed `mmap/munmap`
  - shared write-notify
  - `MAP_PRIVATE` 首次只读接入与首次写物化
  - `fork()` 的私有页共享/COW
  - TLB shootdown 发起/ack
  - kernel stack deferred recycle
  - truncate 触发的 file-backed 映射失效

## 当前设计中的关键数据结构

### 1. `data_frames`

- 含义：
  - 当前 VMA 已经物化为私有页的映射
- 页对象类型：
  - `PrivatePage`
- 适用场景：
  - 匿名页
  - 用户栈
  - 已经物化后的 `MAP_PRIVATE`
  - `fork()` 后参与 COW 的私有页

### 2. `direct_cache_pages`

- 含义：
  - 当前直接映射到用户页表的 page cache 页
- 适用场景：
  - `MAP_SHARED`
  - 首次只读接入的 `MAP_PRIVATE`
- 是否是 `MAP_SHARED`/`MAP_PRIVATE` 不由这个表本身区分，而是由 `VMA.file.shared` 区分

## 已知限制 / 尚未完成

### 1. shared dirty 仍然是第一阶段实现

- 现在依赖 sticky dirty 保证正确性
- 还没有做精确 dirty 闭环：
  - 写回前清 PTE dirty
  - 写回后重新 write-protect
  - 下一次写再次触发 write-notify

TODO:
- 为 direct cache page 建立反向映射
- 写回前扫描并清理相关 PTE dirty
- 写回后按需重新 write-protect

### 2. TLB shootdown 仍是第一阶段实现

- 当前 address-space shootdown 以 `loaded_user_harts` 为目标集合。
- 该设计依赖当前 trap 语义：用户态进入内核会切到 kernel satp 并执行本地 `sfence.vma`。
- 还没有 ASID，也没有按 VA range 精确 flush。
- `ShootdownKind::AddressSpace` 当前仍退化为本地全量 `sfence.vma`。

TODO:
- 引入 ASID 后重新审查 `loaded_user_harts` 语义。
- 支持按 ASID 或按 VA range 的精确 `sfence.vma`。
- 评估是否需要 Linux 风格的 `mm_cpumask(mm)`，记录曾经装载过该 mm 的 hart。
- 为 shootdown 等待路径增加超时/诊断，避免远端 hart 长时间不 ack 时难以定位。

### 3. truncate / ftruncate 仍是保守实现

- 现在已经支持任意长度 truncate，并接入 page cache 与已 present 用户映射失效。
- 当前 inode 反向映射是 lazy cleanup：
  - `munmap/exec/exit` 不主动注销 registry entry
  - truncate 扫描时通过 `Weak<ProcessControlBlock>` 和当前 VMA 二次确认清理失效项
- registry 粒度是 inode -> process，不是 inode -> VMA/page。

TODO:
- 如有性能需求，把 registry 改成更精确的 inode -> VMA/page 反向映射。
- 更精确地区分 partial tail page 的 MAP_SHARED/MAP_PRIVATE 行为和 SIGBUS 行为。
- 增加 truncate + mmap 并发测试。

### 4. msync / fsync / sync syscall 入口未补齐

- 内核内部已有 `PageMapping::sync()`
- 但用户态还缺正式同步入口

TODO:
- 增加 `fsync(fd)`
- 增加 `msync(addr, len, flags)`
- 视需求增加全局 `sync()`

### 5. `MAP_PRIVATE` 仍不是 Linux 完整形态

- 现在首次读 fault 已经接近 Linux
- 但整体上仍是工程化的第一阶段近似
- 目前 private 页的生命周期管理还比较简化

TODO:
- 评估是否要进一步把 private file 页和匿名页统一到更完整的 COW/rmap 模型

### 6. page cache 仍缺完整文件侧反向映射

- 当前已有一个 inode -> process 的弱引用注册表，足够支持 truncate 的保守失效。
- 还没有 Linux 风格的 `i_mmap`/per-page rmap。

TODO:
- 为 page cache / inode 增加更完整的 file mapping 反向映射。
- 支持 writeback / invalidate 时从文件页精确反查用户映射。
- 为 shared dirty 精确闭环提供 per-page 映射扫描能力。

### 7. reclaim 机制仍是简化版本

- 当前是同步触发的简化 CLOCK/second-chance
- 没有后台 writeback 线程
- 没有更精细的 active/inactive 管理

TODO:
- 视需求增加后台 writeback
- 评估是否需要更细的冷热页管理

### 8. `MAP_SHARED` 的 fork 继承还只是 sticky dirty 前提下的近似

- 现在在 sticky dirty 前提下允许子进程继承父进程当前 `W` 状态
- 这对当前正确性是够的
- 但以后切到精确 dirty 后需要重新审视

TODO:
- 在精确 dirty 方案落地后，重新评估 `fork()` 继承 `MAP_SHARED` 的 PTE 权限策略

## 建议的后续顺序

1. 先补 `fsync/msync`，把“能标脏”接到“能显式刷回”
2. 做 shared dirty 的精确闭环
3. 为 truncate + mmap + fork 增加覆盖 SMP 的测试
4. 引入 ASID/range flush，降低 shootdown 成本
5. 最后评估是否继续往 Linux 的完整 file rmap / anon_vma 方向推进
