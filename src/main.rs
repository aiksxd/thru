use std::collections::VecDeque;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use thru_core::{read_frame, write_frame};
use thru_dict::Dict;
use thru_exec;
use thru_fs;
use thru_transport::{connect, Connection, Server, Transport};
use thru_proto_tcp::Tcp;

// CLI entry point.
//   server: thru [port] [-proto]
//   client: thru <host:port> [-proto]
//   dict:   thru dict [-a] <key>
//           thru dict list [-v|-kv]
//           (--connect <addr> overrides THRU_SERVER env)

/// Default listen port when none is given.
const PORT: u16 = 61696;

/// Resolve a protocol name to a Transport implementation.
fn transport(p: &str) -> Box<dyn Transport> {
    match p {
        "tcp" => Box::new(Tcp),
        _ => panic!("unsupported protocol: -{p}"),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // dict subcommand takes over the entire argument space.
    if args.first().map(|s| s.as_str()) == Some("dict") {
        return dict_cmd(&args[1..]);
    }

    // fetch2 subcommand: file download (non-interactive + interactive).
    if args.first().map(|s| s.as_str()) == Some("fetch2") {
        return fetch2_cmd(&args[1..]);
    }

    // Parse args: target (port or host:port), -p password, -protocol.
    let mut password: Option<String> = None;
    let mut proto = "tcp".to_string();
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-p" && i + 1 < args.len() {
            password = Some(args[i + 1].clone());
            i += 2;
        } else if a.starts_with('-') {
            proto = a.trim_start_matches('-').to_string();
            i += 1;
        } else {
            target = Some(a.clone());
            i += 1;
        }
    }
    let t = transport(&proto);
    let t: &dyn Transport = &*t;

    match target {
        Some(a) if a.contains(':') => client(t, &a, password.as_deref()),
        Some(p) => server(t, &format!("0.0.0.0:{p}"), password.as_deref()),
        None => server(t, &format!("0.0.0.0:{PORT}"), password.as_deref()),
    }
}

// --- authentication handshake ---

const AUTH_REQUIRED: u8 = 0x40;
const AUTH_NONE: u8 = 0x41;
const AUTH_OK: u8 = 0x42;
const AUTH_FAIL: u8 = 0x43;
const AUTH_PASS: u8 = 0x44;

/// Server-side auth: if password is set, challenge the client; else signal no-auth.
fn auth_server(conn: &mut Box<dyn Connection>, password: Option<&str>) -> io::Result<bool> {
    match password {
        Some(pw) => {
            write_frame(conn, &[AUTH_REQUIRED])?;
            let f = read_frame(conn)?;
            if f.len() < 3 || f[0] != AUTH_PASS {
                write_frame(conn, &[AUTH_FAIL])?;
                return Ok(false);
            }
            let plen = u16::from_be_bytes([f[1], f[2]]) as usize;
            if f.len() < 3 + plen {
                write_frame(conn, &[AUTH_FAIL])?;
                return Ok(false);
            }
            let client_pw = String::from_utf8_lossy(&f[3..3 + plen]);
            if client_pw == pw {
                write_frame(conn, &[AUTH_OK])?;
                Ok(true)
            } else {
                write_frame(conn, &[AUTH_FAIL])?;
                Ok(false)
            }
        }
        None => {
            write_frame(conn, &[AUTH_NONE])?;
            Ok(true)
        }
    }
}

/// Client-side auth: read server's auth signal, send password if required.
fn auth_client(conn: &mut Box<dyn Connection>, password: Option<&str>) -> io::Result<bool> {
    let f = read_frame(conn)?;
    if f.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, "connection closed during auth"));
    }
    match f[0] {
        AUTH_NONE => Ok(true),
        AUTH_REQUIRED => {
            let pw = match password {
                Some(p) => p.to_string(),
                None => read_password()?,
            };
            let mut frame = vec![AUTH_PASS];
            frame.extend_from_slice(&(pw.len() as u16).to_be_bytes());
            frame.extend_from_slice(pw.as_bytes());
            write_frame(conn, &frame)?;
            let resp = read_frame(conn)?;
            match resp.first() {
                Some(&AUTH_OK) => Ok(true),
                Some(&AUTH_FAIL) => {
                    eprintln!("authentication failed");
                    Ok(false)
                }
                _ => Err(io::Error::new(io::ErrorKind::Other, "invalid auth response")),
            }
        }
        _ => Err(io::Error::new(io::ErrorKind::Other, "invalid auth handshake")),
    }
}

/// Read a password from stdin without echoing.
fn read_password() -> io::Result<String> {
    use crossterm::event::{self, KeyCode, KeyEventKind};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    eprint!("password: ");
    io::stderr().flush()?;
    enable_raw_mode()?;
    let mut pw = String::new();
    let result = (|| -> io::Result<String> {
        loop {
            if let event::Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Enter => break,
                        KeyCode::Backspace => { pw.pop(); }
                        KeyCode::Char(c) => pw.push(c),
                        _ => {}
                    }
                }
            }
        }
        Ok(pw)
    })();
    let _ = disable_raw_mode();
    eprintln!();
    result
}

// --- server / client ---

/// Listen and accept the first connection; authenticate; enter symmetric session mode.
fn server(t: &dyn Transport, addr: &str, password: Option<&str>) -> io::Result<()> {
    let mut s = Server::bind(t, addr)?;
    eprintln!("thru listen {addr}");
    let mut conn = s.accept()?;
    eprintln!("connected");
    if !auth_server(&mut conn, password)? {
        eprintln!("authentication failed, disconnecting");
        return Ok(());
    }
    session_mode(conn)
}

/// Connect to a remote peer; authenticate; enter symmetric session mode.
fn client(t: &dyn Transport, addr: &str, password: Option<&str>) -> io::Result<()> {
    let mut conn = connect(t, addr)?;
    if !auth_client(&mut conn, password)? {
        std::process::exit(1);
    }
    session_mode(conn)
}

// --- symmetric session mode ---

/// A thread-safe queue for response frames read by the background reader.
struct RespQueue {
    q: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
}

impl RespQueue {
    fn new() -> Self {
        Self { q: Mutex::new(VecDeque::new()), cv: Condvar::new() }
    }
    fn push(&self, f: Vec<u8>) {
        let mut q = self.q.lock().unwrap();
        q.push_back(f);
        self.cv.notify_one();
    }
    fn pop(&self) -> io::Result<Vec<u8>> {
        let mut q = self.q.lock().unwrap();
        while q.is_empty() {
            q = self.cv.wait(q).unwrap();
        }
        Ok(q.pop_front().unwrap())
    }
}

/// Symmetric session: both sides can issue requests and respond to them.
/// Background thread handles incoming requests; foreground reads local commands.
fn session_mode(mut conn: Box<dyn Connection>) -> io::Result<()> {
    let dict = Arc::new(Dict::new());
    let waiting = Arc::new(AtomicBool::new(false));
    let rq = Arc::new(RespQueue::new());

    // Background reader: dispatch requests or queue responses for foreground.
    let mut rc = conn.try_clone()?;
    let d2 = dict.clone();
    let w2 = waiting.clone();
    let rq2 = rq.clone();
    thread::spawn(move || {
        while let Ok(f) = read_frame(&mut rc) {
            if w2.load(Ordering::SeqCst) {
                // Foreground is waiting for a response; queue every frame (including empty terminators).
                rq2.push(f);
            } else {
                if f.is_empty() { continue; }
                match f[0] {
                    0..=9 => { let r = d2.handle(&f); let _ = write_frame(&mut rc, &r); }
                    20..=29 => { let _ = thru_fs::handle(&mut rc, &f); }
                    30 => { let _ = thru_exec::handle_shell_open(&mut rc, &f); }
                    32 => { let _ = thru_exec::handle_exec_script(&mut rc, &f); }
                    _ => {}
                }
            }
        }
    });

    // Helpers for request/response on the shared connection.
    let send_req = |conn: &mut Box<dyn Connection>, frame: &[u8]| -> io::Result<()> {
        waiting.store(true, Ordering::SeqCst);
        write_frame(conn, frame)
    };
    let recv = || -> io::Result<Vec<u8>> { rq.pop() };
    let done = || { waiting.store(false, Ordering::SeqCst); };

    println!("thru session connected. type 'help' for commands, 'exit' to quit.");
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("thru> ");
        stdout.flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 { break; }
        let line = line.trim();
        if line.is_empty() { continue; }
        if line == "exit" || line == "quit" { break; }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() { continue; }
        let result = match parts[0] {
            "help" => { print_session_help(); Ok(()) }
            "dict" => session_dict(&mut conn, &parts[1..], &send_req, &recv, &done),
            "ls" => session_ls(&mut conn, parts.get(1).copied().unwrap_or("."), &send_req, &recv, &done),
            "get" => session_get(&mut conn, &parts[1..], &send_req, &recv, &done),
            "exec" => session_exec(&mut conn, line.strip_prefix("exec ").unwrap_or("").trim_start(), &send_req, &recv, &done),
            "shell" => session_shell(&mut conn, &waiting, &rq),
            _ => { eprintln!("unknown command: {}. type 'help'", parts[0]); Ok(()) }
        };
        if let Err(e) = result {
            eprintln!("error: {e}");
            done();
        }
    }

    let _ = write_frame(&mut conn, &[]);
    Ok(())
}

fn print_session_help() {
    println!("commands:");
    println!("  dict <key>              read a value from the remote dict");
    println!("  dict <key> <value>      write a value to the remote dict");
    println!("  dict -a <key> <value>   append to a remote dict value");
    println!("  dict list                list all remote dict keys");
    println!("  ls [path]                list remote directory");
    println!("  get <remote> [local]     download a remote file");
    println!("  exec <command>           execute a command on the remote side");
    println!("  shell                     enter interactive remote PTY shell (Ctrl+] to exit)");
    println!("  help                      show this help");
    println!("  exit                      disconnect");
}

/// dict command inside a session.
fn session_dict(
    conn: &mut Box<dyn Connection>,
    args: &[&str],
    send_req: &dyn Fn(&mut Box<dyn Connection>, &[u8]) -> io::Result<()>,
    recv: &dyn Fn() -> io::Result<Vec<u8>>,
    done: &dyn Fn(),
) -> io::Result<()> {
    use thru_dict::{req, OP_APPEND, OP_GET, OP_KEYS, OP_SET, ST_NOT_FOUND, ST_OK};

    if args.first() == Some(&"list") {
        send_req(conn, &req(OP_KEYS, &[], &[]))?;
        let r = recv()?;
        done();
        if r[0] != ST_OK { return Err(io::Error::new(io::ErrorKind::Other, "dict error")); }
        let payload = &r[1..];
        if payload.is_empty() {
            println!("(empty)");
        } else {
            println!("{}", String::from_utf8_lossy(payload).replace('\n', "\n"));
        }
        return Ok(());
    }

    let (append, is_get, rest) = match args.first() {
        Some(&"-a") => (true, false, &args[1..]),
        Some(&"set") => (false, false, &args[1..]),
        Some(&"get") => (false, true, &args[1..]),
        _ => (false, false, args),
    };
    let key = match rest.first() {
        Some(k) => k.to_string(),
        None => { eprintln!("usage: dict <key> [value] | dict get <key> | dict set <key> <value> | dict -a <key> <value> | dict list"); return Ok(()); }
    };
    let want_read = is_get || rest.len() <= 1;

    if want_read {
        send_req(conn, &req(OP_GET, key.as_bytes(), &[]))?;
        let r = recv()?;
        done();
        match r[0] {
            ST_OK => { io::stdout().write_all(&r[1..])?; io::stdout().write_all(b"\n")?; }
            ST_NOT_FOUND => { eprintln!("key not found: {key}"); }
            _ => return Err(io::Error::new(io::ErrorKind::Other, "dict read failed")),
        }
        return Ok(());
    }

    // Write/append: value is remaining args joined with spaces.
    let value_str = rest[1..].join(" ");
    let value = value_str.as_bytes();
    if append {
        send_req(conn, &req(OP_APPEND, key.as_bytes(), value))?;
    } else {
        send_req(conn, &req(OP_SET, key.as_bytes(), value))?;
    }
    let r = recv()?;
    done();
    if r[0] != ST_OK { return Err(io::Error::new(io::ErrorKind::Other, "dict write failed")); }
    println!("ok");
    Ok(())
}

/// ls command inside a session.
fn session_ls(
    conn: &mut Box<dyn Connection>,
    path: &str,
    send_req: &dyn Fn(&mut Box<dyn Connection>, &[u8]) -> io::Result<()>,
    recv: &dyn Fn() -> io::Result<Vec<u8>>,
    done: &dyn Fn(),
) -> io::Result<()> {
    use thru_fs::{ET_DIR, OP_LS, ST_OK};
    let mut req = vec![OP_LS];
    req.extend_from_slice(&(path.len() as u16).to_be_bytes());
    req.extend_from_slice(path.as_bytes());
    send_req(conn, &req)?;
    let r = recv()?;
    done();
    if r.is_empty() || r[0] != ST_OK {
        return Err(io::Error::new(io::ErrorKind::Other, "ls failed"));
    }
    let mut p = &r[1..];
    while p.len() >= 3 {
        let is_dir = p[0] == ET_DIR;
        let nlen = u16::from_be_bytes([p[1], p[2]]) as usize;
        p = &p[3..];
        if p.len() < nlen { break; }
        let name = String::from_utf8_lossy(&p[..nlen]).to_string();
        p = &p[nlen..];
        println!("{}{}", name, if is_dir { "/" } else { "" });
    }
    Ok(())
}

/// get command inside a session.
fn session_get(
    conn: &mut Box<dyn Connection>,
    args: &[&str],
    send_req: &dyn Fn(&mut Box<dyn Connection>, &[u8]) -> io::Result<()>,
    recv: &dyn Fn() -> io::Result<Vec<u8>>,
    done: &dyn Fn(),
) -> io::Result<()> {
    use thru_fs::{OP_GET, ST_NOT_FOUND, ST_OK};
    let remote = match args.first() {
        Some(p) => p.to_string(),
        None => { eprintln!("usage: get <remote> [local]"); return Ok(()); }
    };
    let local = args.get(1).map(|s| s.to_string()).unwrap_or_else(|| {
        std::path::Path::new(&remote).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "download".into())
    });
    let mut req = vec![OP_GET];
    req.extend_from_slice(&(remote.len() as u16).to_be_bytes());
    req.extend_from_slice(remote.as_bytes());
    send_req(conn, &req)?;
    let head = recv()?;
    if head.is_empty() {
        done();
        return Err(io::Error::new(io::ErrorKind::Other, "get failed"));
    }
    if head[0] == ST_NOT_FOUND {
        done();
        eprintln!("remote file not found: {remote}");
        return Ok(());
    }
    if head[0] != ST_OK || head.len() < 9 {
        done();
        return Err(io::Error::new(io::ErrorKind::Other, "get failed"));
    }
    let size = u64::from_be_bytes([head[1],head[2],head[3],head[4],head[5],head[6],head[7],head[8]]);
    let mut f = std::fs::File::create(&local)?;
    loop {
        let chunk = recv()?;
        if chunk.is_empty() { break; }
        f.write_all(&chunk)?;
    }
    done();
    println!("downloaded {local} ({size} bytes)");
    Ok(())
}

/// exec command inside a session.
fn session_exec(
    conn: &mut Box<dyn Connection>,
    command: &str,
    send_req: &dyn Fn(&mut Box<dyn Connection>, &[u8]) -> io::Result<()>,
    recv: &dyn Fn() -> io::Result<Vec<u8>>,
    done: &dyn Fn(),
) -> io::Result<()> {
    use thru_exec::OP_EXEC_SCRIPT;
    let mut frame = vec![OP_EXEC_SCRIPT];
    frame.extend_from_slice(&(command.len() as u32).to_be_bytes());
    frame.extend_from_slice(command.as_bytes());
    send_req(conn, &frame)?;
    let mut output = Vec::new();
    let mut code = 1;
    loop {
        let f = recv()?;
        if f.is_empty() { break; }
        if f.len() == 4 {
            code = i32::from_be_bytes([f[0], f[1], f[2], f[3]]);
        } else {
            output.extend_from_slice(&f);
        }
    }
    done();
    io::stdout().write_all(&output)?;
    if code != 0 {
        eprintln!("(exit code: {code})");
    }
    Ok(())
}

/// Interactive remote PTY shell inside a session.
fn session_shell(
    conn: &mut Box<dyn Connection>,
    waiting: &Arc<AtomicBool>,
    rq: &Arc<RespQueue>,
) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

    let (cols, rows) = size().unwrap_or((80, 24));
    let mut open = vec![thru_exec::OP_SHELL_OPEN];
    open.extend_from_slice(&cols.to_be_bytes());
    open.extend_from_slice(&rows.to_be_bytes());
    waiting.store(true, Ordering::SeqCst);
    write_frame(conn, &open)?;

    enable_raw_mode()?;

    // Reader thread: response queue -> stdout.
    let rq2 = rq.clone();
    let reader_handle = thread::spawn(move || {
        let mut stdout = io::stdout();
        while let Ok(f) = rq2.pop() {
            if stdout.write_all(&f).is_err() { break; }
            let _ = stdout.flush();
        }
    });

    // Main thread: local keyboard -> connection frames.
    let result = (|| -> io::Result<()> {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    let bytes = key_to_bytes(key);
                    if !bytes.is_empty() {
                        write_frame(conn, &bytes)?;
                    }
                }
                Ok(Event::Resize(cols, rows)) => {
                    let mut frame = vec![thru_exec::OP_RESIZE];
                    frame.extend_from_slice(&cols.to_be_bytes());
                    frame.extend_from_slice(&rows.to_be_bytes());
                    write_frame(conn, &frame)?;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(())
    })();

    let _ = write_frame(conn, &[]);
    waiting.store(false, Ordering::SeqCst);
    let _ = reader_handle.join();
    let _ = disable_raw_mode();
    result
}

/// Convert a crossterm KeyEvent to raw terminal bytes.
fn key_to_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = c as u8;
                if (b'a'..=b'z').contains(&b) {
                    out.push(b - b'a' + 1);
                } else if (b'A'..=b'Z').contains(&b) {
                    out.push(b - b'A' + 1);
                } else {
                    out.extend_from_slice(c.to_string().as_bytes());
                }
            } else {
                out.extend_from_slice(c.to_string().as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\n'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(&[0x1b, b'[', b'D']),
        KeyCode::Right => out.extend_from_slice(&[0x1b, b'[', b'C']),
        KeyCode::Up => out.extend_from_slice(&[0x1b, b'[', b'A']),
        KeyCode::Down => out.extend_from_slice(&[0x1b, b'[', b'B']),
        KeyCode::Home => out.extend_from_slice(&[0x1b, b'[', b'H']),
        KeyCode::End => out.extend_from_slice(&[0x1b, b'[', b'F']),
        KeyCode::PageUp => out.extend_from_slice(&[0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => out.extend_from_slice(&[0x1b, b'[', b'6', b'~']),
        KeyCode::Delete => out.extend_from_slice(&[0x1b, b'[', b'3', b'~']),
        _ => {}
    }
    out
}

// --- dict subcommand ---

fn dict_cmd(args: &[String]) -> io::Result<()> {
    // Dict client always dials over TCP for now.
    let t: &dyn Transport = &Tcp;
    // Resolve server address: --connect flag > THRU_SERVER env > default.
    let mut addr = std::env::var("THRU_SERVER").unwrap_or_else(|_| format!("127.0.0.1:{PORT}"));
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--connect" && i + 1 < args.len() {
            addr = args[i + 1].clone();
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }

    // thru dict list [-v|-kv]
    if rest.first().map(|s| s.as_str()) == Some("list") {
        let kv = rest.iter().any(|s| s == "-kv");
        let vonly = rest.iter().any(|s| s == "-v");
        if kv || vonly {
            let all = thru_dict::client_all(t, &addr)?;
            for (k, v) in &all {
                if kv {
                    println!("{k}\t{}", escape(v));
                } else {
                    println!("{}", escape(v));
                }
            }
        } else {
            // Default: keys only, one per line.
            let keys = thru_dict::client_keys(t, &addr)?;
            for k in keys {
                println!("{k}");
            }
        }
        return Ok(());
    }

    // thru dict [-a] <key>
    let append = rest.iter().any(|s| s == "-a");
    let key = match rest.iter().find(|s| !s.starts_with('-')) {
        Some(k) => k.clone(),
        None => {
            eprintln!("usage: thru dict [-a] <key>  |  thru dict list [-v|-kv]");
            exit(2);
        }
    };

    if io::stdin().is_terminal() {
        // No piped input -> read mode.
        match thru_dict::client_get(t, &addr, &key)? {
            Some(v) => io::stdout().write_all(&v)?,
            None => exit(1),
        }
    } else {
        // Piped/redirected input -> write mode.
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        if append {
            thru_dict::client_append(t, &addr, &key, &buf)?;
        } else {
            thru_dict::client_set(t, &addr, &key, &buf)?;
        }
    }
    Ok(())
}

/// Escape newlines/tabs/backslashes for line-oriented output (list -v/-kv).
fn escape(v: &[u8]) -> String {
    String::from_utf8_lossy(v)
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

// --- fetch2 subcommand ---

/// `thru fetch2 <local_dir> [--connect addr]`
/// Non-interactive: remote path on stdin (first line) -> direct download.
/// Interactive (stdin is tty): directory browser UI (step 4).
fn fetch2_cmd(args: &[String]) -> io::Result<()> {
    let t: &dyn Transport = &Tcp;

    // Resolve --connect flag.
    let mut addr = std::env::var("THRU_SERVER").unwrap_or_else(|_| format!("127.0.0.1:{PORT}"));
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--connect" && i + 1 < args.len() {
            addr = args[i + 1].clone();
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }

    let local_dir = match rest.first() {
        Some(d) => d.clone(),
        None => {
            eprintln!("usage: thru fetch2 <local_dir> [--connect addr]");
            exit(2);
        }
    };

    // Ensure the local directory exists and is writable.
    ensure_dir(&local_dir)?;

    if io::stdin().is_terminal() {
        return interactive_browser(t, &addr, &local_dir);
    }

    // Non-interactive: read the first line of stdin as the remote path.
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let remote = line.trim();
    if remote.is_empty() {
        eprintln!("error: no remote path on stdin");
        exit(1);
    }

    download_file(t, &addr, remote, &local_dir)
}

/// Verify the directory exists (prompt to create if missing) and is writable.
fn ensure_dir(dir: &str) -> io::Result<()> {
    let p = Path::new(dir);
    if !p.exists() {
        if io::stdin().is_terminal() {
            eprint!("directory '{dir}' does not exist. create recursively? [y/N] ");
            io::stderr().flush()?;
            let mut ans = String::new();
            io::stdin().read_line(&mut ans)?;
            if ans.trim().to_lowercase() != "y" {
                eprintln!("aborted.");
                exit(1);
            }
            fs::create_dir_all(dir)?;
        } else {
            eprintln!("error: directory '{dir}' does not exist (run interactively to create)");
            exit(1);
        }
    }
    // Write-permission check via a temporary file.
    let test = p.join(".thru_write_test");
    match fs::File::create(&test) {
        Ok(mut f) => {
            f.write_all(b"x")?;
            drop(f);
            fs::remove_file(&test)?;
        }
        Err(e) => {
            eprintln!("error: no write permission in '{dir}': {e}");
            exit(1);
        }
    }
    Ok(())
}

/// Download a remote file into `local_dir`, using the remote file name.
fn download_file(
    t: &dyn Transport,
    addr: &str,
    remote: &str,
    local_dir: &str,
) -> io::Result<()> {
    let filename = Path::new(remote)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let local_path = Path::new(local_dir).join(&filename);

    let mut f = fs::File::create(&local_path)?;
    let size = thru_fs::client_get_progress(t, addr, remote, &mut f, |received, total| {
        let pct = if total > 0 { received * 100 / total } else { 0 };
        eprint!("\r{filename}: {received}/{total} bytes ({pct}%)   ");
        let _ = io::stderr().flush();
    })?;

    eprintln!("\r{filename}: done ({size} bytes)              ");
    Ok(())
}

// --- interactive file browser (crossterm) ---

use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyModifiers},
    queue,
    style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

/// Interactive remote directory browser.
/// ↑↓ select, →/Enter enter dir, ← back, Enter download file,
/// Enter twice on directory = confirm recursive download, q/Esc/Ctrl+C quit.
fn interactive_browser(t: &dyn Transport, addr: &str, local_dir: &str) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    queue!(stdout, EnterAlternateScreen)?;
    stdout.flush()?;

    let mut cur = String::from(".");
    let mut entries = load_entries(t, addr, &cur)?;
    let mut sel = 0;
    let mut pending: Option<usize> = None; // index of directory awaiting download confirmation

    loop {
        render(&mut stdout, &cur, &entries, sel, pending)?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Up => {
                    if sel > 0 { sel -= 1; }
                    pending = None;
                }
                KeyCode::Down => {
                    if sel + 1 < entries.len() { sel += 1; }
                    pending = None;
                }
                KeyCode::Left => {
                    let parent = parent_path(&cur);
                    if parent != cur {
                        cur = parent;
                        entries = load_entries(t, addr, &cur)?;
                        sel = 0;
                    }
                    pending = None;
                }
                KeyCode::Right => {
                    if let Some(e) = entries.get(sel) {
                        if e.is_dir {
                            cur = join_path(&cur, &e.name);
                            entries = load_entries(t, addr, &cur)?;
                            sel = 0;
                        }
                    }
                    pending = None;
                }
                KeyCode::Enter => {
                    if let Some(e) = entries.get(sel).cloned() {
                        if e.is_dir {
                            if pending == Some(sel) {
                                // Second Enter: confirm recursive directory download.
                                let remote = join_path(&cur, &e.name);
                                let local = Path::new(local_dir).join(&e.name);
                                leave_ui(&mut stdout)?;
                                let res = download_dir(t, addr, &remote, &local.to_string_lossy());
                                enter_ui(&mut stdout)?;
                                if let Err(err) = res {
                                    eprintln!("download dir error: {err}");
                                }
                                pending = None;
                            } else {
                                // First Enter: mark for confirmation.
                                pending = Some(sel);
                            }
                        } else {
                            // File: download immediately.
                            let remote = join_path(&cur, &e.name);
                            leave_ui(&mut stdout)?;
                            let res = download_file(t, addr, &remote, local_dir);
                            enter_ui(&mut stdout)?;
                            if let Err(err) = res {
                                eprintln!("download error: {err}");
                            }
                            pending = None;
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                _ => { pending = None; }
            }
        }
    }

    queue!(stdout, LeaveAlternateScreen)?;
    stdout.flush()?;
    disable_raw_mode()?;
    Ok(())
}

/// Load and sort directory entries (directories first, then alphabetical).
fn load_entries(t: &dyn Transport, addr: &str, path: &str) -> io::Result<Vec<thru_fs::FsEntry>> {
    let mut v = thru_fs::client_ls(t, addr, path)?;
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(v)
}

fn leave_ui(stdout: &mut io::Stdout) -> io::Result<()> {
    queue!(stdout, LeaveAlternateScreen)?;
    stdout.flush()?;
    disable_raw_mode()
}

fn enter_ui(stdout: &mut io::Stdout) -> io::Result<()> {
    enable_raw_mode()?;
    queue!(stdout, EnterAlternateScreen)?;
    stdout.flush()
}

/// Render the browser screen.
fn render(
    stdout: &mut io::Stdout,
    path: &str,
    entries: &[thru_fs::FsEntry],
    sel: usize,
    pending: Option<usize>,
) -> io::Result<()> {
    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    writeln!(stdout, " {path}")?;
    writeln!(stdout, "{}", "─".repeat(60))?;
    for (i, e) in entries.iter().enumerate() {
        let icon = if e.is_dir { "DIR " } else { "FILE" };
        let confirm = if pending == Some(i) { "  [ENTER again to download dir]" } else { "" };
        if i == sel {
            queue!(stdout, SetBackgroundColor(Color::DarkGrey), SetForegroundColor(Color::White))?;
        }
        writeln!(stdout, "  {} {}{}", icon, e.name, confirm)?;
        if i == sel {
            queue!(stdout, ResetColor)?;
        }
    }
    writeln!(stdout, "{}", "─".repeat(60))?;
    writeln!(stdout, " ↑↓ select  → enter  ← back  Enter download  q/esc quit")?;
    if let Some(_) = pending {
        writeln!(stdout, " Directory selected — press Enter again to confirm, any other key to cancel")?;
    }
    stdout.flush()
}

/// Recursively download a remote directory into `local_dir`.
fn download_dir(t: &dyn Transport, addr: &str, remote_dir: &str, local_dir: &str) -> io::Result<()> {
    fs::create_dir_all(local_dir)?;
    let entries = thru_fs::client_ls(t, addr, remote_dir)?;
    for e in entries {
        let remote = join_path(remote_dir, &e.name);
        let local = Path::new(local_dir).join(&e.name);
        if e.is_dir {
            download_dir(t, addr, &remote, &local.to_string_lossy())?;
        } else {
            let mut f = fs::File::create(&local)?;
            let name = e.name.clone();
            thru_fs::client_get_progress(t, addr, &remote, &mut f, |r, total| {
                eprint!("\r  {name}: {r}/{total}   ");
                let _ = io::stderr().flush();
            })?;
            eprintln!("\r  {name}: done              ");
        }
    }
    Ok(())
}

// --- path helpers (remote paths use '/') ---

fn join_path(base: &str, name: &str) -> String {
    if base == "." || base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    if path == "." || path == "/" || path.is_empty() {
        return ".".to_string();
    }
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => ".".to_string(),
    }
}
