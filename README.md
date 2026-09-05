# thru

Cross-platform device communication tool for personal use. Run a lightweight daemon on one machine, then read/write a shared dictionary, transfer files, and execute commands from another machine — all over a single TCP connection.

## Features

- **Daemon server** — runs in the background without blocking the terminal; start/stop like a service
- **One-shot client commands** — each operation connects, executes, and exits (like `echo`); only `thru shell` enters interactive mode
- **Shared dictionary** — `thru dict set/get/list`, supports append mode for logging scenarios, supports pipes
- **File transfer** — `thru fetch2 ./dir` interactive remote file browser or direct piped download; large files streamed in 1 MiB chunks
- **Remote command execution** — `thru exec "command"` one-shot execution; `thru shell` enters a full interactive PTY (like SSH)

## Build

```bash
cargo build --release
```

The binary is located at `target/release/thru`.

## Quick Start

### 1. Start the daemon (Machine A)

```bash
thru [port] [-p password]
# default port 61696
thru
thru 222
thru 222 -p mysecret
```

The server daemonizes immediately and returns the shell. PID is written to `$TEMP/thru_server.pid` (Windows) or `/tmp/thru_server.pid` (Unix).

### 2. Connect and execute commands (Machine B)

```bash
thru 10.255.255.254:22  # client connect
# password: _  (if the server has a password, enter it when prompted)

# shared dictionary
thru dict set greeting "hello from B"
thru dict get greeting
thru dict list

# remote command
thru exec "whoami && hostname"

# file transfer
thru fetch2 ./downloads

# interactive shell
thru shell
```

### 3. Check Server Status

```bash
thru pid
# server pid: 12345
# active connections: 2
#   1: 192.168.1.10:54321
#   2: 10.0.0.5:33445
```

### 4. Stop the daemon

```bash
thru stop
```

## Usage

### Server Daemon

```bash
thru [port] [-p password]      # start daemon (background)
thru stop                        # stop the running daemon
```

- Default port: `61696`
- If `-p` is set, all clients must authenticate with the correct password
- The daemon is peer-to-peer: any connected client can use all features
- Argument validation: the server positional argument must be a valid port (1-65535), and the client must be a valid `host:port`; invalid arguments error out instead of silently starting a server

### Client Connect

```bash
thru <host:port> [-p password]
```

Establishes a connection and caches the session (address + password) for subsequent commands.

- If the server requires a password and none is provided via `-p`, you will be prompted interactively
- On success, prints the server PID and active connection count: `connected to <addr> (server pid: 12345, 1 active connection)`
- Password is only used here; subcommands do not accept `-p`

### Server Status

```bash
thru pid
```

Shows the server PID, active connection count, and remote IP for each connection. Errors if no server is running.

```
server pid: 12345
active connections: 2
  1: 192.168.1.10:54321
  2: 10.0.0.5:33445
```

### Client Connect

```bash
thru <host:port> [-p password]
```

Establishes a connection to the server, authenticates, and caches the session (address + password) for subsequent commands.

- If the server requires a password and none is provided via `-p`, you will be prompted interactively
- On success, prints `connected to <host:port>` and exits
- The cached session is used automatically by all subsequent subcommands

### Shared Dictionary

```bash
# set a value
thru dict set mykey "my value"
thru dict set mykey myvalue          # quotes optional for single words

# get a value
thru dict get mykey

# append (like >> )
thru dict set -a mykey " more text"
echo "log line" | thru dict set -a mykey

# list
thru dict list              # keys only
thru dict list -v           # values only
thru dict list -kv          # key<TAB>value pairs
```

Pipe behavior: if `stdin` is not a terminal and no value is provided on the command line, the value is read from stdin. Supports `echo "data" | thru dict set mykey`.

### Remote Command Execution

```bash
# one-shot command (connect, execute, exit)
thru exec "ls -la"
thru exec "cd /tmp && mkdir test && ls"
```

Commands execute in the daemon's shell environment. On Unix the shell is `$SHELL` (fallback `/bin/sh`); on Windows it is `%COMSPEC%` (fallback `cmd.exe`). Can be overridden with the `THRU_SHELL` environment variable.

### Interactive Remote Shell

```bash
thru shell
```

Enters interactive mode. Opens a full PTY session on the remote machine.

- `Ctrl+]` — exit the remote shell, return to the local terminal
- Terminal size is synchronized automatically
- `Ctrl+C` / `Ctrl+D` are sent to the remote side

### File Transfer

```bash
# interactive browser (runs in the terminal)
thru fetch2 ./downloads

# direct download (pipe the remote path)
echo "/var/log/syslog" | thru fetch2 ./downloads

# specify server address
thru fetch2 ./downloads --connect 192.168.1.5:61696
```

Interactive browser controls:
- `↑`/`↓` — select entry
- `→`/`Enter` — enter directory
- `←` — go to parent directory
- `Enter` on a file — download
- `Enter` twice on a directory — confirm recursive download
- `Esc`/`Ctrl+C` — quit

Large files are streamed in 1 MiB chunks (not loaded entirely into memory).

### Password Authentication

Password is only used during the initial connection. Subsequent subcommands automatically reuse the cached session password — no need to enter it again.

```bash
# server sets a password
thru 61696 -p mysecret

# client connect (if the server requires a password, you will be prompted interactively)
thru 10.255.255.254:22
password: mysecret
connected to 10.255.255.254:22

# you can also specify the password directly at connect time (non-interactive)
thru 10.255.255.254:22 -p mysecret

# after connecting, subsequent commands do not need a password
thru dict get mykey
thru exec "whoami"
thru shell
```

Subcommands (`dict` / `exec` / `fetch2` / `shell`) do not accept the `-p` flag. If the server requires a password but no connection has been established, subcommands will error and prompt you to run `thru <host:port>` first.

### Server Address Resolution

All client subcommands resolve the server address in the following order:
1. `--connect <addr>` flag
2. Cached client session (automatically cached after `thru <host:port>`)
3. `THRU_SERVER` environment variable
4. `127.0.0.1:61696` (default)

```bash
export THRU_SERVER=192.168.1.5:61696
thru dict get mykey
thru exec "whoami"
thru fetch2 ./downloads
```

## Project Structure

```
thru/
├── Cargo.toml                  # Workspace root + binary package
├── README.md                   # This document (English)
├── README.zh.md                # Chinese documentation
├── src/
│   └── main.rs                 # CLI entry: daemon server / client connect / one-shot commands / shell
├── examples/
│   ├── fs_test.rs              # File transfer unit tests
│   ├── fs_dir_test.rs          # Recursive directory download tests
│   └── integration_test.rs     # Full integration tests (22 items)
└── crates/
    ├── thru-core/              # Frame codec: 4-byte big-endian length prefix + payload
    ├── thru-transport/         # Connection abstraction: Transport / Listener / Connection traits
    ├── thru-proto-tcp/         # TCP protocol implementation
    ├── thru-dict/              # Shared dictionary (set/get/append/list/keys)
    ├── thru-fs/                # File transfer (LS/GET, 1 MiB chunked streaming)
    └── thru-exec/              # Remote execution (interactive PTY + script execution)
```

### Crate Responsibilities

| Crate | Responsibility |
|---|---|
| `thru-core` | Wire format: length-prefixed frame read/write |
| `thru-transport` | Pluggable protocol trait abstraction; `Server` wrapper |
| `thru-proto-tcp` | TCP implementation of `Transport`/`Listener`/`Connection` |
| `thru-dict` | In-memory shared key-value store; binary-safe values |
| `thru-fs` | Remote directory listing and streaming file download |
| `thru-exec` | PTY session management and script execution with error abort |

## Protocol

All communication uses length-prefixed frames. The first byte of each request frame is an opcode:

| Range | Function |
|---|---|
| 0–9 | Dictionary (GET/SET/APPEND/DEL/KEYS/ALL) |
| 20–29 | File system (LS/GET) |
| 30–39 | Execution (SHELL_OPEN/RESIZE/EXEC_SCRIPT) |
| 50 | Server status (PID / connection count / remote IP list) |
| 0x40–0x44 | Authentication (REQUIRED/NONE/OK/FAIL/PASS) |

## Testing

```bash
# Full integration tests (dict + fs + exec, 22 items)
cargo run --example integration_test
```

## Tech Stack

- **Rust 2024 edition**
- **crossterm** — cross-platform terminal raw mode, keyboard events, resize
- **portable-pty** — cross-platform pseudo-terminal (Unix PTY / Windows ConPTY)
- No other runtime dependencies

## License

Personal use project.
