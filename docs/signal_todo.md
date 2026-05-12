# Signal 机制对齐记录

本文记录当前 signal 实现的状态、已知 Linux/RISC-V ABI 偏差，以及后续建议处理顺序。

## 当前状态

- `rt_sigaction` 已改为按 RISC-V Linux 用户 ABI 读取：
  - `handler`
  - `sa_flags`
  - `sa_mask`
- RISC-V 用户态不提供 `SA_RESTORER`，内核新增了固定用户态 trampoline。
- signal handler 返回时会跳到固定 trampoline，执行 `rt_sigreturn` 系统调用。
- `sys_sigreturn` 当前能按内核自定义 `UContext` 恢复寄存器、`sepc` 和 signal mask。
- 这套机制预计能覆盖 BusyBox ash 的普通 `SIGCHLD handler(signum)` 返回路径。

## 已知问题

### sigset_t 位布局不一致

Linux `sigset_t` 中信号 `n` 对应 bit `n - 1`。

当前内核内部 `SignalFlags` 使用信号 `n` 对应 bit `n`，例如：

```text
SIGCHLD = 1 << 17
```

Linux ABI 中 `SIGCHLD=17` 应为：

```text
1 << 16
```

TODO：在 syscall 边界增加转换函数：

```text
linux_sigset -> internal SignalFlags
internal SignalFlags -> linux_sigset
```

需要覆盖：

- `rt_sigaction.sa_mask`
- `rt_sigprocmask`
- `rt_sigsuspend`
- `rt_sigreturn` 中保存/恢复的 mask

### sigsuspend 恢复 mask 时机错误

当前 `sys_sigsuspend` 在返回 `-EINTR` 前恢复旧 mask。

Linux 语义是：临时 mask 应保持到 signal handler 执行期间，并由 `rt_sigreturn` 恢复旧 mask。

TODO：把旧 mask 保存进 sigframe，由 `sys_sigreturn` 统一恢复。

### fatal signal 判断早于用户 handler

当前 trap 返回前先检查 fatal signal，再投递用户 handler。若用户注册了 `SIGSEGV`、`SIGILL`、`SIGABRT` 等 handler，可能仍被直接杀掉。

TODO：fatal 判断需要参考当前 disposition：

- `SIG_DFL` 且默认动作为 fatal：退出进程
- 用户 handler：进入 `handle_signals`
- `SIG_IGN`：清除 pending

### sigframe/ucontext 不是 Linux RISC-V 布局

当前 `UContext`、`MContext`、`SigInfo` 是内核自定义简化布局。

普通 `handler(signum)` 不读取 `ucontext` 时可以工作，但 `SA_SIGINFO` handler 若读取 `siginfo_t` 或 `ucontext_t`，会和 Linux ABI 不一致。

TODO：后续按 Linux RISC-V `rt_sigframe`、`ucontext_t`、`mcontext_t` 布局重建 sigframe。

### SA_RESETHAND 未实现

`SA_RESETHAND` 要求进入 handler 前把该 signal 的 disposition 重置为 `SIG_DFL`。

TODO：在 `handle_signals` 投递 handler 前处理该 flag。

### SA_ONSTACK 未实现

当前定义了 `SA_ONSTACK`，但没有 `sigaltstack` 支持，也不会切换备用 signal stack。

TODO：实现 `sigaltstack` 后再支持该 flag。实现前遇到该 flag 应记录 TODO 或明确忽略策略。

### SA_NOCLDWAIT / SA_NOCLDSTOP 未完整实现

当前子进程退出会向父进程投递 `SIGCHLD`，但未完整处理：

- `SA_NOCLDWAIT`
- `SA_NOCLDSTOP`
- 默认忽略 `SIGCHLD` 时的 zombie 行为差异

TODO：在进程退出和 wait 逻辑中补齐这些语义。

## 建议处理顺序

1. 修复 Linux `sigset_t` 与内部 `SignalFlags` 的位转换。
2. 修复 `sigsuspend`，让旧 mask 由 `rt_sigreturn` 恢复。
3. 调整 fatal signal 判断，避免覆盖用户 handler。
4. 实现 `SA_RESETHAND`。
5. 完善 Linux RISC-V `rt_sigframe/ucontext_t/mcontext_t` 布局。
6. 实现 `sigaltstack` 和 `SA_ONSTACK`。
7. 补齐 `SIGCHLD` 相关的 `SA_NOCLDWAIT/SA_NOCLDSTOP` 语义。

## 当前可接受的临时前提

- 先服务 BusyBox ash / glibc basic 的简单 signal 路径。
- 固定用户态 trampoline 可以先替代完整 ELF vDSO。
- 暂不暴露 `AT_SYSINFO_EHDR`，后续完整 vDSO 再处理。
- `ucontext_t` 暂不保证被用户程序正确解析。
