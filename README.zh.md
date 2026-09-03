# thru

面向个人使用的跨平台设备通信工具。连接两台设备后，可在同一连接上共享字典、传输文件、执行远程命令。

## 功能特性

- **统一连接抽象层** — 可插拔协议（内置 TCP），server/client 模式
- **公共字典** — `thru dict key` 读写分布式键值存储；支持追加模式（日志场景）
- **文件传输** — `thru fetch2 ./dir` 打开交互式远程文件浏览器；管道传入远程路径直接下载；大文件以 1 MiB 分块流式传输
- **远程命令执行** — `thru host:port` 打开交互式 PTY shell（类似 SSH）；管道传入脚本非交互执行，支持错误中断

## 构建

```bash
cargo build --release
```

二进制位于 `target/release/thru`。

## 快速开始

### 启动 server

```bash
thru [端口] [-协议类型] [-p 密码]
# 默认端口 61696，默认协议 tcp
thru
thru 222
thru 222 -tcp
thru 222 -p mysecret
```

如果设置了 `-p`，client 必须提供正确密码才能进入会话。

### 作为 client 连接

```bash
thru <主机:端口> [-协议类型] [-p 密码]
thru 192.168.1.5:61696
thru 192.168.1.5:61696 -p mysecret
```

如果 server 要求密码但未提供 `-p`，会交互式提示输入密码（输入不回显）。

连接后双方都进入**对等会话模式**——任何一方都可以向对方发起命令。输入 `help` 查看可用命令，`exit` 断开连接。

```
thru> help
commands:
  dict <key> [value]      读写对方字典
  dict list                列出对方字典键
  ls [path]                列出对方目录
  get <remote> [local]     从对方下载文件
  exec <command>           在对方执行命令
  shell                     进入交互式远程 PTY 终端（Ctrl+] 返回）
  exit                      断开连接
```

会话模式下，server 和 client 功能完全对等——双方都可以读取对方的字典、下载对方的文件、在对方上执行命令。唯一区别是谁发起连接。

## 使用说明

### 对等会话模式（主要接口）

```bash
# 设备 A：监听
thru 61696

# 设备 B：连接
thru 192.168.1.5:61696

# 连接后双方都有 "thru> " 提示符，可以互相操作：
thru> dict set greeting hello from A
thru> dict get greeting
thru> ls .
thru> exec whoami
thru> get remote_file.txt local_copy.txt
thru> shell          # 进入完整的交互式远程终端（Ctrl+] 返回）
thru> exit
```

### 一次性命令（脚本接口）

以下命令连接、执行、断开——适合脚本使用。

### 公共字典

```bash
# 写（通过管道传入 value）
echo "hello world" | thru dict msgA

# 读（在终端中直接运行，不接管道）
thru dict msgA

# 追加（等价于 >> ）
cat log.txt | thru dict -a msgA

# 列出所有键
thru dict list
thru dict list -v    # 仅值
thru dict list -kv   # 键<TAB>值 对
```

字典存储在每一方本地；在会话模式中，你读写的是**对方**的字典。

### 文件传输

```bash
# 交互式浏览器（在终端中运行）
thru fetch2 ./downloads

# 直接下载（管道传入远程路径）
echo "/var/log/syslog" | thru fetch2 ./downloads

# 指定 server 地址
thru fetch2 ./downloads --connect 192.168.1.5:61696
```

交互式浏览器操作：
- `↑`/`↓` — 移动选择
- `→`/`Enter` — 进入目录
- `←` — 返回上级目录
- 文件上按 `Enter` — 下载该文件
- 目录上按两次 `Enter` — 确认递归下载整个目录
- `q`/`Esc` — 退出

大文件以 1 MiB 分块流式传输（不会全部缓冲在内存中）。

### 远程命令执行

```bash
# 交互式 PTY shell（类似 SSH）
thru 192.168.1.5:61696

# 非交互脚本执行
echo "ls -la" | thru 192.168.1.5:61696

# 多行脚本，cd 状态保持
cat <<'EOF' | thru 192.168.1.5:61696
cd /tmp
mkdir testdir
cd testdir
echo "hello" > file.txt
cat file.txt
EOF
```

非交互模式下，任一命令失败即中断后续执行（类似 `set -e`），退出码传播到本地进程。

### Server 地址配置

所有 client 子命令按以下优先级解析 server 地址：
1. `--connect <addr>` 参数
2. `THRU_SERVER` 环境变量
3. `127.0.0.1:61696`（默认）

```bash
export THRU_SERVER=192.168.1.5:61696
thru dict msgA
thru fetch2 ./downloads
```

## 项目目录结构

```
thru/
├── Cargo.toml                  # Workspace 根 + 二进制包
├── README.md                   # 英文文档
├── README.zh.md                # 本文档（中文）
├── src/
│   └── main.rs                 # CLI 入口：server / client(PTY) / dict / fetch2
├── examples/
│   ├── fs_test.rs              # 文件传输单元测试
│   ├── fs_dir_test.rs          # 目录递归下载测试
│   └── integration_test.rs     # 全链路集成测试（22 项）
└── crates/
    ├── thru-core/              # 帧编解码：4 字节大端长度 + 数据
    ├── thru-transport/         # 连接抽象：Transport / Listener / Connection trait
    ├── thru-proto-tcp/         # TCP 协议实现
    ├── thru-dict/              # 公共字典（set/get/append/list）
    ├── thru-fs/                # 文件传输（LS/GET，1 MiB 分块流式）
    └── thru-exec/              # 远程执行（交互式 PTY + 脚本执行）
```

### 各 crate 职责

| Crate | 职责 |
|---|---|
| `thru-core` | 线上格式：长度前缀帧的读写 |
| `thru-transport` | 可插拔协议的 trait 抽象；`Server` 封装 |
| `thru-proto-tcp` | `Transport`/`Listener`/`Connection` 的 TCP 实现 |
| `thru-dict` | 内存共享键值存储；二进制安全的值 |
| `thru-fs` | 远程目录列表和流式文件下载 |
| `thru-exec` | PTY 会话管理和带错误中断的脚本执行 |

## 通信协议

所有通信通过所选传输层使用长度前缀帧。每个请求帧的第一个字节为操作码：

| 范围 | 功能 |
|---|---|
| 0–9 | 字典（GET/SET/APPEND/DEL/KEYS/ALL） |
| 20–29 | 文件系统（LS/GET） |
| 30–39 | 命令执行（SHELL_OPEN/RESIZE/EXEC_SCRIPT） |

## 测试

```bash
# 全链路集成测试（字典 + 文件 + 命令，共 22 项）
cargo run --example integration_test
```

## 技术栈

- **Rust 2024 edition**
- **crossterm** — 跨平台终端 raw mode、键盘事件、窗口大小变化处理
- **portable-pty** — 跨平台伪终端（Unix PTY / Windows ConPTY）
- 无其他运行时依赖

## 许可证

个人使用项目。
