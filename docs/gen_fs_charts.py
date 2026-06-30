#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成第五章文件子系统缓存性能对比图（SVG 版）。
配色：蓝（BEFORE / 关闭缓存）与绿（AFTER / 启用缓存）；字体：微软雅黑。
文字以矢量路径输出（svg.fonttype=path），SVG 自包含、不依赖渲染端字体。
用 base 环境 matplotlib 运行。
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
# 把文字转成矢量路径，使 SVG 自包含，不依赖渲染端是否安装了微软雅黑。
plt.rcParams["svg.fonttype"] = "path"

# ---- 配色 ----
BLUE = "#2E6FB0"   # BEFORE（关闭该级缓存）
GREEN = "#2E9D5B"  # AFTER（启用该级缓存）

OUT = "/home/kyle/OS/xxOS/cosmos_docs/assets"
os.makedirs(OUT, exist_ok=True)


def label_bars(ax, bars, fmt="{:.0f}", log=False, fontsize=8):
    for b in bars:
        h = b.get_height()
        if h <= 0:
            continue
        y = h * 1.02 if not log else h * 1.15
        ax.text(b.get_x() + b.get_width() / 2, y, fmt.format(h),
                ha="center", va="bottom", fontsize=fontsize, color="#333")


# =====================================================================
# 图 1：Page Cache —— iozone -s 64m -r 4k（KB/s，越高越好）
# =====================================================================
metrics = ["顺序读", "重读", "顺序写", "重写", "fread", "freread", "随机读", "随机写"]
before = [4943, 4963, 2558, 8873, 4647, 5071, 5447, 10543]
after = [111371, 111969, 88171, 98620, 80910, 81058, 16627, 23121]

fig, ax = plt.subplots(figsize=(9.2, 4.3))
x = range(len(metrics))
w = 0.38
b1 = ax.bar([i - w / 2 for i in x], before, w, label="关闭 Page Cache", color=BLUE)
b2 = ax.bar([i + w / 2 for i in x], after, w, label="启用 Page Cache", color=GREEN)
ax.set_yscale("log")
ax.set_ylabel("吞吐率 (KB/s，对数刻度)")
ax.set_title("Page Cache 对 iozone 吞吐的影响（64 MB 文件，4 KB 记录）")
ax.set_xticks(list(x))
ax.set_xticklabels(metrics)
ax.legend(loc="upper left", framealpha=0.9)
ax.grid(axis="y", linestyle="--", alpha=0.4)
label_bars(ax, b1, fmt="{:.0f}", log=True)
label_bars(ax, b2, fmt="{:.0f}", log=True)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "fs_page_cache_iozone.svg"))
plt.close(fig)

# =====================================================================
# 图 2：Block Cache —— 目录创建与遍历（秒，越低越好，对数刻度）
# =====================================================================
cats = ["创建 50×100 文件", "tree 遍历 #1", "tree 遍历 #2", "tree 遍历 #3"]
b_create = [244.812, 15.807, 15.606, 15.963]
a_create = [79.773, 0.358, 0.021, 0.021]

fig, ax = plt.subplots(figsize=(8.6, 4.3))
x = range(len(cats))
w = 0.38
b1 = ax.bar([i - w / 2 for i in x], b_create, w, label="关闭 Block Cache", color=BLUE)
b2 = ax.bar([i + w / 2 for i in x], a_create, w, label="启用 Block Cache", color=GREEN)
ax.set_yscale("log")
ax.set_ylabel("耗时 (s，对数刻度)")
ax.set_title("Block Cache 对目录树创建与遍历的影响")
ax.set_xticks(list(x))
ax.set_xticklabels(cats)
ax.legend(loc="upper right", framealpha=0.9)
ax.grid(axis="y", linestyle="--", alpha=0.4)
label_bars(ax, b1, fmt="{:.3f}", log=True)
label_bars(ax, b2, fmt="{:.3f}", log=True)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "fs_block_cache.svg"))
plt.close(fig)

# =====================================================================
# 图 3：Inode Cache + Dentry Cache —— du -sh /mnt/musl 连续 5 次
# =====================================================================
runs = [1, 2, 3, 4, 5]
du_before = [1.448, 0.985, 1.002, 1.020, 1.029]
du_after = [1.723, 0.291, 0.292, 0.286, 0.309]

fig, ax = plt.subplots(figsize=(7.6, 4.2))
ax.plot(runs, du_before, "-o", color=BLUE, label="关闭 Inode+Dentry Cache")
ax.plot(runs, du_after, "-o", color=GREEN, label="启用 Inode+Dentry Cache")
for xv, yv in zip(runs, du_before):
    ax.annotate(f"{yv:.2f}", (xv, yv), textcoords="offset points",
                xytext=(0, 6), ha="center", fontsize=8, color=BLUE)
for xv, yv in zip(runs, du_after):
    ax.annotate(f"{yv:.2f}", (xv, yv), textcoords="offset points",
                xytext=(0, -12), ha="center", fontsize=8, color=GREEN)
ax.set_xlabel("连续执行次数（第 1 次为冷启动）")
ax.set_ylabel("耗时 (s)")
ax.set_title("Inode + Dentry Cache 对重复目录统计 (du) 的影响")
ax.set_xticks(runs)
ax.set_ylim(0, max(du_before) * 1.18)
ax.legend(loc="upper right", framealpha=0.9)
ax.grid(axis="y", linestyle="--", alpha=0.4)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "fs_inode_dentry_cache.svg"))
plt.close(fig)

# =====================================================================
# 图 4：Stat Cache —— ls -al 大目录连续 5 次
# =====================================================================
ls_before = [1.333, 0.473, 0.480, 0.468, 0.473]
ls_after = [1.338, 0.426, 0.430, 0.422, 0.424]

fig, ax = plt.subplots(figsize=(7.6, 4.2))
ax.plot(runs, ls_before, "-o", color=BLUE, label="关闭 Stat Cache")
ax.plot(runs, ls_after, "-o", color=GREEN, label="启用 Stat Cache")
for xv, yv in zip(runs, ls_before):
    ax.annotate(f"{yv:.2f}", (xv, yv), textcoords="offset points",
                xytext=(0, 6), ha="center", fontsize=8, color=BLUE)
for xv, yv in zip(runs, ls_after):
    ax.annotate(f"{yv:.2f}", (xv, yv), textcoords="offset points",
                xytext=(0, -12), ha="center", fontsize=8, color=GREEN)
ax.set_xlabel("连续执行次数（第 1 次为冷启动）")
ax.set_ylabel("耗时 (s)")
ax.set_title("Stat Cache 对重复列目录 (ls -al) 的影响")
ax.set_xticks(runs)
ax.set_ylim(0, max(ls_before) * 1.18)
ax.legend(loc="upper right", framealpha=0.9)
ax.grid(axis="y", linestyle="--", alpha=0.4)
fig.tight_layout()
fig.savefig(os.path.join(OUT, "fs_stat_cache.svg"))
plt.close(fig)

print("charts written to", OUT)
for f in sorted(os.listdir(OUT)):
    if f.startswith("fs_"):
        print(" ", f)
