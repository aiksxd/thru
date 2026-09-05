# thru

跨平台设备通信工具，面向个人使用。在一台机器上运行轻量级守护进程，然后从另一台机器读写共享字典、传输文件、执行命令——全部通过单个 TCP 连接完成。

## 功能特性

- **守护进程服务器** — 后台运行，不阻塞终端；像服务一样启动/停止
- **一次性客户端命令** — 每个操作连接、执行、退出（类似 `echo`）；只有 `thru shell` 进入交互模式
- **共享字典** — `thru dict set/get/list`，支持追加模式用于日志场景，支持管道
- **文件传输** — `thru fetch2 ./dir` 交互式远程文件浏览器或直接管道下载；大文件以 1 MiB 分块流式传输
- **远程命令执行** — `thru exec "命令"` 一次性执行；`thru shell` 进入完整交互式 PTY（类似 SSH）

## 构建

```bash
cargo build --release
```

二进制文件位于 `target/release/thru`。

## 快速开始

### 1. 启动守护进程（机器 A）

```bash
thru [端口] [-p 密码]
# 默认端口 61696
thru
thru 222
thru 222 -p mysecret
```

服务器立即守护化并返回 shell。PID 写入 `$TEMP/thru_server.pid`（Windows）或 `/tmp/thru_server.pid`（Unix）。

### 2. 执行命令（机器 B）

```bash
thru 10.255.255.254:22 # client连接
# passwd:_ 若服务器有密码则需输入密码
# 共享字典
thru dict set greeting "hello from B"
thru dict get greeting
thru dict list

# 远程命令
thru exec "whoami && hostname"

# 文件传输
thru fetch2 ./downloads

# 启用交互式 shell
thru shell
```

### 3. 查看服务端状态

```bash
thru pid
# server pid: 12345
# active connections: 2
#   1: 192.168.1.10:54321
#   2: 10.0.0.5:33445
```

### 4. 停止守护进程

```bash
thru stop
```

## 使用说明

### 服务器守护进程

```bash
thru [端口] [-p 密码]      # 启动守护进程（后台）
thru stop                    # 停止运行中的守护进程
```

- 默认端口：`61696`
- 如果设置了 `-p`，所有客户端必须使用正确密码认证
- 守护进程是对等的：任何连接的客户端都可以使用所有功能
- 参数校验：服务端的位置参数必须是合法端口号（1-65535），客户端必须是合法的 `host:port`；非法参数会报错退出，不会静默启动服务端

### 客户端连接

```bash
thru <host:port> [-p password]
```

建立连接并缓存会话（地址 + 密码），供后续命令使用。

- 若服务器需要密码且未通过 `-p` 提供，则交互式提示输入
- 连接成功后显示服务端 PID 和当前活跃连接数：`connected to <addr> (server pid: 12345, 1 active connection)`
- 密码只在此处使用，后续子命令不接受 `-p`

### 服务端状态

```bash
thru pid
```

显示服务端 PID、活跃连接数量和每个连接的远程 IP 地址。若服务端未运行则报错。

```
server pid: 12345
active connections: 2
  1: 192.168.1.10:54321
  2: 10.0.0.5:33445
```

### 共享字典

```bash
# 设置值
thru dict set mykey "my value"
thru dict set mykey myvalue          # 单词可省略引号

# 读取值
thru dict get mykey

# 追加（类似 >> ）
thru dict set -a mykey " more text"
echo "log line" | thru dict set -a mykey

# 列表
thru dict list              # 仅键
thru dict list -v           # 仅值
thru dict list -kv          # 键<TAB>值对
```

管道行为：如果 `stdin` 不是终端且命令行没有提供值，则从 stdin 读取值。支持 `echo "data" | thru dict set mykey`。

### 远程命令执行

```bash
# 一次性命令（连接、执行、退出）
thru exec "ls -la"
thru exec "cd /tmp && mkdir test && ls"
```

命令在守护进程的 shell 环境中执行。Unix 下 shell 为 `$SHELL`（回退 `/bin/sh`）；Windows 下为 `%COMSPEC%`（回退 `cmd.exe`）。可用 `THRU_SHELL` 环境变量覆盖。

### 交互式远程 shell

```bash
thru shell
```

进入交互模式。在远程机器上打开完整 PTY 会话。

- `Ctrl+]` — 退出远程 shell，返回本地终端
- 终端大小自动同步
- `Ctrl+C` / `Ctrl+D` 发送到远程端


### 文件传输

```bash
# 交互式浏览器（在终端中运行）
thru fetch2 ./downloads

# 直接下载（管道传入远程路径）
echo "/var/log/syslog" | thru fetch2 ./downloads

# 指定服务器地址
thru fetch2 ./downloads
```

交互式浏览器操作：
- `↑`/`↓` — 选择条目
- `→`/`Enter` — 进入目录
- `←` — 返回上级目录
- 文件上按 `Enter` — 下载
- 目录上按两次 `Enter` — 确认递归下载
- `Esc`/`Ctrl+C` — 退出

大文件以 1 MiB 分块流式传输（不会全部加载到内存）。

### 密码认证

密码只在初始建立连接时使用，后续子命令自动复用缓存的会话密码，无需再次输入。

```bash
# 服务器设置密码
thru 61696 -p mysecret

# 客户端连接（若服务器有密码，会交互式提示输入）
thru 10.255.255.254:22
password: mysecret
connected to 10.255.255.254:22

# 也可以在连接时直接通过 -p 指定密码（非交互式场景）
thru 10.255.255.254:22 -p mysecret

# 建立连接后，后续命令无需再带密码
thru dict get mykey
thru exec "whoami"
thru shell
```

子命令（`dict` / `exec` / `fetch2` / `shell`）不接受 `-p` 参数。如果服务器需要密码但尚未建立连接，子命令会报错提示先执行 `thru <host:port>` 连接。

### 服务器地址解析

所有客户端子命令按以下顺序解析服务器地址：
1. `--connect <addr>` 参数
2. 已缓存的客户端会话（通过 `thru <host:port>` 连接后自动缓存）
3. `THRU_SERVER` 环境变量
4. `127.0.0.1:61696`（默认）

```bash
export THRU_SERVER=192.168.1.5:61696
thru dict get mykey
thru exec "whoami"
thru fetch2 ./downloads
```

## 项目结构

```
thru/
├── Cargo.toml                  # Workspace 根 + 二进制包
├── README.md                   # 英文文档
├── README.zh.md                # 本文档（中文）
├── src/
│   └── main.rs                 # CLI 入口：守护进程服务器 / 一次性客户端 / shell
├── examples/
│   ├── fs_test.rs              # 文件传输单元测试
│   ├── fs_dir_test.rs          # 递归目录下载测试
│   └── integration_test.rs     # 全链路集成测试（22 项）
└── crates/
    ├── thru-core/              # 帧编解码：4 字节大端长度前缀 + payload
    ├── thru-transport/         # 连接抽象：Transport / Listener / Connection traits
    ├── thru-proto-tcp/         # TCP 协议实现
    ├── thru-dict/              # 共享字典（set/get/append/list/keys）
    ├── thru-fs/                # 文件传输（LS/GET，1 MiB 分块流式）
    └── thru-exec/              # 远程执行（交互式 PTY + 脚本执行）
```

### 各 crate 职责

| Crate | 职责 |
|---|---|
| `thru-core` | 线路格式：长度前缀帧读写 |
| `thru-transport` | 可插拔协议的 trait 抽象；`Server` 封装 |
| `thru-proto-tcp` | `Transport`/`Listener`/`Connection` 的 TCP 实现 |
| `thru-dict` | 内存共享键值存储；二进制安全的值 |
| `thru-fs` | 远程目录列表和流式文件下载 |
| `thru-exec` | PTY 会话管理和带错误中断的脚本执行 |

## 协议

所有通信使用长度前缀帧。每个请求帧的第一个字节是操作码：

| 范围 | 功能 |
|---|---|
| 0–9 | 字典（GET/SET/APPEND/DEL/KEYS/ALL） |
| 20–29 | 文件系统（LS/GET） |
| 30–39 | 执行（SHELL_OPEN/RESIZE/EXEC_SCRIPT） |
| 50 | 服务端状态（PID / 连接数 / 远程 IP 列表） |
| 0x40–0x44 | 认证（REQUIRED/NONE/OK/FAIL/PASS） |

## 测试

```bash
# 全链路集成测试（dict + fs + exec，22 项）
cargo run --example integration_test
```

## 技术栈

- **Rust 2024 edition**
- **crossterm** — 跨平台终端 raw 模式、键盘事件、大小调整
- **portable-pty** — 跨平台伪终端（Unix PTY / Windows ConPTY）
- 无其他运行时依赖

## 许可证

个人使用项目。
