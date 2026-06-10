# Vendor 依赖说明

本目录保存评测环境不能访问 GitHub 时仍需使用的 Cargo Git 依赖源码。

当前 `os/Cargo.toml` 直接使用本地 path 依赖：

```toml
riscv = { path = "../vendor/riscv", features = ["inline-asm"] }
smoltcp = { path = "../vendor/smoltcp", default-features = false, features = [
    "alloc",
    "medium-ethernet",
    "proto-ipv4",
    "proto-ipv4-fragmentation",
    "socket-udp",
    "socket-tcp",
    "fragmentation-buffer-size-16384",
] }
```

因此 Cargo 会直接读取本目录中的源码；如果目录缺失或内容不完整，会直接报错，不会自动回退到 GitHub。

## 当前来源

- `vendor/riscv`
  - 原依赖：`https://github.com/rcore-os/riscv`
  - 当前提交：`11d43cf7cccb3b62a3caaf3e07a1db7449588f9a`
- `vendor/smoltcp`
  - 原依赖：`https://github.com/KyleMao2023/smoltcp`
  - 当前分支：`os`
  - 当前提交：`1e0289f919427e009f216da6eed774a4ccb5da2b`

## 手动更新流程

1. 临时把 `os/Cargo.toml` 改回 Git 依赖。

```toml
riscv = { git = "https://github.com/rcore-os/riscv", features = ["inline-asm"] }
smoltcp = { git = "https://github.com/KyleMao2023/smoltcp", branch = "os", default-features = false, features = [
    "alloc",
    "medium-ethernet",
    "proto-ipv4",
    "proto-ipv4-fragmentation",
    "socket-udp",
    "socket-tcp",
    "fragmentation-buffer-size-16384",
] }
```

2. 让 Cargo 拉取并锁定新版本。

```sh
cd os
cargo update -p riscv -p smoltcp
```

3. 从 `os/Cargo.lock` 中确认新的提交号。

```sh
rg 'name = "riscv"|name = "smoltcp"|source = "git\\+' Cargo.lock
```

4. 找到 Cargo git 缓存中的对应源码目录。

```sh
find ~/.cargo/git/checkouts -maxdepth 3 -type d -name '<短提交号>'
```

例如提交 `11d43cf7cccb3b62a3caaf3e07a1db7449588f9a` 对应的短提交号通常是 `11d43cf`。

5. 用缓存内容覆盖项目内 vendor 目录。

```sh
rsync -a --delete --exclude .git --exclude .cargo-ok \
  ~/.cargo/git/checkouts/<riscv-cache-dir>/<riscv-short-rev>/ \
  ../vendor/riscv/

rsync -a --delete --exclude .git --exclude .cargo-ok \
  ~/.cargo/git/checkouts/<smoltcp-cache-dir>/<smoltcp-short-rev>/ \
  ../vendor/smoltcp/
```

6. 把 `os/Cargo.toml` 恢复为本地 path 依赖。

```toml
riscv = { path = "../vendor/riscv", features = ["inline-asm"] }
smoltcp = { path = "../vendor/smoltcp", default-features = false, features = [
    "alloc",
    "medium-ethernet",
    "proto-ipv4",
    "proto-ipv4-fragmentation",
    "socket-udp",
    "socket-tcp",
    "fragmentation-buffer-size-16384",
] }
```

7. 离线验证。

```sh
cd os
cargo check --offline
```

8. 确认没有 GitHub Git 依赖残留。

```sh
rg 'git\\+https://github\\.com|git\\s*=\\s*"https://github\\.com' -n --glob 'Cargo.toml' --glob 'Cargo.lock'
```
