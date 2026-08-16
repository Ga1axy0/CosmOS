# BAIS：面向 AI 并行长尾的干扰分散调度

## 1. 一句话介绍

BAIS（Barrier-Aware Interference Spreading）是 CosmOS 内核中的单一调度机制：
当 AI 算子用多个 worker 跑满所有 hart 时，它根据各 worker 的 phase 进度，把可调度
的普通任务和设备 completion 分散到预计更早完成的 worker 所在 hart，避免 OS 干扰
继续拖慢 laggard。BAIS 不减少 AI worker 数量，也不预留 system core。

## 2. 要解决的问题

一个 fork-join AI phase 的完成时间由最慢 worker 决定：

```text
T_phase = max_i(T_worker_i)
T_worker_i = C_i + S_i + I_i
```

- `C_i`：有效计算时间；
- `S_i`：缺页、内存控制、设备控制和系统调用等同步服务时间；
- `I_i`：IRQ、completion worker 和其他 non-AI 任务造成的干扰。

普通负载均衡关注 runnable 数量、利用率和公平性，不知道哪些线程属于同一个 AI
phase，也不知道哪个 worker 会成为 barrier laggard。即使总 OS 开销不大，只要它
不均匀地落在一个 worker 上，就可能被 `max()` 放大成整个算子的长尾。

BAIS 的目标不是把 OS 工作集中到一个保留核，而是在 AI 使用全部 hart 的前提下，
根据 phase 关键路径动态选择对尾延迟伤害更小的执行位置。

## 3. 最终策略

当前代码中只有一个可启用策略，名称统一为 `bais`；`off` 只是关闭开关。最终 BAIS
由四部分组成。

### 3.1 显式 phase 语义

AI runtime 通过 syscall 480 向内核提供少量提示：

| 操作 | 含义 |
|---:|---|
| 1 | 注册 AI PID 和 worker 数量 |
| 2 | 开始一个新 phase |
| 3 | 当前 worker 上报 `0～1000` 的进度 |
| 4 | 当前 worker 已到达 barrier |
| 5 | 注销 AI workload |

同一注册 PID 的线程被识别为 AI worker。独立 I/O 进程、普通用户进程和内核
completion worker 属于 non-AI。比赛版选择显式标注，而不是用进程名、CPU 利用率
或指令流猜测任务类型，保证行为可解释、开销可控。

### 3.2 AI worker 稳定映射

AI worker 按 TID 在 affinity 允许的 hart 集合中稳定映射，并在 idle stealing 时保持
该映射。8 worker/8 hart 时仍然使用全部 8 个 hart：

```text
worker 0 -> hart 0
worker 1 -> hart 1
...
worker 7 -> hart 7
```

这减少迁移、缓存失效和 worker 扎堆，但不隔离任何 hart。llama2.c 测试在首个
forward 的多个 hint 边界记录 OpenMP worker 所在 CPU，96/96 条记录都是正确的
单 hart mask。

### 3.3 预计完成时间放置

对 hart `i` 上的 AI worker，内核记录当前 phase 已执行的 AI runtime `A_i`、进度
`P_i`、是否已经到达 barrier，以及该 hart 上的 OS 干扰。剩余时间估计为：

```text
R_i = A_i * (1000 - P_i) / P_i
```

`P_i=0` 时剩余时间未知；已经到达 barrier 时 `R_i=0`。普通 CFS 任务或 completion
worker 唤醒时，BAIS 优先选择预计更早完成的 worker 所在 hart，而不是继续干扰
laggard。

多个任务同时唤醒时，单纯选择最小 `R_i` 会使它们扎堆。BAIS 在返回放置结果时立即
预留成本：

```text
Score(i) = R_i + reservations_i * E_non_ai
target = argmin_i Score(i)
```

`E_non_ai` 是普通任务执行片段成本的 EWMA。若得分相同，再使用当前 phase 的 non-AI
runtime、IRQ、缺页和控制路径时间作为 interference-debt tie-break。

### 3.4 可调度 completion

仅改变 CFS 任务放置无法移动 hardirq 中的工作。因此 VirtIO top half 只确认事件并
唤醒内核 worker，主要 block/network completion 在可调度线程中执行。block worker
每次最多处理 16 个 completion；仍有工作时主动让出 CPU，使下一段工作可以根据最新
worker 进度重新放置。

QEMU PLIC 的 hard IRQ 在当前实现中仍可能集中到 bootstrap hart。BAIS 能调度的是
下沉后的 completion，不声称已经实现硬中断或 DMA steering。

## 4. 内核接口与观测

`/proc/bais` 只接受：

```text
bais    # 启用最终策略
off     # 关闭 BAIS
reset   # 清空统计
```

读取 `/proc/bais` 可以看到：

- 注册 PID、worker 数量、phase 和 arrived mask；
- AI/non-AI 放置次数及 fallback；
- 每 hart 的 AI、non-AI、IRQ runtime；
- block/network deferred work 及跨 hart 次数；
- worker progress、reservation 和预计剩余时间；
- AI 缺页、内存控制、设备控制和其他 syscall 的时间。

这些数据用于比赛现场解释一次长尾来自哪里，以及 BAIS 把 completion 放到了哪里。

## 5. 测试一：自编 phase/completion workload

### 5.1 测例

受控测例让 8 个持久 worker 执行一个包含 OS 服务的完整 phase：

```text
每个 worker 触发匿名页缺页
    -> 第一段定长整数计算，进度到 500
    -> 等待本轮 pwrite + fsync completion
    -> 第二段定长整数计算，进度到 1000
    -> fork-join 完成
```

completion 位于 phase 关键路径中。测试使用 RISC-V QEMU `virt`、8 个 guest hart、
8 个 worker、300 个测量 round、20 个 warmup round，每个配置运行 3 次。QEMU 固定
到宿主逻辑 CPU `0,2,4,6,8,10,12,14,16,18`，避免同一物理核的 SMT sibling。

实验基线保留 AI worker 稳定映射，但让 non-AI 使用普通放置；BAIS 在此基础上启用
预计完成时间放置和 reservation。因此该组数据测量的是干扰分散机制的增量。

### 5.2 结果

| 配置 | throughput | phase avg | phase P95 | phase P99 | start-skew P99 | prepare P99 | E2E P99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 稳定映射基线 | 158 round/s | 5.66 ms | 6.59 ms | 9.14 ms | 0.96 ms | 0.97 ms | 13.67 ms |
| BAIS | 155 round/s | 5.86 ms | 7.24 ms | 10.56 ms | 0.92 ms | 0.83 ms | 11.30 ms |

BAIS 的完整 phase E2E P99 下降 17.31%，prepare P99 下降 14.26%，start-skew P99
下降 3.97%；代价是 release 后的 phase P99 增加 15.53%，throughput 下降约 1.90%。

结论应限定为：BAIS 改善了包含 dispatch、缺页和 completion 的完整 E2E 尾部，但
还没有稳定改善所有子区间。不能用该结果宣称所有 P95/P99 定义都同步改善。

原始汇总：`results/bais/bais-20260816-141211.csv`。

## 6. 测试二：llama2.c 端到端模型

### 6.1 测例

端到端测试使用 karpathy/llama2.c 的 stories15M Transformer。它是真实模型推理，
同时只依赖 musl、libm 和 OpenMP，适合系统调用支持仍有限的比赛 OS。路径覆盖：

- 8 worker OpenMP matmul；
- 模型文件 `mmap` 和冷态文件缺页；
- fork、pipe、文件写入和 fsync；
- VirtIO block IRQ 与 completion worker；
- token 级 phase/progress hint；
- checksum 校验，确保两种配置生成相同 token 序列。

配置为 32 个 forward，排除第一个 cold forward，每个配置 3 次独立冷启动，并发
128 MiB write + fsync。

### 6.2 结果

| 配置 | TTFT | first-forward | token avg | P95 | P99 | E2E |
|---|---:|---:|---:|---:|---:|---:|
| 稳定映射基线 | 6674.47 ms | 6116.24 ms | 41.14 ms | 50.26 ms | 73.72 ms | 7761.23 ms |
| BAIS | 6137.75 ms | 5637.97 ms | 41.84 ms | 51.99 ms | 55.69 ms | 7255.76 ms |

BAIS 的 TTFT、first-forward 和 E2E 分别下降 8.04%、7.82% 和 6.51%，三个 trial
的方向一致。token avg 增加 1.71%，P95 增加 3.45%。表中 P99 看起来下降 24.46%，
但基线有一个 106.84 ms 单点异常值，且每次只有 31 个热态样本，nearest-rank P99
实际等于最大值，所以不把该 P99 当作收益证据。

关闭独立写入、仅保留模型 mmap 冷缺页时，BAIS 的 TTFT、first-forward 和 E2E
分别下降约 6.00%、6.69% 和 5.40%。这表明收益并非完全由独立 I/O 子进程造成，
但当前 trial 数仍不足以给出更强的统计结论。

原始汇总：`results/llama2c-bais/llama2c-bais-20260816-140053.csv`。

## 7. 比赛展示结论

可以陈述：

1. BAIS 不保留系统核，AI 仍使用 8 worker/8 hart；
2. 它利用 phase 进度预测 worker 完成时间，并分散可调度的 OS/completion 工作；
3. 自编关键路径测例的完整 E2E P99 下降 17.31%；
4. llama2.c 并发 I/O 测试的 TTFT 和 E2E 均值分别下降 8.04% 和 6.51%；
5. 绑核记录证明当前差异不是 worker 意外迁移造成的。

不能陈述：

- BAIS 已稳定降低所有真实模型的热态 P95/P99；
- hard IRQ 或 DMA 本身已经可以任意迁移；
- QEMU TCG 等价于真实 NPU、PCIe 或 DMA 硬件；
- 内核能够自动识别任意 AI 应用；
- 所有 AI workload 都会得到相同幅度的收益。

建议现场表述：

> BAIS 面向 AI fork-join phase，在不预留系统核的前提下，利用 worker progress
> 估计完成时间，把可调度的 OS/completion 工作分散到更早完成的 worker 所在
> hart，避免继续拖慢 laggard。在 8 worker/8 hart 的 CosmOS RISC-V QEMU 中，
> 自编关键路径测例的完整 E2E P99 下降 17.31%；stories15M 推理在并发 I/O 下的
> TTFT 和 E2E 均值分别下降 8.04% 和 6.51%。

## 8. 保留的实现与演示入口

- `os/src/sched/bais.rs`：phase 状态、完成时间预测、reservation 和统计；
- `os/src/sched/runqueue.rs`：任务放置与 AI worker 稳定映射；
- `os/src/trap/mod.rs`：IRQ、缺页和 syscall 服务时间计账；
- `os/src/drivers/block/`：有预算的 block completion worker；
- `os/src/net/`、`os/src/drivers/net/`：可调度 network deferred worker；
- `benchmarks/llama2c/run.c`：真实模型的 phase hint、affinity 和指标输出；
- `scripts/run-llama2c-bais.sh`：最终 `off`/`bais` 端到端演示入口。

历史 synthetic runner、多策略消融入口和废弃策略已从比赛代码中删除。

## 9. 参考论文

以下工作提供了设计启发，但 BAIS 不是其中任何一篇的直接复现：

1. Jeremy Carin 等，[PeeR: First-Class Scheduling for Latency-Critical eBPF Applications](https://www.usenix.org/conference/osdi26/presentation/carin)，OSDI 2026。PeeR 把原本隐藏在不可抢占执行环境中的工作变成一等可调度实体；BAIS 借鉴这一方向，把主要 VirtIO completion 下沉到内核线程。PeeR 面向 eBPF 抢占，BAIS 面向 AI fork-join laggard，问题和策略并不相同。
2. Weihang Shen 等，[XSched: Preemptive Scheduling for Diverse XPUs](https://www.usenix.org/conference/osdi25/presentation/shen-weihang)，OSDI 2025。XSched 说明设备工作也需要显式调度机制和抢占边界；BAIS 当前只做到 CPU 侧 deferred completion 放置，尚未调度真实 XPU/DMA 命令。
3. Biao Sun 等，[Llumnix: Dynamic Scheduling for Large Language Model Serving](https://www.usenix.org/conference/osdi24/presentation/sun-biao)，OSDI 2024。Llumnix 根据运行时状态迁移 LLM 请求以改善负载均衡和尾延迟；BAIS 借鉴“运行时语义优于静态负载”的思路，但作用层次是 OS hart，而不是模型实例。
4. Amey Agrawal 等，[Taming Throughput-Latency Tradeoff in LLM Inference with Sarathi-Serve](https://www.usenix.org/conference/osdi24/presentation/agrawal)，OSDI 2024。Sarathi-Serve 利用 prefill/decode phase 构造低停顿调度；它支持了 BAIS 将 AI 执行看作 phase 的建模方式，但其调度对象是请求和 batch。
5. Wei Zhao 等，[Tally: Non-Intrusive Performance Isolation for Concurrent Deep Learning Workloads](https://arxiv.org/abs/2410.07381)，2024。Tally 通过细粒度 GPU thread-block 调度缓解并发 workload 干扰；BAIS 关注 CPU/OS completion 在并行 worker 间形成的不均匀干扰。
