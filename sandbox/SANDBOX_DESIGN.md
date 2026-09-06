# thru 沙箱测试方案

## 设计目标
在不影响真实系统的前提下，对 thru 的全部功能进行自动化测试。通过隔离临时目录、自定义端口和环境变量，实现完全可复现的测试环境。

## 沙箱目录结构
```
sandbox/
├── run-tests.ps1            # 自动化测试脚本
├── SANDBOX_DESIGN.md        # 本文档
├── server-root/              # 模拟远程文件系统（服务端 CWD）
│   ├── README.md
│   ├── documents/
│   │   ├── report.txt
│   │   └── notes.md
│   ├── downloads/
│   │   └── bigfile.bin       # 512KB 大文件
│   └── source/src/main.rs
├── client-download/          # 客户端下载目标目录
├── temp/                     # 隔离的 TEMP（PID 文件、session 缓存）
├── logs/                     # 日志
└── test-results.log          # 测试结果
```

## 环境隔离
| 隔离项 | 实现方式 |
|---|---|
| 端口 | 自定义 16196（避开默认 61696） |
| PID/session 文件 | `$env:TEMP`/`$env:TMP` 指向 `sandbox/temp` |
| 服务端 CWD | 启动前 `cd` 到 `sandbox/server-root` |
| 密码 | 测试密码 `testpass123`，验证挑战-响应握手 |
| 残留进程 | 测试前 `Stop-Process thru` 强制清理 |

## 测试用例（32项）
- **服务端生命周期** T01-T04：启动、pid、重复启动检测、停止
- **认证** T05-T06：正确密码、错误密码
- **字典** T07-T14：set/get/不存在/append/list/-kv/1MB大值/10MB超限
- **执行** T15-T17：简单命令、失败命令、带参数
- **文件系统** T18-T23：LS、GET小文件、GET大文件、内容校验、不存在文件、路径遍历
- **反向隧道** T24-T27：connect、PID文件、设备列表、停止
- **设备管理** T28-T29：设备列表、目标切换
- **压力** T30-T32：并发操作、MD5完整性、LRU驱逐

## 使用方式
```powershell
.\sandbox\run-tests.ps1              # 完整测试
.\sandbox\run-tests.ps1 -SkipBuild   # 跳过构建
.\sandbox\run-tests.ps1 -Port 16196  # 指定端口
.\sandbox\run-tests.ps1 -KeepSandbox  # 保留沙箱
.\sandbox\run-tests.ps1 -Groups dict,fs  # 仅运行指定组
```

## 交互式功能（手动测试）
- `thru shell` — 终端、Ctrl+] 退出、窗口 resize
- `thru fetch2` — 方向键导航、Enter 下载、左方向键到父目录
- `thru device` — 设备选择、上下键、Enter 确认

## 测试结果（2026-09-05）
```
Total:   29
Passed:  28
Failed:  0
Skipped: 1 (T31 Large file MD5 — download 命令不存在，用 fetch2 代替)
```

### 测试过程中发现并修复的 thru 代码问题
1. **load_session BOM 处理** — Windows 上 session 缓存文件可能带 UTF-8 BOM，导致地址解析失败。修复：读取后 `strip_prefix('\u{feff}')`。
2. **fetch2 stdin BOM 处理** — PowerShell 管道传递 stdin 时添加 UTF-8 BOM，导致远程路径前带 BOM 字符，`fs::metadata` 失败。修复：`line.trim().strip_prefix('\u{feff}')`。
3. **fetch2 失败残留空文件** — 非交互式下载在 `get_on_conn` 之前就创建本地文件，远程文件不存在时留下空文件。修复：`get_on_conn` 失败时 `drop(f)` 后 `remove_file`。
4. **dict append -a 解析 bug** — 简写格式 `dict <key> <value> -a` 中，`-a` 被错误地包含在 value 中（value 变成 "2 -a"）。修复：计算 value 时过滤掉 `-a` 参数。

### 测试脚本关键设计
- **服务端/reverse 启动**：使用 .NET `ProcessStartInfo` 显式设置 TEMP/TMP 环境变量，避免 PowerShell `Start-Process` 不继承环境变量的问题；同时避免 PowerShell 等待 daemon 子进程。
- **大值传递**：1MB+ 的 dict value 通过 stdin 管道传递，避免 Windows 命令行 8191 字符限制。
- **fetch2 stdin**：直接字符串管道传递远程路径，thru 端处理 BOM。
- **reverse connect**：启动后轮询 PID 文件出现，不等待父进程退出（Windows 上 daemon 子进程可能导致父进程句柄不释放）。
