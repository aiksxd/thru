# thru 项目代码审查报告

> 审查范围：整个 workspace（`src/main.rs` + `crates/*` + `examples/*`）
> 审查日期：2026-09-05
> 编译状态：`cargo check` 通过，零警告
> 代码规模：main.rs 1741 行 / 6 个 crate / 3 个 example

---

## 项目概览

| 模块 | 行数 | 职责 |
|------|------|------|
| `src/main.rs` | 1741 | CLI 入口、服务端循环、反向隧道、认证、shell/exec/dict/fetch2/device 客户端 |
| `crates/thru-core` | 19 | 长度前缀帧编解码 |
| `crates/thru-transport` | 49 | Transport/Listener/Connection trait |
| `crates/thru-proto-tcp` | 54 | TCP 传输实现 |
| `crates/thru-dict` | 203 | 共享字典（服务端+客户端） |
| `crates/thru-fs` | 238 | 文件系统 LS/GET（服务端+客户端） |
| `crates/thru-exec` | 277 | PTY 交互 shell + 脚本执行 |

---

# 第一项：代码冗余与可重构精简点

按"精简收益"从高到低排列。

## R1. 【高收益】main.rs 单文件承担 8 种职责，应拆分模块

**位置**：`src/main.rs` 全文

当前 1741 行的单文件混杂了以下独立职责：
- CLI 分发与参数解析（`main`, `server_cmd`, `connect_cmd`）
- 服务端主循环与连接管理（`run_server`）
- 反向隧道注册/代理/心跳（`handle_reverse_*`, `spawn_heartbeat_monitor`）
- 认证握手（`auth_server`, `auth_client`, `read_password`）
- shell 客户端（`shell_cmd`, `remote_shell_interactive`, `key_to_bytes`）
- exec 客户端（`exec_cmd`）
- dict 客户端（`dict_cmd`）
- fetch2 文件浏览器（`fetch2_cmd`, `fetch2_interactive`, `download_dir_recursive`）
- device 选择器（`device_cmd`, `select_device_interactive`, `parse_device_list`）

**建议**：拆分为 `src/cli.rs`、`src/server.rs`、`src/reverse.rs`、`src/auth.rs`、`src/tui.rs` 等模块，`main.rs` 仅保留入口分发。

---

## R2. 【高收益】"连接+认证+超时"样板在 6 个命令中重复

**位置**：`shell_cmd`(L~1110)、`exec_cmd`(L~1260)、`dict_cmd`(L~1310)、`fetch2_cmd`(L~1430)、`device_cmd`(L~1690)、`pid_cmd`(L~1060)

每个客户端命令都包含几乎相同的代码块：

```rust
let (addr, rest) = resolve_addr(args);
let pw = session_password();
let t: &dyn Transport = &Tcp;
let mut conn = connect(t, &addr)?;
conn.set_read_timeout(Some(READ_TIMEOUT))?;
let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
if !ok { exit(1); }
```

**建议**：提取统一函数：

```rust
fn open_session(args: &[String], interactive: bool) -> io::Result<(Box<dyn Connection>, String, Vec<String>)> {
    let (addr, rest) = resolve_addr(args);
    let pw = session_password();
    let mut conn = connect(&Tcp, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), interactive)?;
    if !ok { exit(1); }
    Ok((conn, addr, rest))
}
```

预计可消除约 60 行重复代码。

---

## R3. 【高收益】Daemon 化逻辑在 `server_cmd` 和 `connect_cmd` 中重复

**位置**：`server_cmd`(L~230-260)、`connect_cmd`(L~960-990)

两处都有：
```rust
let exe = std::env::current_exe()?;
let mut cmd = Command::new(exe);
cmd.args(args).env("THRU_DAEMON"/"THRU_REVERSE_DAEMON", "1")
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
#[cfg(windows)] { cmd.creation_flags(0x08000000); }
let child = cmd.spawn()?;
```

**建议**：提取 `spawn_daemon(args: &[String], env_key: &str, pid_file: &Path) -> io::Result<u32>`，统一处理 spawn、PID 写入、Windows 标志。

---

## R4. 【中收益】进程终止代码在 `stop_server` 中重复两次

**位置**：`stop_server`(L~830-870)

Windows `taskkill /F /PID` 和 Unix `kill -9` 的逻辑分别对 server PID 和 reverse PID 各写了一遍。

**建议**：提取 `fn force_kill(pid: u32)` 函数，用 `#[cfg]` 内部分支。

---

## R5. 【中收益】Wire format 编码/解码模式在 4 个 crate 中重复

以下模式反复出现：
- 写 `[u16 len][bytes]`：`thru-dict::req`、`thru-fs::req`、`main::push_string`、`main::make_proxy_frame`、`thru-dict::OP_ALL` 编码
- 读 `[u16 len][bytes]`：`thru-dict::handle`、`thru-fs::parse_path`、`main::parse_reverse_registration`、`main::parse_reverse_proxy_req`、`main::handle_set_target`、`main::parse_device_list`

**建议**：在 `thru-core` 中添加扩展 trait：

```rust
pub trait BufExt {
    fn push_u16(&mut self, v: u16);
    fn push_str(&mut self, s: &str);
    fn push_u32(&mut self, v: u32);
    fn push_u64(&mut self, v: u64);
}
pub trait ReadExt {
    fn read_u16(&mut self) -> io::Result<u16>;
    fn read_len_str(&mut self) -> io::Result<String>;
    fn read_u32(&mut self) -> io::Result<u32>;
    fn read_u64(&mut self) -> io::Result<u64>;
}
```

预计可消除约 80 行手写字节操作。

---

## R6. 【中收益】`handle_reverse_proxy_ls` 与 `handle_reverse_proxy_get` 结构高度重复

**位置**：`main.rs` L~700-810

两个函数的前半部分（解析请求 → 查找设备 → 加锁 → 写请求 → 错误处理）完全相同，只有后半部分（LS 单帧转发 vs GET 流式转发）不同。

**建议**：提取 `fn acquire_device(devices, name) -> io::Result<(Arc<Mutex<Box<dyn Connection>>>, String, String)>`，然后两个函数各自只处理协议差异部分。

---

## R7. 【中收益】`thru-fs` 中 size header 解析重复

**位置**：`thru-fs/src/lib.rs` L180-186（`get_on_conn`）和 L224-227（`client_get_progress`）

两处都有：
```rust
if first.len() < 9 { return Err(...); }
let size = u64::from_be_bytes([first[1],...first[8]]);
```

**建议**：`get_on_conn` 内部直接调用 `client_get_progress` 风格的逻辑，或提取 `fn parse_size_header(frame: &[u8]) -> io::Result<u64>`。

---

## R8. 【中收益】TUI 交替屏幕 + raw_mode 样板在 3 处重复

**位置**：`fetch2_interactive`(L~1500)、`select_device_interactive`(L~1620)、`read_password`(L~905)

都有相同的模式：
```rust
print!("\x1b[?1049h\x1b[?25l");  // 进入交替屏幕
enable_raw_mode()?;
// ... 交互循环 ...
let _ = disable_raw_mode();
print!("\x1b[?1049l\x1b[?25h");  // 退出交替屏幕
```

且 `read_password` 中用 IIFE `(|| -> io::Result<String> { ... })()` 来确保 raw_mode 被清理，写法笨拙。

**建议**：定义 RAII guard：

```rust
struct RawModeGuard;
impl RawModeGuard {
    fn new() -> io::Result<Self> { enable_raw_mode()?; Ok(Self) }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) { let _ = disable_raw_mode(); }
}
struct AlternateScreen;
impl Drop for AlternateScreen {
    fn drop(&mut self) { print!("\x1b[?1049l\x1b[?25h"); let _ = io::stdout().flush(); }
}
```

---

## R9. 【低收益】`thru-dict` 客户端函数样板

**位置**：`thru-dict/src/lib.rs` L162-180

`client_get`/`client_set`/`client_append`/`client_keys` 每个都是 `connect + *_on_conn` 的两行包装。

**建议**：可用宏 `client_fn!($name:ident, $on_conn:ident)` 批量生成，或接受当前的显式写法（可读性尚可）。

---

## R10. 【低收益】`parse_reverse_registration` 与 `parse_reverse_proxy_req` 结构相同

**位置**：`main.rs` L~380-430

两个函数都解析 `[op][u16 len][str][u16 len][str]` 格式，只是返回类型不同（前者总是返回 Some，后者返回 Option）。

**建议**：统一为 `fn parse_two_strings(data: &[u8]) -> Option<(String, String)>`，`parse_reverse_registration` 调用后 `unwrap_or(("unknown".into(), "unknown".into()))`。

---

## R11. 【低收益】`PtySize` 构造重复

**位置**：`thru-exec/src/lib.rs` L54-61 和 L98-105

`PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }` 出现两次。

**建议**：添加 `impl PtySession { fn size(rows: u16, cols: u16) -> PtySize { ... } }` 或直接用 `..Default::default()`（如果 PtySize 实现了 Default）。

---

## R12. 【低收益】examples 中的服务端循环与 main.rs 重复

**位置**：`examples/integration_test.rs` L30-55

测试中的服务端 dispatch 逻辑（opcode match）与 `run_server` 中的几乎一样。如果协议变更，需要同步修改两处。

**建议**：将服务端 dispatch 提取为 `thru-server` crate 中的可复用函数，example 直接调用。

---

# 第二项：丑陋实现审查（甲方优雅代码标准）

按"丑陋程度"从高到低排列。

## U1. 【严重】魔法数字遍布服务端 dispatch

**位置**：`main.rs` `run_server` 线程循环 L~340-370

```rust
match f[0] {
    0..=9 => { ... }           // dict — 没有用 thru_dict::OP_* 常量
    20..=29 => { ... }         // fs — 没有用 thru_fs::OP_* 常量
    30 => { ... }               // shell — 裸数字
    32 => { ... }               // exec — 裸数字
    50 => { ... }               // status — 裸数字
    61 => handle_reverse_list(...)   // 裸数字
    62 => handle_reverse_proxy_ls(...)
    63 => handle_reverse_proxy_get(...)
    64 => handle_device_list(...)
    65 => handle_set_target(...)
    _ => {}
}
```

opcode 常量在各自 crate 中定义了（`thru_dict::OP_GET` 等），但服务端 dispatch 全部用裸数字范围。`run_reverse_daemon` 中同样如此（L~1030）。

**优雅做法**：
```rust
match f[0] {
    op if (thru_dict::OP_GET..=thru_dict::OP_ALL).contains(&op) => { ... }
    op if (thru_fs::OP_LS..=thru_fs::OP_GET).contains(&op) => { ... }
    thru_exec::OP_SHELL_OPEN => { ... }
    thru_exec::OP_EXEC_SCRIPT => { ... }
    OP_STATUS => { ... }
    OP_REVERSE_LIST => { ... }
    // ...
}
```

---

## U2. 【严重】状态码 `0u8`/`2u8` 裸字面量散落各处

**位置**：`main.rs` 中至少 15 处

- `let mut resp = vec![0u8];` — ST_OK
- `vec![2u8]` — ST_ERR（`reverse_err` 内部）
- `write_frame(conn, &[0u8])` — ST_OK
- `if ack.is_empty() || ack[0] != 0` — 检查 ST_OK
- `if header.first() != Some(&0u8)` — 检查 ST_OK
- `if resp[0] != 0` — 检查 ST_OK

`thru_dict::ST_OK`/`ST_ERR` 和 `thru_fs::ST_OK`/`ST_ERR` 都已定义，但 main.rs 中大量使用裸数字。

**优雅做法**：统一引用 `thru_dict::ST_OK` 或将状态码提升到 `thru-core` 中共享定义。

---

## U3. 【严重】`run_server` 参数解析逻辑脆弱且反直觉

**位置**：`main.rs` L~285-305

```rust
while i < args.len() {
    let a = &args[i];
    if a == "-p" && i + 1 < args.len() {
        password = Some(args[i + 1].clone());
        i += 2;
    } else if (a == "-m" || a == "--max-devices") && i + 1 < args.len() {
        max_devices = args[i + 1].parse().unwrap_or(32);
        i += 2;
    } else if a.starts_with('-') {
        proto = a.trim_start_matches('-').to_string();  // ← 任何 -xxx 都被当成协议名
        i += 1;
    } else {
        port = a.parse().unwrap_or(PORT);
        i += 1;
    }
}
```

问题：
1. 任何未知的 `-xxx` 参数都会被静默当成协议名（如 `-foo` → proto="foo"），然后 `transport("foo")` panic。
2. `proto` 变量只有 `"tcp"` 一个有效值，`transport()` 函数对其他值 panic。
3. 协议选择通过 `-tcp` 这样的参数传递，与常规 CLI 风格（`--proto tcp`）不符。
4. `unwrap_or(32)` 和 `unwrap_or(PORT)` 静默忽略解析错误，用户输入 `-m abc` 不会报错。

**优雅做法**：使用 `clap` crate 或至少写一个明确的参数解析结构体，对未知参数报错。

---

## U4. 【严重】`parse_reverse_registration` 静默降级为 "unknown"

**位置**：`main.rs` L~385-410

```rust
fn parse_reverse_registration(data: &[u8]) -> (String, String) {
    if data.len() < 3 {
        return ("unknown".to_string(), "unknown".to_string());
    }
    // ... 更多分支返回 "unknown" ...
}
```

解析失败时返回 `"unknown"` 字符串，调用方无法区分"设备真的叫 unknown"和"解析失败"。这是一个典型的"用哨兵值代替错误处理"的反模式。

**优雅做法**：返回 `Option<(String, String)>` 或 `io::Result<(String, String)>`，调用方显式处理失败。

---

## U5. 【严重】大量 `let _ = write_frame(...)` 静默忽略写入错误

**位置**：`main.rs` 服务端请求处理中至少 20 处

```rust
0..=9 => { let r = dict.handle(&f); let _ = write_frame(&mut conn, &r); }
30 => { let _ = thru_exec::handle_shell_open(&mut conn, &f); }
32 => { let _ = thru_exec::handle_exec_script(&mut conn, &f); }
```

服务端处理请求时，如果写入响应失败（客户端已断开），错误被静默忽略。更严重的是，`handle_shell_open` 和 `handle_exec_script` 返回 `io::Result<()>`，整个结果被 `let _ =` 丢弃，包括其中的 PTY 启动失败、脚本执行失败等错误。

**优雅做法**：至少记录日志；对致命错误应断开连接。可定义 `fn log_err<T>(r: io::Result<T>, ctx: &str)` 辅助函数。

---

## U6. 【中等】`dispatch_fs` 中 `unwrap()` 虽安全但不优雅

**位置**：`main.rs` L~490

```rust
if target_device.is_none() {
    *target_device = Some(default_target(devices));
}
let (name, user) = target_device.as_ref().unwrap();  // ← 刚赋值完就 unwrap
```

虽然逻辑上安全，但 `unwrap` 会让读者停下来验证"这里真的不会 panic 吗"。

**优雅做法**：
```rust
let target = target_device.get_or_insert_with(|| default_target(devices));
let (name, user) = &target;
```

---

## U7. 【中等】Windows 临时 .bat 文件用 PID 命名，并发不安全

**位置**：`thru-exec/src/lib.rs` L218

```rust
let bat_path = std::env::temp_dir().join(format!("thru_exec_{}.bat", std::process::id()));
```

同一进程内并发执行多个 `exec_script` 会互相覆盖同一个 .bat 文件。且进程崩溃后临时文件残留。

**优雅做法**：使用 `tempfile::NamedTempFile` 或在文件名中加入随机/递增 ID（如 `thru_exec_{pid}_{counter}.bat`），并用 RAII 确保清理。

---

## U8. 【中等】`fetch2_cmd` 用"写测试文件再删掉"检查写权限

**位置**：`main.rs` L~1450

```rust
let test_file = std::path::Path::new(&local_dir).join(".thru_write_test");
if fs::write(&test_file, b"x").is_err() {
    eprintln!("no write permission for '{local_dir}'");
    exit(1);
}
let _ = fs::remove_file(&test_file);
```

这种方式有几个问题：
1. 如果程序在 `write` 和 `remove_file` 之间崩溃，测试文件残留。
2. 固定文件名 `.thru_write_test`，并发运行两个 `fetch2` 会冲突。
3. 写完立刻删，本质上是在做 `fs::metadata` + 权限位检查能做的事。

**优雅做法**：用 `std::fs::metadata` 检查目录权限，或至少用唯一文件名 + RAII 清理。

---

## U9. 【中等】`key_to_bytes` 中 `c as u8` 对非 ASCII 字符截断

**位置**：`main.rs` L~1210

```rust
KeyCode::Char(c) => {
    if ctrl {
        let b = c as u8;  // ← 非 ASCII 字符（如中文）会被截断为低位字节
        if (b'a'..=b'z').contains(&b) { out.push(b - b'a' + 1); }
        else if (b'A'..=b'Z').contains(&b) { out.push(b - b'A' + 1); }
        else { out.extend_from_slice(c.to_string().as_bytes()); }
    } else {
        out.extend_from_slice(c.to_string().as_bytes());
    }
}
```

如果用户按 Ctrl+中文字符，`c as u8` 截断后可能恰好落在 a-z 范围内，导致发送错误的控制字符。

**优雅做法**：先检查 `c.is_ascii()`，非 ASCII 字符直接走 UTF-8 编码分支。

---

## U10. 【中等】`read_password` 用 IIFE 模拟异常安全

**位置**：`main.rs` L~910-930

```rust
enable_raw_mode()?;
let mut pw = String::new();
let result = (|| -> io::Result<String> {
    loop {
        if let event::Event::Key(key) = event::read()? {
            // ...
        }
    }
    Ok(pw)
})();
let _ = disable_raw_mode();
eprintln!();
result
```

IIFE 的目的是确保 `disable_raw_mode()` 在返回前被调用，但写法笨拙，且 `pw` 被 move 进闭包后外部无法访问。

**优雅做法**：用 R8 中建议的 `RawModeGuard` RAII，直接写普通循环。

---

## U11. 【中等】`stop_server` 中 `let _ = Command::new(...).output()` 忽略所有错误

**位置**：`main.rs` L~840, L~860

```rust
let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
```

如果进程杀不掉（权限不足、进程已退出），用户不会收到任何提示，只会看到 "thru server stopped"。

**优雅做法**：检查 `output()` 的返回值和退出码，失败时打印警告。

---

## U12. 【低】`download_dir_recursive` 中 `local_dir.to_str().unwrap()`

**位置**：`main.rs` L~1590

```rust
download_dir_recursive(conn, &remote_dir, &e.name, local_dir.to_str().unwrap())?;
```

如果本地路径包含非 Unicode 字符（Windows 上罕见但可能），会 panic。

**优雅做法**：传递 `&Path` 而不是 `&str`，避免 Unicode 转换。

---

## U13. 【低】`thru-dict` 中 `Mutex::lock().unwrap()` 遇中毒 panic

**位置**：`thru-dict/src/lib.rs` L66

```rust
let mut map = self.map.lock().unwrap();
```

如果另一个线程在持有锁时 panic，mutex 中毒，后续所有 `handle` 调用都会 panic 而不是返回错误。

**优雅做法**：`.lock().map_err(|_| io::Error::new(io::ErrorKind::Other, "dict mutex poisoned"))`，或使用 `parking_lot::Mutex`（不中毒）。

---

## U14. 【低】`run_server` 中 `s.accept()` 失败后无限 `continue`

**位置**：`main.rs` L~315

```rust
let mut conn = match s.accept() {
    Ok(c) => c,
    Err(_) => continue,  // ← 如果 listener 致命错误，CPU 100% 空转
};
```

accept 失败可能是瞬时错误（如 EINTR），也可能是致命错误（如 listener fd 被关闭、EMFILE）。无差别 `continue` 会在致命错误时导致 CPU 空转。

**优雅做法**：对致命错误（非 EINTR/ECONNABORTED）记录日志并 `break` 或退避重试。

---

## U15. 【低】仓库根目录提交了 3 个 .pdb 文件

**位置**：`test_device.pdb`, `test_device2.pdb`, `test_heartbeat.pdb`（各 1.3MB）

这些是 Windows 调试符号文件，不应提交到版本控制。`.gitignore` 仅 8 字节（可能只忽略了 `/target`）。

**优雅做法**：在 `.gitignore` 中添加 `*.pdb`，并从仓库中移除这三个文件。

---

## U16. 【低】多个 crate 的 Cargo.toml 带 BOM 头

**位置**：`crates/*/Cargo.toml`（除 thru-core 外都有 `﻿` BOM 字符）

虽然 cargo 能容忍 BOM，但不符合 Rust 生态惯例，可能导致某些工具解析异常。

**优雅做法**：保存为 UTF-8 无 BOM。

---

## U17. 【低】`print_help` 帮助文本不完整

**位置**：`main.rs` L~155

```
thru dict <key> [value] [-a]         Read/write shared dictionary
```

但实际 `dict` 还支持 `get`/`set`/`list -v/-kv` 子命令，帮助文本只字未提。

**优雅做法**：完整列出所有子命令和选项。

---

# 第三项：项目风险评估

按风险等级排列。

## 高风险

### H1. 明文密码传输，无 TLS 加密

**位置**：`auth_server`/`auth_client`（`main.rs` L~880-950）

密码通过 `AUTH_PASS` 帧以明文形式在 TCP 连接上传输。任何中间人（同一局域网、代理、路由器）都能截获密码。

**影响**：密码泄露 → 攻击者获得完整的远程文件访问 + 远程代码执行能力。

**建议**：至少支持 TLS（`rustls`），或在文档中明确要求通过 SSH 隧道/VPN 使用。

---

### H2. 无暴力破解防护

**位置**：`auth_server`（`main.rs` L~880）

密码错误时立即返回 `AUTH_FAIL`，无延迟、无锁定、无尝试次数限制。攻击者可以每秒尝试数千个密码。

**影响**：弱密码可在秒级被暴力破解。

**建议**：失败后增加指数退避延迟（如 1s → 2s → 4s），或限制同一 IP 的尝试频率。

---

### H3. 任意文件读取（路径遍历）

**位置**：`thru-fs/src/lib.rs` `handle_ls`(L75)、`handle_get`(L96)

```rust
let path = parse_path(data)?;
fs::read_dir(p) / fs::File::open(path)
```

客户端传入的路径直接传给 `fs::read_dir`/`fs::File::open`，无任何白名单、沙箱或规范化检查。客户端可以请求 `../../etc/passwd`、`C:\Windows\System32\config\SAM` 等任意路径。

**影响**：服务器上任何 thru 进程可读的文件都能被下载。结合密码明文传输和无暴力破解防护，这是一个严重的信息泄露漏洞。

**建议**：
1. 服务器启动时指定一个根目录（如 `--root /srv/thru`），所有路径都 `canonicalize` 后确认在根目录内。
2. 至少在文档中警告"thru 暴露整个文件系统"。

---

### H4. 远程代码执行（RCE）

**位置**：`thru-exec/src/lib.rs` `handle_exec_script`(L197)、`handle_shell_open`(L138)

`exec_script` 允许客户端在服务器上执行任意 shell 命令/脚本。`shell_open` 提供完整的交互式 PTY shell。

**影响**：任何通过认证的客户端都能在服务器上执行任意命令，获得与 thru 进程相同的权限。如果 thru 以 root/Administrator 运行，则完全控制服务器。

**建议**：
1. 这是设计功能，但应在文档中显著警告安全风险。
2. 考虑添加 `--no-exec` 选项禁用远程执行。
3. 考虑以非特权用户运行。

---

### H5. `thru-core::read_frame` 无最大长度限制，可 OOM 攻击

**位置**：`crates/thru-core/src/lib.rs` L12-18

```rust
let n = u32::from_be_bytes(h) as usize;
let mut b = vec![0u8; n];  // ← 攻击者可声明 4GB 长度
r.read_exact(&mut b)?;
```

帧头是 4 字节无符号整数，最大可表示 4GB。攻击者连接后发送一个声称长度为 4GB 的帧头，服务器会立即 `vec![0u8; 4_000_000_000]`，导致 OOM 崩溃。

**影响**：未认证即可导致服务器拒绝服务（因为 `read_frame` 在认证之前的 `auth_server` 中就被调用）。

**建议**：添加 `const MAX_FRAME: usize = 64 * 1024 * 1024;`（或根据需求调整），超过则返回错误。

---

### H6. `thru-dict` 无内存限制，可 OOM 攻击

**位置**：`crates/thru-dict/src/lib.rs` `handle` OP_SET/OP_APPEND

`Dict` 的 `HashMap` 可以无限增长，单个 value 也没有大小限制。已认证的客户端可以通过不断 `SET` 大值或 `APPEND` 导致服务器 OOM。

**影响**：已认证客户端可导致服务器拒绝服务。

**建议**：添加最大总内存限制和单 key 最大 value 限制，超过时返回错误。

---

### H7. 会话文件明文存储密码，权限不受控

**位置**：`main.rs` `save_session`(L~210)、`session_file`(L~200)

```rust
fn session_file() -> PathBuf {
    std::env::temp_dir().join("thru_client_session")
}
fn save_session(addr: &str, password: Option<&str>) -> io::Result<()> {
    let content = format!("{}\n{}\n", addr, password.unwrap_or(""));
    fs::write(session_file(), content)  // ← 默认权限，其他用户可读
}
```

密码以明文存储在系统临时目录中，文件权限使用系统默认（Windows 上通常继承目录权限，多用户系统上其他用户可能可读；Linux 上通常是 644）。

**影响**：同一机器上的其他用户可以读取密码，然后连接到 thru 服务器。

**建议**：
1. Linux 上创建文件后 `chmod 0600`。
2. 考虑使用系统密钥环（Windows Credential Manager、macOS Keychain、Linux Secret Service）。
3. 至少在文档中警告。

---

## 中风险

### M1. PID 文件 TOCTOU 竞争 + PID 复用

**位置**：`server_cmd`(L~225)、`stop_server`(L~830)

`server_cmd` 先检查 PID 文件是否存在，再 spawn 子进程，再写 PID 文件——检查和写入之间有竞争窗口。更严重的是，`stop_server` 读取 PID 文件中的 PID 后直接 `kill -9`，如果原进程已退出且 PID 被其他进程复用，会杀掉无辜进程。

**影响**：低概率但可能导致误杀其他进程。

**建议**：
1. 写 PID 文件时用 `O_CREAT | O_EXCL` 原子创建。
2. `stop_server` 中 kill 前检查进程名是否匹配。

---

### M2. Daemon 化后不确认子进程是否成功启动

**位置**：`server_cmd`(L~260)、`connect_cmd`(L~990)

父进程 spawn 子进程后立即 `exit(0)`，打印 "thru server started"。但如果子进程绑定端口失败（端口被占用），它会静默退出，用户误以为服务已启动。

**影响**：用户看到 "started" 但实际服务未运行，后续连接失败时难以排查。

**建议**：父进程等待子进程发送"启动成功"信号（如通过管道或 PID 文件+健康检查），或至少等待 100ms 后检查子进程是否仍在运行。

---

### M3. 反向隧道双向 RCE 信任链

**位置**：`run_reverse_daemon`(L~1010)

反向客户端注册后，服务器可以向其发送 `OP_SHELL_OPEN`(30) 和 `OP_EXEC_SCRIPT`(32) 请求，反向客户端会无条件执行。这意味着：
- 如果服务器被攻陷，所有连接的反向客户端都会被 RCE。
- 如果一个反向客户端被攻陷，它可以向服务器返回恶意文件内容（但不能直接 RCE 服务器，除非服务器端有解析漏洞）。

**影响**：横向移动风险。一个节点被攻陷可能波及整个 thru 网络。

**建议**：
1. 反向客户端可以配置 `--allow-exec=false` 禁用远程执行。
2. 服务器端记录所有 exec 请求的审计日志。
3. 考虑双向认证（服务器也向客户端证明身份）。

---

### M4. Heartbeat 线程可能泄漏或误判

**位置**：`spawn_heartbeat_monitor`(L~660)

心跳线程每 15 秒写一个 PING 帧。如果写失败，标记设备离线并退出。问题：
1. 设备重连后，旧的心跳线程可能仍在运行（如果旧连接的写还没失败），两个线程同时操作同一个设备记录。
2. 代码用 `Arc::ptr_eq` 检查是否仍是当前连接，这是正确的，但旧线程在 `ptr_eq` 检查和 `mark_device_offline` 之间有竞争窗口。
3. 心跳只写不读，无法检测"写成功但对端已死"的半开连接（TCP 层面可能需要很久才能检测到）。

**影响**：设备状态可能短暂不一致，或离线检测延迟较高（依赖 TCP keepalive）。

**建议**：
1. 设备重连时显式停止旧心跳线程（用 `AtomicBool` 取消标志）。
2. 考虑设置 TCP keepalive 选项。

---

### M5. `download_dir_recursive` 无深度限制和循环检测

**位置**：`main.rs` L~1580

递归下载目录时没有深度限制。如果服务器端存在符号链接循环（如 `dir/link -> ..`），或恶意服务器构造极深目录结构，可能导致：
- 栈溢出（递归深度过大）
- 磁盘耗尽
- 路径过长错误

**影响**：恶意或配置不当的服务器可导致客户端崩溃或磁盘耗尽。

**建议**：
1. 添加最大递归深度（如 64 层）。
2. 跟踪已访问的 inode（Unix）或文件索引（Windows）检测循环。
3. 跟踪已下载总字节数，超过上限时停止。

---

### M6. 文件传输无校验和

**位置**：`thru-fs/src/lib.rs` `handle_get`/`get_on_conn`

文件流式传输只有大小头 + 空终止符，没有 CRC/哈希校验。网络传输中的比特翻转（虽然 TCP 有校验和，但应用层无法检测内存损坏或中间盒篡改）无法检测。

**影响**：低概率，但下载的文件可能静默损坏。

**建议**：可选地在传输末尾附加 SHA-256 校验和，客户端验证。

---

### M7. 错误处理不一致，服务端可能静默失败

**位置**：全项目

- 有些地方用 `?` 传播错误（如 `auth_server`）
- 有些地方用 `let _ =` 忽略（如服务端 dispatch 中的 `write_frame`）
- 有些地方用 `eprintln! + exit(1)`（如客户端命令）
- 有些地方用 `unwrap()`（如 `dict.handle` 中的 mutex lock）

服务端请求处理中大量 `let _ = write_frame(...)` 意味着：如果客户端在请求处理中断开，服务端不会记录任何日志，也不会提前终止处理（可能继续执行 PTY 启动等昂贵操作）。

**影响**：生产环境难以排查问题，可能有资源泄漏。

**建议**：
1. 引入 `log` crate，统一日志级别。
2. 服务端处理函数返回 `io::Result<()>`，调用方记录错误日志。
3. 消除所有 `let _ =` 对 `Result` 的忽略（至少 `if let Err(e) = ... { warn!(...) }`）。

---

## 低风险

### L1. 无单元测试/集成测试

项目只有 `examples/` 下的手动测试脚本，没有 `#[test]` 函数。核心协议编解码（`thru-core` 的帧读写、`thru-dict` 的请求编码、`thru-fs` 的路径解析）没有自动化测试覆盖。

**影响**：重构时容易引入回归，协议变更时难以验证兼容性。

**建议**：至少为 `thru-core`、`thru-dict`、`thru-fs` 的编解码函数添加单元测试。

---

### L2. 无日志框架，全部用 `eprintln!`

服务端的诊断信息（反向客户端注册/断开、max devices 拒绝等）用 `eprintln!` 输出。无法配置日志级别，无法重定向到文件，生产环境排查问题困难。

**建议**：引入 `log` + `env_logger`，用 `info!`/`warn!`/`error!` 替代 `eprintln!`。

---

### L3. `thru-exec` Unix 端硬编码 `-e -c`，不兼容所有 shell

**位置**：`thru-exec/src/lib.rs` L210

```rust
Command::new(&shell).arg("-e").arg("-c").arg(&script)
```

`default_shell()` 可能返回用户配置的任意 shell（如 `fish`、`tcsh`），但 `-e`（set -e，遇错退出）和 `-c`（执行命令字符串）是 POSIX sh 风格参数，fish 不支持 `-e`，tcsh 的 `-e` 含义不同。

**影响**：如果用户设置 `THRU_SHELL=fish`，`exec` 命令可能行为异常或失败。

**建议**：
1. 检测 shell 类型，使用对应参数。
2. 或文档中明确 `exec` 仅支持 POSIX 兼容 shell，`THRU_SHELL` 主要影响交互式 `shell`。

---

### L4. `handle_shell_open` 中 PTY 子进程可能残留

**位置**：`thru-exec/src/lib.rs` L138-188

`handle_shell_open` 在循环中调用 `session.try_wait()?`，如果 `try_wait` 返回 Err，函数提前返回 `Err`，但 PTY 子进程可能还在运行。`PtySession` drop 时是否会 kill 子进程取决于 `portable-pty` 的实现，不能保证。

**影响**：长时间运行后可能有僵尸 shell 进程残留。

**建议**：在 `PtySession::drop` 中显式 `let _ = self.child.kill();`，或确认 `portable-pty` 的 Child drop 行为。

---

### L5. `fetch2_interactive` 大文件下载无进度反馈

**位置**：`main.rs` L~1560

交互式浏览器中下载文件时调用 `thru_fs::get_on_conn`，该函数没有进度回调。下载大文件（如几个 GB）时界面完全卡死，用户无法判断是在下载还是已经挂了。

**影响**：用户体验差，可能误以为程序崩溃而强制终止。

**建议**：使用 `thru_fs::client_get_progress`（已存在！）替代 `get_on_conn`，在 TUI 中显示进度条。注意 `client_get_progress` 会新建连接，需要改造为接受已有连接的版本。

---

### L6. `auth_client` 交互式密码输入失败时返回 `Ok((false, None))`

**位置**：`main.rs` L~920

```rust
match read_password() {
    Ok(p) => p,
    Err(e) => {
        eprintln!("{e}");
        return Ok((false, None));  // ← IO 错误被包装成"认证失败"
    }
}
```

`read_password` 失败（如 raw_mode 启用失败）时返回 `Ok((false, None))`，调用方无法区分"密码错误"和"终端 IO 错误"。

**建议**：返回 `io::Result<(bool, Option<String>)>`，IO 错误用 `Err` 传播。

---

## 风险汇总表

| ID | 风险 | 等级 | 前置条件 | 影响 |
|----|------|------|----------|------|
| H1 | 明文密码传输 | 高 | 中间人位置 | 密码泄露 → 完整 RCE |
| H2 | 无暴力破解防护 | 高 | 网络可达 | 弱密码秒破 |
| H3 | 任意文件读取 | 高 | 已认证 | 服务器文件系统完全暴露 |
| H4 | 远程代码执行 | 高 | 已认证 | 服务器完全被控 |
| H5 | read_frame 无长度限制 | 高 | 网络可达（未认证） | 服务器 OOM 崩溃 |
| H6 | dict 无内存限制 | 高 | 已认证 | 服务器 OOM 崩溃 |
| H7 | 会话文件明文存密码 | 高 | 本地多用户 | 密码被其他用户窃取 |
| M1 | PID 复用误杀 | 中 | 进程退出+PID 复用 | 误杀无辜进程 |
| M2 | daemon 启动不确认 | 中 | 端口被占用 | 用户误以为服务已启动 |
| M3 | 反向隧道双向 RCE | 中 | 任一节点被攻陷 | 横向移动 |
| M4 | 心跳线程竞争 | 中 | 设备快速重连 | 设备状态短暂不一致 |
| M5 | 递归下载无深度限制 | 中 | 恶意服务器 | 客户端栈溢出/磁盘耗尽 |
| M6 | 文件传输无校验和 | 中 | 网络比特翻转 | 静默文件损坏 |
| M7 | 错误处理不一致 | 中 | 生产环境 | 难以排查，资源泄漏 |
| L1 | 无自动化测试 | 低 | 重构/协议变更 | 回归风险 |
| L2 | 无日志框架 | 低 | 生产环境 | 排查困难 |
| L3 | exec 不兼容非 POSIX shell | 低 | THRU_SHELL=fish | exec 命令异常 |
| L4 | PTY 子进程可能残留 | 低 | 异常断开 | 僵尸进程 |
| L5 | 大文件下载无进度 | 低 | 下载大文件 | 用户体验差 |
| L6 | auth_client 错误混淆 | 低 | 终端 IO 异常 | 错误信息误导 |

---

## 审查结论

**代码质量整体评价**：中等偏上。

- **优点**：crate 分层清晰（core/transport/proto-tcp/dict/fs/exec），协议设计有 opcode 分区，帧格式统一，编译零警告，`client_get` 复用 `client_get_progress` 的设计不错。
- **主要问题**：`main.rs` 严重膨胀（1741 行），魔法数字和裸状态码遍布，错误处理不一致（大量 `let _ =`），安全方面有多个高风险漏洞（明文密码、任意文件读取、无 OOM 防护）。
- **优先修复建议**：
  1. **安全**：H5（read_frame 长度限制，一行代码的事）→ H1/H2（TLS + 暴力破解防护）→ H3（文件系统沙箱）→ H7（会话文件权限）
  2. **可维护性**：R1（main.rs 拆分）→ R2（连接样板提取）→ U1/U2（魔法数字替换为常量）
  3. **优雅度**：R8（TUI RAII guard）→ U3（参数解析）→ U5（消除 `let _ =`）
