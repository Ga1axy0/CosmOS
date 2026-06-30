#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成第五章“系统调用入口微优化”A/B 对照图（SVG 版）。

对照：`no_syscall_io_fastpath` 开关关闭（BEFORE，旁路慢路径） vs 开启（AFTER，
优化快路径）。负载 iozone -s 64m，记录尺寸 1/4/64/256 KB。

配色与现有 fs_*_cache.svg 一致：蓝（BEFORE / 关闭入口优化）与绿（AFTER / 开启
入口优化）；字体微软雅黑，文字以矢量路径输出，SVG 自包含。用 base 环境
matplotlib 运行：python3 docs/gen_fs_fastpath_chart.py
"""

import os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager

# ---- 字体：微软雅黑（Windows ttc）----
YAHEI = "/mnt/c/Windows/Fonts/msyh.ttc"
if os.path.exists(YAHEI):
    font_manager.fontManager.addfont(YAHEI)
plt.rcParams["font.sans-serif"] = ["Microsoft YaHei", "DejaVu Sans"]
plt.rcParams["axes.unicode_minus"] = False
plt.rcParams["svg.fonttype"] = "path"

# ---- 配色（与 gen_fs_charts.py 一致）----
BLUE = "#2E6FB0"   # BEFORE（关闭入口优化 / 旁路）
GREEN = "#2E9D5B"  # AFTER（开启入口优化 / 快路径）

OUT = "/home/kyle/OS/xxOS/cosmos_docs/assets"
os.makedirs(OUT, exist_ok=True)

RECORDS = ["1 KB", "4 KB", "64 KB", "256 KB"]

# iozone -s 64m 实测（KB/s）。off = 旁路（BEFORE），on = 优化（AFTER）。
DATA = {
    "顺序读": {
        "off":  [31085, 116790, 375928, 928323],
        "on":   [38306, 144659, 373504, 947943],
    },
    "顺序写": {
        "off":  [25820, 97778, 589278, 948163],
        "on":   [32705, 129447, 624467, 1085534],
    },
    "随机读": {
        "off":  [17412, 24253, 328514, 883867],
        "on":   [21011, 80547, 326111, 880991],
    },
    "随机写": {
        "off":  [16270, 44577, 461833, 918115],
        "on":   [19413, 76076, 502851, 1043283],
    },
}


def label_bars(ax, bars, fmt="{:.0f}", log=True, fontsize=7.5):
    for b in bars:
        h = b.get_height()
        if h <= 0:
            continue
        y = h * 1.12 if log else h * 1.02
        ax.text(b.get_x() + b.get_width() / 2, y, fmt.format(h),
                ha="center", va="bottom", fontsize=fontsize, color="#333")


fig, axes = plt.subplots(2, 2, figsize=(10.2, 7.0))
axes = axes.flatten()
x = list(range(len(RECORDS)))
w = 0.38

for ax, (metric, d) in zip(axes, DATA.items()):
    off = d["off"]
    on = d["on"]
    b1 = ax.bar([i - w / 2 for i in x], off, w, label="关闭入口优化（旁路）", color=BLUE)
    b2 = ax.bar([i + w / 2 for i in x], on, w, label="开启入口优化（快路径）", color=GREEN)
    ax.set_yscale("log")
    ax.set_ylabel("吞吐率 (KB/s，对数刻度)")
    ax.set_title(metric)
    ax.set_xticks(x)
    ax.set_xticklabels(RECORDS, fontsize=9)
    ax.grid(axis="y", linestyle="--", alpha=0.4)
    label_bars(ax, b1)
    label_bars(ax, b2)
    # 在每组上方标注加速比（开启 / 关闭）。
    ymax = max(max(off), max(on))
    for i, (o, n) in enumerate(zip(off, on)):
        ratio = n / o
        ax.text(i, ymax * 1.45, f"×{ratio:.2f}", ha="center", va="bottom",
                fontsize=8.5, color="#b03030", fontweight="bold")
    ax.set_ylim(top=ymax * 2.0)

# 仅在第一个子图放图例，避免重复。
handles, labels = axes[0].get_legend_handles_labels()
fig.legend(handles, labels, loc="upper center", ncol=2, framealpha=0.9,
           bbox_to_anchor=(0.5, 1.0))
fig.suptitle("系统调用入口微优化对 iozone 吞吐的影响（64 MB 文件，KB/s，对数刻度）",
             fontsize=13)
fig.tight_layout(rect=(0, 0, 1, 0.95))
fig.savefig(os.path.join(OUT, "fs_fastpath_iozone.svg"))
plt.close(fig)

print("written:", os.path.join(OUT, "fs_fastpath_iozone.svg"))
