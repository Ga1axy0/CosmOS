# Cache Benchmark Scenarios

本文档描述如何通过 feature flag 独立关闭各项文件系统缓存，以及针对每种缓存的典型性能对比场景。

## Feature Flags

在 `os/Cargo.toml` 中定义了以下 cache bypass features（均为 opt-in 的**关闭**开关）：

| Feature            | 作用                                     | 所属 crate |
|--------------------|------------------------------------------|------------|
| `no_page_cache`    | 关闭 Page Cache，所有文件 I/O 直通后端   | `os`       |
| `no_block_cache`   | 关闭 Block Cache，块设备 I/O 直通磁盘    | `fs`       |
| `no_inode_cache`   | 关闭 Inode Cache，每次创建新的 `Arc<Inode>` | `fs`    |
| `no_dentry_cache`  | 关闭 Dentry Cache，路径解析直通后端      | `fs`       |
| `no_stat_cache`    | 关闭 Stat Cache，stat 属性直通后端       | `fs`       |

### 编译方式

```bash
# 默认编译（所有缓存启用）
make -C os kernel ARCH=riscv64

# 关闭页面缓存
make -C os kernel ARCH=riscv64 \
  EXTRA_FEATURES="--features legacy-vdb-names --features io_perf_counters --features no_page_cache"

# 关闭全部缓存
make -C os kernel ARCH=riscv64 \
  EXTRA_FEATURES="--features legacy-vdb-names --features io_perf_counters \
  --features no_page_cache --features no_block_cache \
  --features no_inode_cache --features no_dentry_cache --features no_stat_cache"
```

### 测试辅助

启用 `io_perf_counters` feature 后，可通过 `/proc/io_perf` 读取各层缓存的命中率统计数据，用于验证缓存是否按预期工作。

---

## 各缓存的典型测试场景

### 1. Page Cache — 重复读取大文件

**核心价值**：缓存文件数据页，避免重复的磁盘 I/O。对于同一文件的多次读取，第二次起完全命中内存。

**测试程序**：

```c
// bench_page_cache.c — 两次顺序读取同一文件
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <sys/time.h>

#define BUF_SIZE (256 * 1024)  // 256KB buffer

static double now_sec(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1000000.0;
}

int main(int argc, char *argv[]) {
    if (argc < 2) { fprintf(stderr, "Usage: %s <file>\n", argv[0]); return 1; }
    char buf[BUF_SIZE];
    double t0, t1;

    // Pass 1: cold read
    int fd = open(argv[1], O_RDONLY);
    t0 = now_sec();
    while (read(fd, buf, BUF_SIZE) > 0) {}
    t1 = now_sec();
    printf("Pass 1 (cold):  %.3f s\n", t1 - t0);
    close(fd);

    // Pass 2: warm re-read — page cache should satisfy this entirely
    fd = open(argv[1], O_RDONLY);
    t0 = now_sec();
    while (read(fd, buf, BUF_SIZE) > 0) {}
    t1 = now_sec();
    printf("Pass 2 (warm):  %.3f s\n", t1 - t0);
    close(fd);

    return 0;
}
```

**等效 Linux 命令**：
```bash
# 创建一个 64MB 测试文件
dd if=/dev/urandom of=/tmp/testfile bs=1M count=64

# 清空 page cache（如果 Linux 内核），然后测试
echo 3 > /proc/sys/vm/drop_caches
time cat /tmp/testfile > /dev/null   # cold
time cat /tmp/testfile > /dev/null   # warm — page cache hit
```

**等效 iozone**：
```bash
iozone -i 1 -s 64m -r 4k -f /tmp/iozone_testfile
# -i 1 = re-read test
```

**期望结果**：

| 配置              | Pass 1 (Cold) | Pass 2 (Warm) | 加速比 |
|-------------------|---------------|---------------|--------|
| 有 Page Cache     | ~磁盘速度     | ~内存速度     | 10-100x |
| `no_page_cache`   | ~磁盘速度     | ~磁盘速度     | ~1x    |

**验证方式**：检查 `/proc/io_perf` 中 `page_cache:` 段的 `read_page_loads` 是否在 warm pass 中归零。

---

### 2. Block Cache — 大规模目录遍历 (find)

**核心价值**：缓存磁盘块级数据（目录块、inode 表块、间接块等），避免同一磁盘块的重复读取。
当遍历深层目录树时，目录块和 inode 表块会被反复访问，Block Cache 直接决定此类操作的性能。

**测试程序**：

```c
// bench_block_cache.c — 多次 find（目录树遍历）
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>

static double now_sec(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1000000.0;
}

int main(void) {
    double t0, t1;

    // Pass 1: cold directory walk
    t0 = now_sec();
    system("find /bench -type f -exec stat {} \\; > /dev/null 2>&1");
    t1 = now_sec();
    printf("Pass 1 (cold):  %.3f s\n", t1 - t0);

    // Pass 2: warm — block cache should hold dir blocks & inode table blocks
    t0 = now_sec();
    system("find /bench -type f -exec stat {} \\; > /dev/null 2>&1");
    t1 = now_sec();
    printf("Pass 2 (warm):  %.3f s\n", t1 - t0);

    return 0;
}
```

**等效 Linux 命令**：
```bash
# 创建深层目录结构
mkdir -p /tmp/dirtest
for i in $(seq 1 50); do mkdir -p "/tmp/dirtest/sub_$i"; for j in $(seq 1 100); do touch "/tmp/dirtest/sub_$i/file_$j"; done; done

# 测试
echo 3 > /proc/sys/vm/drop_caches
time find /tmp/dirtest -type f -exec stat {} \; > /dev/null   # cold
time find /tmp/dirtest -type f -exec stat {} \; > /dev/null   # warm
```

**期望结果**：

Block Cache 对目录树遍历的影响尤为显著：
- **无 Block Cache**：每个目录块和 inode 块都要从磁盘读取，随机 I/O 极多
- **有 Block Cache**：热点目录块和 inode 表驻留内存，warm pass 几乎全是内存访问

| 配置              | Pass 1 (Cold) | Pass 2 (Warm) | 加速比 |
|-------------------|---------------|---------------|--------|
| 有 Block Cache    | ~磁盘速度     | ~内存速度     | 10-50x |
| `no_block_cache`  | ~磁盘速度     | ~磁盘速度     | ~1x    |

**验证方式**：检查 `/proc/io_perf` 中 `block_cache:` 段的 `get_hits` 在 warm pass 中的占比。

---

### 3. Inode Cache — 硬链接多路径访问

**核心价值**：Inode Cache 确保同一 `(fs_id, ino)` 的底层文件对象映射到同一个 `Arc<Inode>`，
使得页缓存、stat 缓存能跨不同路径共享。关闭 Inode Cache 会导致同一文件通过不同路径
（硬链接）访问时产生多份页面缓存，浪费内存并降低性能。

**测试程序**：

```c
// bench_inode_cache.c — 通过多个硬链接访问同一文件
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>

#define FILE_SIZE (32 * 1024 * 1024)  // 32MB
#define BUF_SIZE  (64 * 1024)

static double now_sec(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1000000.0;
}

int main(void) {
    char buf[BUF_SIZE];
    double t0, t1;

    // Setup: create a file and 10 hardlinks
    system("dd if=/dev/urandom of=/tmp/base_file bs=1M count=32 2>/dev/null");
    for (int i = 0; i < 10; i++) {
        char cmd[256];
        snprintf(cmd, sizeof(cmd), "ln /tmp/base_file /tmp/link_%d 2>/dev/null", i);
        system(cmd);
    }

    // Read each hardlink sequentially (same inode, different paths)
    t0 = now_sec();
    for (int i = 0; i < 10; i++) {
        char path[256];
        snprintf(path, sizeof(path), "/tmp/link_%d", i);
        int fd = open(path, O_RDONLY);
        while (read(fd, buf, BUF_SIZE) > 0) {}
        close(fd);
    }
    t1 = now_sec();
    printf("10 hardlinks read:  %.3f s\n", t1 - t0);

    return 0;
}
```

**期望结果**：

| 配置              | 行为                                                     |
|-------------------|----------------------------------------------------------|
| 有 Inode Cache    | 同一 inode 复用同一 `Arc<Inode>`，Page Cache 跨硬链接共享 |
| `no_inode_cache`  | 每个路径创建独立的 `Arc<Inode>`，各自有独立的 Page Cache |

**性能差异**：
- 有 Inode Cache：首次读取填充 Page Cache，后续 9 个硬链接全部命中
- 无 Inode Cache：每个硬链接都需要重新填充各自的 Page Cache（因为没有 inode 级去重）

**验证方式**：检查 `/proc/io_perf` 中 `vfs:` 段，以及 `page_cache:` 的命中率。

---

### 4. Dentry Cache — 重复路径解析

**核心价值**：Dentry Cache 缓存 `(fs_id, parent_ino, name) → child_inode` 的映射，
避免每次路径查找都经过后端文件系统的 `find()` 操作。

**测试程序**：

```c
// bench_dentry_cache.c — 反复打开同一组文件
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <sys/time.h>

#define ITERATIONS 5000

static double now_sec(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1000000.0;
}

int main(void) {
    double t0, t1;

    // Setup: create 100 files
    system("mkdir -p /tmp/dentry_test");
    for (int i = 0; i < 100; i++) {
        char cmd[256];
        snprintf(cmd, sizeof(cmd), "touch /tmp/dentry_test/file_%d", i);
        system(cmd);
    }

    // Repeatedly open the same set of 100 files
    t0 = now_sec();
    for (int iter = 0; iter < ITERATIONS; iter++) {
        for (int i = 0; i < 100; i++) {
            char path[256];
            snprintf(path, sizeof(path), "/tmp/dentry_test/file_%d", i);
            int fd = open(path, O_RDONLY);
            if (fd >= 0) close(fd);
        }
    }
    t1 = now_sec();
    printf("%d iterations × 100 opens:  %.3f s\n", ITERATIONS, t1 - t0);

    return 0;
}
```

**等效 Linux 命令**：
```bash
mkdir -p /tmp/dtest && for i in $(seq 1 100); do touch /tmp/dtest/f_$i; done
time for i in $(seq 1 5000); do for f in /tmp/dtest/f_*; do test -f "$f"; done; done
```

**期望结果**：

| 配置              | 每次 open 的路径解析            | 相对性能        |
|-------------------|---------------------------------|-----------------|
| 有 Dentry Cache   | 第一次走 `backend.find()`，后续命中缓存 | ~1x (基准) |
| `no_dentry_cache` | 每次都走 `backend.find()` → 磁盘 I/O  | 10-100x 慢 |

**验证方式**：检查 `/proc/io_perf` 中 `vfs:` 段的 `dentry_lookups` / `dentry_hits` / `dentry_misses`。

---

### 5. Stat Cache — 反复获取文件属性

**核心价值**：Stat Cache 缓存每个 `Inode` 的 `VfsAttrs`（mode, size, nlink, timestamps 等），
避免每次 `stat()`/`fstat()` 都去后端读取 inode 元数据。

**测试程序**：

```c
// bench_stat_cache.c — 反复 stat 同一批文件
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/time.h>

#define ITERATIONS 10000

static double now_sec(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec + (double)tv.tv_usec / 1000000.0;
}

int main(void) {
    double t0, t1;
    struct stat st;

    // Setup: create 50 test files
    system("mkdir -p /tmp/stat_test");
    for (int i = 0; i < 50; i++) {
        char cmd[256];
        snprintf(cmd, sizeof(cmd), "touch /tmp/stat_test/file_%d", i);
        system(cmd);
    }

    // Repeatedly stat the same files
    t0 = now_sec();
    for (int iter = 0; iter < ITERATIONS; iter++) {
        for (int i = 0; i < 50; i++) {
            char path[256];
            snprintf(path, sizeof(path), "/tmp/stat_test/file_%d", i);
            stat(path, &st);
        }
    }
    t1 = now_sec();
    printf("%d iterations × 50 stats:  %.3f s\n", ITERATIONS, t1 - t0);

    return 0;
}
```

**等效 Linux 命令**：
```bash
mkdir -p /tmp/st && for i in $(seq 1 50); do touch /tmp/st/f_$i; done
time for i in $(seq 1 10000); do stat /tmp/st/f_* > /dev/null 2>&1; done
```

**等效场景 — `ls -l` 大目录**：
```bash
mkdir -p /tmp/bigdir && for i in $(seq 1 500); do touch /tmp/bigdir/file_$i; done
time ls -l /tmp/bigdir > /dev/null    # 每个文件都需要 stat_attrs
time ls -l /tmp/bigdir > /dev/null    # warm — 全部命中 stat cache
```

**期望结果**：

| 配置             | 首次 `ls -l`    | 第二次 `ls -l`    |
|------------------|-----------------|-------------------|
| 有 Stat Cache    | 每个文件读 inode | 全部命中内存缓存    |
| `no_stat_cache`  | 每个文件读 inode | 每个文件再次读 inode |

**验证方式**：检查 `/proc/io_perf` 中 `vfs:` 段的 `stat_attrs_calls` / `stat_attrs_cache_hits` / `stat_attrs_cache_misses`。

---

## 综合测试矩阵

以下矩阵列出了各 Cache 之间的依赖关系和推荐的测试组合：

| 测试目标          | no_page_cache | no_block_cache | no_inode_cache | no_dentry_cache | no_stat_cache |
|-------------------|:---:|:---:|:---:|:---:|:---:|
| **Page Cache**    | ✓   |     |     |     |     |
| **Block Cache**   |     | ✓   |     |     |     |
| **Inode Cache**   |     |     | ✓   |     |     |
| **Dentry Cache**  |     |     |     | ✓   |     |
| **Stat Cache**    |     |     |     |     | ✓   |
| **全缓存 vs 全直通** | ✓ | ✓   | ✓   | ✓   | ✓   |

> **注意**：
> - `no_page_cache` 只影响数据读写；元数据操作（find, stat, ls）不受影响。
> - `no_block_cache` 影响所有磁盘 I/O，包括元数据和数据。是最底层的缓存。
> - `no_inode_cache` 主要影响多路径访问同一文件的场景（硬链接、重复 open）。
> - `no_dentry_cache` 和 `no_stat_cache` 有协同效应：禁用两者后，`ls -l` 的开销会叠加。
> - 建议每个测试运行 3 次取平均值，第一次 warm-up 不计入。

## 与 `/proc/io_perf` 的集成

启用 `io_perf_counters` 后，以下统计数据可用：

```
# cat /proc/io_perf
page_cache:
  read_page_loads 1234
  write_mapping_calls 567
  ...
block_cache:
  cached_blocks 512
  get_calls 8000
  get_hits 7500
  get_misses 500
  ...
vfs:
  find_calls 1000
  dentry_lookups 800
  dentry_hits 790
  dentry_misses 10
  stat_attrs_calls 5000
  stat_attrs_cache_hits 4990
  stat_attrs_cache_misses 10
  ...
```

在每次 benchmark 前后 `cat /proc/io_perf` 可精确量化缓存效果。
