# thru

A cross-platform device communication tool for personal use. Connect two machines and share a dictionary, transfer files, and execute remote commands — all over a single connection.

## Features

- **Unified connection abstraction** — pluggable protocols (TCP built-in), server/client model
- **Shared dictionary** — `thru dict key` reads/writes a distributed key-value store; append mode for log-style usage
- **File transfer** — `thru fetch2 ./dir` opens an interactive remote file browser; pipe a remote path for direct download; large files stream in 1 MiB chunks
- **Remote command execution** — `thru host:port` opens an interactive PTY shell (SSH-like); pipe a script for non-interactive execution with error abort

## Build

```bash
cargo build --release
```

The binary is at `target/release/thru`.

## Quick Start

### Start a server

```bash
thru [port] [-protocol] [-p password]
# default port 61696, default protocol tcp
thru
thru 222
thru 222 -tcp
thru 222 -p mysecret
```

If `-p` is set, clients must provide the correct password before the session starts.

### Connect as a client

```bash
thru <host:port> [-protocol] [-p password]
thru 192.168.1.5:61696
thru 192.168.1.5:61696 -p mysecret
```

If the server requires a password and `-p` is not provided, you will be prompted interactively (input is hidden).

Once connected, both sides enter a **symmetric session mode** — either side can issue commands to the other. Type `help` to see available commands, `exit` to disconnect.

```
thru> help
commands:
  dict <key> [value]      read/write remote dict
  dict list                list remote dict keys
  ls [path]                list remote directory
  get <remote> [local]     download remote file
  exec <command>           execute command on remote side
  shell                     enter interactive remote PTY shell (Ctrl+] to exit)
  exit                      disconnect
```

In session mode, both server and client are functionally equal — either can read the other's dictionary, download files, or execute commands. The only difference is who initiates the connection.

## Usage

### Symmetric Session Mode (primary interface)

```bash
# Machine A: listen
thru 61696

# Machine B: connect
thru 192.168.1.5:61696

# Now both sides have a "thru> " prompt and can command each other:
thru> dict set greeting hello from A
thru> dict get greeting
thru> ls .
thru> exec whoami
thru> get remote_file.txt local_copy.txt
thru> shell          # enter full interactive remote terminal (Ctrl+] to return)
thru> exit
```

### One-shot Commands (alternative interface)

These connect, execute, and disconnect — useful for scripts.

### Shared Dictionary

```bash
# Set (pipe value to stdin)
echo "hello world" | thru dict msgA

# Read (run in a terminal — no stdin pipe)
thru dict msgA

# Append (like >> )
cat log.txt | thru dict -a msgA

# List all keys
thru dict list
thru dict list -v    # values only
thru dict list -kv   # key<TAB>value pairs
```

The dictionary is local to each side; in session mode you read/write the **remote** side's dictionary.

### File Transfer

```bash
# Interactive browser (run in a terminal)
thru fetch2 ./downloads

# Direct download (pipe the remote path)
echo "/var/log/syslog" | thru fetch2 ./downloads

# Override server address
thru fetch2 ./downloads --connect 192.168.1.5:61696
```

Interactive browser controls:
- `↑`/`↓` — select entry
- `→`/`Enter` — enter directory
- `←` — go up a directory
- `Enter` on a file — download it
- `Enter` twice on a directory — confirm recursive download
- `q`/`Esc` — quit

Large files are streamed in 1 MiB chunks (not buffered entirely in memory).

### Remote Command Execution

```bash
# Interactive PTY shell (SSH-like)
thru 192.168.1.5:61696

# Non-interactive script execution
echo "ls -la" | thru 192.168.1.5:61696

# Multi-line script with cd state persistence
cat <<'EOF' | thru 192.168.1.5:61696
cd /tmp
mkdir testdir
cd testdir
echo "hello" > file.txt
cat file.txt
EOF
```

In non-interactive mode, any command that fails aborts the rest of the script (like `set -e`), and the exit code propagates to the local process.

### Server address configuration

All client subcommands resolve the server address in this order:
1. `--connect <addr>` flag
2. `THRU_SERVER` environment variable
3. `127.0.0.1:61696` (default)

```bash
export THRU_SERVER=192.168.1.5:61696
thru dict msgA
thru fetch2 ./downloads
```

## Project Structure

```
thru/
├── Cargo.toml                  # Workspace root + binary package
├── README.md                   # This file (English)
├── README.zh.md                # Chinese documentation
├── src/
│   └── main.rs                 # CLI entry: server / client(PTY) / dict / fetch2
├── examples/
│   ├── fs_test.rs              # File transfer unit tests
│   ├── fs_dir_test.rs          # Recursive directory download test
│   └── integration_test.rs     # Full integration test (22 cases)
└── crates/
    ├── thru-core/              # Frame codec: 4-byte big-endian length + payload
    ├── thru-transport/         # Connection abstraction: Transport / Listener / Connection traits
    ├── thru-proto-tcp/         # TCP protocol implementation
    ├── thru-dict/              # Shared dictionary (set/get/append/list/keys)
    ├── thru-fs/                # File transfer (LS/GET, 1 MiB chunk streaming)
    └── thru-exec/              # Remote execution (interactive PTY + script execution)
```

### Crate responsibilities

| Crate | Responsibility |
|---|---|
| `thru-core` | Wire format: length-prefixed frame read/write |
| `thru-transport` | Trait abstraction for pluggable protocols; `Server` wrapper |
| `thru-proto-tcp` | TCP implementation of `Transport`/`Listener`/`Connection` |
| `thru-dict` | In-memory shared key-value store; binary-safe values |
| `thru-fs` | Remote directory listing and streaming file download |
| `thru-exec` | PTY session management and script execution with error abort |

## Protocol

All communication uses length-prefixed frames over the chosen transport. The first byte of each request frame is an operation code:

| Range | Feature |
|---|---|
| 0–9 | Dictionary (GET/SET/APPEND/DEL/KEYS/ALL) |
| 20–29 | File system (LS/GET) |
| 30–39 | Execution (SHELL_OPEN/RESIZE/EXEC_SCRIPT) |

## Testing

```bash
# Full integration test (dict + fs + exec, 22 cases)
cargo run --example integration_test
```

## Tech Stack

- **Rust 2024 edition**
- **crossterm** — cross-platform terminal raw mode, keyboard events, resize handling
- **portable-pty** — cross-platform pseudo-terminal (Unix PTY / Windows ConPTY)
- Zero other runtime dependencies

## License

Personal use project.
