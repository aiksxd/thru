use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use thru_core::{read_frame, write_frame};
use thru_dict::Dict;
use thru_transport::{connect, Connection, Server, Transport};
use thru_proto_tcp::Tcp;

// CLI entry point.
//   server:  thru [port] [-p password]          (daemonizes, returns immediately)
//   connect: thru <host:port> [-p password]     (authenticate & cache session for later commands)
//   stop:    thru stop
//   shell:   thru shell [--connect addr]
//   exec:    thru exec <command> [--connect addr]
//   dict:    thru dict get/set/list ... [--connect addr]
//   fetch2:  thru fetch2 <local_dir> [--connect addr]
//
// Password is only used during initial 'thru <host:port>' connect.
// Subsequent commands read the cached session (address + password) automatically.

const PORT: u16 = 61696;
/// Read timeout for one-shot client operations (auth, dict, exec, fs metadata).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("dict") => return dict_cmd(&args[1..]),
        Some("fetch2") => return fetch2_cmd(&args[1..]),
        Some("shell") => return shell_cmd(&args[1..]),
        Some("exec") => return exec_cmd(&args[1..]),
        Some("stop") => return stop_server(),
        Some("pid") => return pid_cmd(&args[1..]),
        Some("help") | Some("--help") | Some("-h") => { print_help(); return Ok(()); }
        _ => {}
    }

    // Not a subcommand — must be either server (numeric port) or client connect (host:port).
    let target = args.iter().find(|a| !a.starts_with('-') && a.as_str() != "-p");
    if let Some(t) = target {
        if t.contains(':') {
            // Client connect: validate host:port format.
            if let Err(e) = validate_addr(t) {
                eprintln!("{e}");
                exit(1);
            }
            return connect_cmd(&args);
        }
        // Server: the positional argument must be a valid port number.
        if t.parse::<u16>().is_err() {
            eprintln!("invalid argument: '{t}' — expected a port number (server) or host:port (client connect)");
            exit(1);
        }
    }
    server_cmd(&args)
}

/// Validate that an address string is well-formed "host:port" with a numeric port.
fn validate_addr(addr: &str) -> io::Result<()> {
    // Split on the last colon to support IPv6 brackets like [::1]:61696.
    let parts: Vec<&str> = addr.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': expected host:port")));
    }
    let port_str = parts[0];
    let host = parts[1];
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': host is empty")));
    }
    if port_str.parse::<u16>().is_err() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': port '{port_str}' is not a valid number (1-65535)")));
    }
    Ok(())
}

fn print_help() {
    println!("thru — cross-platform device communication");
    println!();
    println!("USAGE:");
    println!("  thru [port] [-p password]           Start server (daemon, returns immediately)");
    println!("  thru <host:port> [-p password]      Connect to server and cache session (password only here)");
    println!("  thru stop                             Stop the running server");
    println!("  thru pid                              Show server PID, active connections and remote IPs");
    println!("  thru shell [--connect addr]          Interactive remote terminal");
    println!("  thru exec <command> [--connect addr] Execute command on remote side");
    println!("  thru dict <key> [value] [-a]         Read/write shared dictionary");
    println!("  thru dict list [-v|-kv]               List dictionary keys");
    println!("  thru fetch2 <local_dir>               Download files from remote side");
    println!("  thru help                              Show this help");
}

fn transport(p: &str) -> Box<dyn Transport> {
    match p {
        "tcp" => Box::new(Tcp),
        _ => panic!("unsupported protocol: -{p}"),
    }
}

// --- session cache (client connection state) ---

fn session_file() -> PathBuf {
    std::env::temp_dir().join("thru_client_session")
}

/// Load cached session: (address, password). Password is None if server has no password.
fn load_session() -> Option<(String, Option<String>)> {
    let content = fs::read_to_string(session_file()).ok()?;
    let mut lines = content.lines();
    let addr = lines.next()?.to_string();
    let pw = lines.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
    Some((addr, pw))
}

/// Save session after successful connect.
fn save_session(addr: &str, password: Option<&str>) -> io::Result<()> {
    let content = format!("{}\n{}\n", addr, password.unwrap_or(""));
    fs::write(session_file(), content)
}

// --- daemon / server ---

fn pid_file() -> PathBuf {
    std::env::temp_dir().join("thru_server.pid")
}

fn server_cmd(args: &[String]) -> io::Result<()> {
    // If already daemonized (child process), run the server loop directly.
    if std::env::var("THRU_DAEMON").is_ok() {
        return run_server(args);
    }

    // Check for existing server.
    let pf = pid_file();
    if pf.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pf) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                eprintln!("thru server already running (pid {pid}). use 'thru stop' first.");
                exit(1);
            }
        }
    }

    // Parse port and password for the status message.
    let port = args.iter().find(|a| !a.starts_with('-') && a.as_str() != "-p")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(PORT);

    // Spawn detached child process as daemon.
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env("THRU_DAEMON", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let child = cmd.spawn()?;
    let pid = child.id();
    fs::write(&pf, pid.to_string())?;
    println!("thru server started on port {port} (pid {pid})");
    std::process::exit(0);
}

fn run_server(args: &[String]) -> io::Result<()> {
    let mut password: Option<String> = None;
    let mut proto = "tcp".to_string();
    let mut port = PORT;
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
            port = a.parse().unwrap_or(PORT);
            i += 1;
        }
    }

    let t = transport(&proto);
    let t: &dyn Transport = &*t;
    let addr = format!("0.0.0.0:{port}");
    let mut s = Server::bind(t, &addr)?;
    let dict = Arc::new(Dict::new());
    // Track active connections: id -> remote peer address.
    let connections: Arc<Mutex<HashMap<u64, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let conn_id_counter = Arc::new(AtomicU64::new(0));

    loop {
        let mut conn = match s.accept() {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Authenticate before processing any requests.
        let pw = password.clone();
        let auth_ok = match auth_server(&mut conn, pw.as_deref()) {
            Ok(b) => b,
            Err(_) => false,
        };
        if !auth_ok {
            continue;
        }
        // Register this connection.
        let id = conn_id_counter.fetch_add(1, Ordering::SeqCst);
        let remote = conn.peer_addr().unwrap_or_else(|_| "unknown".to_string());
        connections.lock().unwrap().insert(id, remote);

        let dict = dict.clone();
        let connections = connections.clone();
        thread::spawn(move || {
            while let Ok(f) = read_frame(&mut conn) {
                if f.is_empty() { continue; }
                match f[0] {
                    0..=9 => { let r = dict.handle(&f); let _ = write_frame(&mut conn, &r); }
                    20..=29 => { let _ = thru_fs::handle(&mut conn, &f); }
                    30 => { let _ = thru_exec::handle_shell_open(&mut conn, &f); }
                    32 => { let _ = thru_exec::handle_exec_script(&mut conn, &f); }
                    50 => {
                        // Status query: respond with pid, connection count, remote IPs.
                        let pid = std::process::id();
                        let conns = connections.lock().unwrap();
                        let count = conns.len() as u32;
                        let mut resp = vec![0u8]; // ST_OK
                        resp.extend_from_slice(&pid.to_be_bytes());
                        resp.extend_from_slice(&count.to_be_bytes());
                        for peer in conns.values() {
                            resp.extend_from_slice(&(peer.len() as u16).to_be_bytes());
                            resp.extend_from_slice(peer.as_bytes());
                        }
                        drop(conns);
                        let _ = write_frame(&mut conn, &resp);
                    }
                    _ => {}
                }
            }
            // Unregister on disconnect.
            connections.lock().unwrap().remove(&id);
        });
    }
}

fn stop_server() -> io::Result<()> {
    let pf = pid_file();
    if !pf.exists() {
        eprintln!("no thru server running");
        exit(1);
    }
    let pid_str = fs::read_to_string(&pf)?;
    let pid = pid_str.trim().parse::<u32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pid file"))?;

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }

    let _ = fs::remove_file(&pf);
    println!("thru server stopped (pid {pid})");
    Ok(())
}

// --- authentication handshake ---

const AUTH_REQUIRED: u8 = 0x40;
const AUTH_NONE: u8 = 0x41;
const AUTH_OK: u8 = 0x42;
const AUTH_FAIL: u8 = 0x43;
const AUTH_PASS: u8 = 0x44;

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

/// Client-side authentication.
/// Returns (success, password_used). password_used is returned so the caller can cache it.
/// If interactive is true and no password is provided but server requires one, prompt on stdin.
/// If interactive is false and no password is available, fail with a message to connect first.
fn auth_client(conn: &mut Box<dyn Connection>, password: Option<&str>, interactive: bool) -> io::Result<(bool, Option<String>)> {
    let f = read_frame(conn)?;
    if f.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, "connection closed during auth"));
    }
    match f[0] {
        AUTH_NONE => Ok((true, None)),
        AUTH_REQUIRED => {
            let pw = match password {
                Some(p) => p.to_string(),
                None => {
                    if interactive {
                        match read_password() {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("{e}");
                                return Ok((false, None));
                            }
                        }
                    } else {
                        eprintln!("server requires password; run 'thru <host:port>' to connect first");
                        return Ok((false, None));
                    }
                }
            };
            let mut frame = vec![AUTH_PASS];
            frame.extend_from_slice(&(pw.len() as u16).to_be_bytes());
            frame.extend_from_slice(pw.as_bytes());
            write_frame(conn, &frame)?;
            let resp = read_frame(conn)?;
            match resp.first() {
                Some(&AUTH_OK) => Ok((true, Some(pw))),
                Some(&AUTH_FAIL) => {
                    eprintln!("authentication failed");
                    Ok((false, None))
                }
                _ => Err(io::Error::new(io::ErrorKind::Other, "invalid auth response")),
            }
        }
        _ => Err(io::Error::new(io::ErrorKind::Other, "invalid auth handshake")),
    }
}

fn read_password() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Other, "stdin is not a terminal; use -p <password>"));
    }
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

// --- client connect: establish and cache session ---

fn connect_cmd(args: &[String]) -> io::Result<()> {
    let (password, rest) = parse_password(args);
    let addr = match rest.iter().find(|a| a.contains(':')) {
        Some(a) => a.clone(),
        None => {
            eprintln!("usage: thru <host:port> [-p password]");
            exit(1);
        }
    };
    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    // Interactive: only the initial connect command may prompt for password.
    let (ok, pw_used) = auth_client(&mut conn, password.as_deref(), true)?;
    if !ok {
        exit(1);
    }
    save_session(&addr, pw_used.as_deref())?;
    // Query and display server PID + active connection count.
    match query_server_status(&mut conn) {
        Ok(status) => println!(
            "connected to {addr} (server pid: {}, {} active connection{})",
            status.pid,
            status.connections.len(),
            if status.connections.len() == 1 { "" } else { "s" }
        ),
        Err(_) => println!("connected to {addr}"),
    }
    Ok(())
}

/// Parse -p password from args, return (password, remaining args).
fn parse_password(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut pw = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-p" && i + 1 < args.len() {
            pw = Some(args[i + 1].clone());
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    (pw, rest)
}

/// Resolve server address: --connect flag > cached session > THRU_SERVER env > default.
fn resolve_addr(args: &[String]) -> (String, Vec<String>) {
    let mut addr: Option<String> = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--connect" && i + 1 < args.len() {
            addr = Some(args[i + 1].clone());
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    // Cached session from a prior 'thru <host:port>'.
    if addr.is_none() {
        if let Some((saddr, _)) = load_session() {
            addr = Some(saddr);
        }
    }
    // Environment fallback.
    if addr.is_none() {
        addr = Some(std::env::var("THRU_SERVER").unwrap_or_else(|_| format!("127.0.0.1:{PORT}")));
    }
    (addr.unwrap(), rest)
}

/// Get cached password from session (for subcommands after initial connect).
fn session_password() -> Option<String> {
    load_session().and_then(|(_, pw)| pw)
}

// --- server status query (opcode 50) ---

/// Parsed server status response.
struct ServerStatus {
    pid: u32,
    connections: Vec<String>,
}

/// Send a status query (opcode 50) on an existing connection and parse the response.
fn query_server_status(conn: &mut Box<dyn Connection>) -> io::Result<ServerStatus> {
    write_frame(conn, &[50u8])?;
    let resp = read_frame(conn)?;
    if resp.len() < 9 || resp[0] != 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "invalid status response"));
    }
    let pid = u32::from_be_bytes([resp[1], resp[2], resp[3], resp[4]]);
    let count = u32::from_be_bytes([resp[5], resp[6], resp[7], resp[8]]) as usize;
    let mut p = &resp[9..];
    let mut connections = Vec::new();
    for _ in 0..count {
        if p.len() < 2 { break; }
        let alen = u16::from_be_bytes([p[0], p[1]]) as usize;
        p = &p[2..];
        if p.len() < alen { break; }
        connections.push(String::from_utf8_lossy(&p[..alen]).to_string());
        p = &p[alen..];
    }
    Ok(ServerStatus { pid, connections })
}

/// `thru pid` — show server PID, active connection count, and remote IPs.
fn pid_cmd(args: &[String]) -> io::Result<()> {
    let pf = pid_file();
    if !pf.exists() {
        eprintln!("no thru server running");
        exit(1);
    }
    let local_pid = fs::read_to_string(&pf)?.trim().to_string();

    let (addr, _) = resolve_addr(args);
    let pw = session_password();
    let t: &dyn Transport = &Tcp;
    let mut conn = match connect(t, &addr) {
        Ok(c) => c,
        Err(e) => {
            // Still report the local PID even if we can't query the server.
            println!("server pid: {local_pid}");
            eprintln!("could not connect to {addr}: {e}");
            exit(1);
        }
    };
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        println!("server pid: {local_pid}");
        exit(1);
    }
    match query_server_status(&mut conn) {
        Ok(status) => {
            println!("server pid: {}", status.pid);
            println!("active connections: {}", status.connections.len());
            for (i, peer) in status.connections.iter().enumerate() {
                println!("  {}: {}", i + 1, peer);
            }
        }
        Err(e) => {
            println!("server pid: {local_pid}");
            eprintln!("could not query server status: {e}");
            exit(1);
        }
    }
    Ok(())
}

// --- shell: interactive remote terminal ---

fn shell_cmd(args: &[String]) -> io::Result<()> {
    let (addr, _) = resolve_addr(args);
    let pw = session_password();
    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    // Non-interactive: password must come from cached session.
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        exit(1);
    }
    // Clear timeout for long-running interactive session.
    conn.set_read_timeout(None)?;
    remote_shell_interactive(&mut conn)
}

fn remote_shell_interactive(conn: &mut Box<dyn Connection>) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

    let (cols, rows) = size().unwrap_or((80, 24));
    let mut open = vec![thru_exec::OP_SHELL_OPEN];
    open.extend_from_slice(&cols.to_be_bytes());
    open.extend_from_slice(&rows.to_be_bytes());
    write_frame(conn, &open)?;

    enable_raw_mode()?;

    let mut read_conn = conn.try_clone()?;
    let reader_handle = thread::spawn(move || {
        let mut stdout = io::stdout();
        while let Ok(f) = read_frame(&mut read_conn) {
            if stdout.write_all(&f).is_err() { break; }
            let _ = stdout.flush();
        }
    });

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
    let _ = reader_handle.join();
    let _ = disable_raw_mode();
    result
}

fn key_to_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt { out.push(0x1b); }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = c as u8;
                if (b'a'..=b'z').contains(&b) { out.push(b - b'a' + 1); }
                else if (b'A'..=b'Z').contains(&b) { out.push(b - b'A' + 1); }
                else { out.extend_from_slice(c.to_string().as_bytes()); }
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

// --- exec: one-shot remote command ---

fn exec_cmd(args: &[String]) -> io::Result<()> {
    let (addr, rest) = resolve_addr(args);
    if rest.is_empty() {
        eprintln!("usage: thru exec <command> [--connect addr]");
        exit(1);
    }
    let command = rest.join(" ");
    let pw = session_password();
    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        exit(1);
    }
    let (output, code) = thru_exec::exec_script_on_conn(&mut conn, &command)?;
    io::stdout().write_all(output.as_bytes())?;
    if code != 0 {
        exit(code);
    }
    Ok(())
}

// --- dict subcommand ---

fn dict_cmd(args: &[String]) -> io::Result<()> {
    let (addr, rest) = resolve_addr(args);
    let pw = session_password();
    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        exit(1);
    }

    if rest.first().map(|s| s.as_str()) == Some("list") {
        let kv = rest.iter().any(|s| s == "-kv");
        let vonly = rest.iter().any(|s| s == "-v");
        let keys = thru_dict::keys_on_conn(&mut conn)?;
        if kv {
            for k in &keys {
                if let Some(v) = thru_dict::get_on_conn(&mut conn, k)? {
                    println!("{k}\t{}", String::from_utf8_lossy(&v));
                }
            }
        } else if vonly {
            for k in &keys {
                if let Some(v) = thru_dict::get_on_conn(&mut conn, k)? {
                    io::stdout().write_all(&v)?;
                    io::stdout().write_all(b"\n")?;
                }
            }
        } else {
            for k in &keys { println!("{k}"); }
        }
        return Ok(());
    }

    let append = rest.iter().any(|s| s == "-a");
    // Support "get" and "set" subcommands for explicit read/write.
    let is_get = rest.first().map(|s| s.as_str()) == Some("get");
    let is_set = rest.first().map(|s| s.as_str()) == Some("set");
    let args: &[String] = if is_get || is_set { &rest[1..] } else { &rest };
    let key = match args.iter().find(|s| !s.starts_with('-')) {
        Some(k) => k.clone(),
        None => { eprintln!("usage: thru dict get <key> | thru dict set <key> <value> | thru dict [-a] <key> [value] | thru dict list [-v|-kv]"); exit(2); }
    };
    let key_pos = args.iter().position(|s| s == &key).unwrap();
    let has_value = key_pos + 1 < args.len();

    // Explicit get: read the key regardless of stdin.
    if is_get {
        match thru_dict::get_on_conn(&mut conn, &key)? {
            Some(v) => { io::stdout().write_all(&v)?; io::stdout().write_all(b"\n")?; }
            None => { eprintln!("key not found: {key}"); exit(1); }
        }
        return Ok(());
    }

    // Value on command line: write directly.
    if has_value {
        let value = args[key_pos + 1..].join(" ");
        if append {
            thru_dict::append_on_conn(&mut conn, &key, value.as_bytes())?;
        } else {
            thru_dict::set_on_conn(&mut conn, &key, value.as_bytes())?;
        }
        eprintln!("OK: {key}");
        return Ok(());
    }

    // No value arg: if stdin is piped, read value from stdin.
    if !io::stdin().is_terminal() {
        let mut value = Vec::new();
        io::stdin().read_to_end(&mut value)?;
        if value.last() == Some(&b'\n') { value.pop(); }
        if append {
            thru_dict::append_on_conn(&mut conn, &key, &value)?;
        } else {
            thru_dict::set_on_conn(&mut conn, &key, &value)?;
        }
        eprintln!("OK: {key}");
        return Ok(());
    }

    // Terminal mode, no value: if explicit set, error; else read key.
    if is_set {
        eprintln!("usage: thru dict set <key> <value>");
        exit(2);
    }
    match thru_dict::get_on_conn(&mut conn, &key)? {
        Some(v) => { io::stdout().write_all(&v)?; io::stdout().write_all(b"\n")?; }
        None => { eprintln!("key not found: {key}"); exit(1); }
    }
    Ok(())
}

// --- fetch2 subcommand ---

fn fetch2_cmd(args: &[String]) -> io::Result<()> {
    let (addr, rest) = resolve_addr(args);
    let local_dir = match rest.first() {
        Some(d) => d.clone(),
        None => { eprintln!("usage: thru fetch2 <local_dir> [--connect addr]"); exit(2); }
    };

    // Ensure local directory exists and is writable.
    if !std::path::Path::new(&local_dir).exists() {
        if io::stdin().is_terminal() {
            eprint!("directory '{local_dir}' does not exist. create? [y/N] ");
            io::stderr().flush()?;
            let mut ans = String::new();
            io::stdin().read_line(&mut ans)?;
            if !ans.trim().eq_ignore_ascii_case("y") {
                eprintln!("aborted");
                exit(1);
            }
        }
        fs::create_dir_all(&local_dir)?;
    }
    let test_file = std::path::Path::new(&local_dir).join(".thru_write_test");
    if fs::write(&test_file, b"x").is_err() {
        eprintln!("no write permission for '{local_dir}'");
        exit(1);
    }
    let _ = fs::remove_file(&test_file);

    let pw = session_password();
    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        exit(1);
    }

    // Non-interactive: first line of stdin is the remote path -> direct download.
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let remote = line.trim();
        if remote.is_empty() {
            eprintln!("no remote path on stdin");
            exit(1);
        }
        let fname = std::path::Path::new(remote).file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".into());
        let local_path = std::path::Path::new(&local_dir).join(&fname);
        let mut f = fs::File::create(&local_path)?;
        let size = thru_fs::get_on_conn(&mut conn, remote, &mut f)?;
        eprintln!("downloaded {fname} ({size} bytes)");
        return Ok(());
    }

    // Interactive browser.
    fetch2_interactive(&mut conn, &local_dir)
}

fn fetch2_interactive(conn: &mut Box<dyn Connection>, local_dir: &str) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    // Clear read timeout for long-running interactive session.
    conn.set_read_timeout(None)?;

    let mut remote_path = String::from(".");
    let mut entries = thru_fs::ls_on_conn(conn, &remote_path)?;
    let mut sel = 0usize;
    // Pending download confirmation: Some((name, is_dir)) after first Enter.
    let mut confirm_download: Option<(String, bool)> = None;

    // Enter alternate screen buffer (vim-like) and hide cursor.
    // This prevents polluting the shell scrollback / command history.
    print!("\x1b[?1049h\x1b[?25l");
    io::stdout().flush()?;

    enable_raw_mode()?;
    let result = (|| -> io::Result<()> {
        loop {
            // Render.
            let mut out = String::new();
            out.push_str(&format!("remote: {remote_path}  (↑↓ select, → enter dir, ← back, Enter download, q/Esc quit)\n"));
            if let Some((name, is_dir)) = &confirm_download {
                let kind = if *is_dir { "directory (recursive)" } else { "file" };
                out.push_str(&format!("Download {kind} '{name}'?  Enter = confirm, any other key = cancel\n"));
            }
            for (i, e) in entries.iter().enumerate() {
                let mark = if i == sel { ">" } else { " " };
                let suffix = if e.is_dir { "/" } else { "" };
                out.push_str(&format!("{mark} {}{suffix}\n", e.name));
            }
            print!("\x1b[2J\x1b[H{out}");
            io::stdout().flush()?;

            match event::read()? {
                Event::Key(key) => {
                    // Only process Press and Repeat — ignore Release to avoid double-trigger.
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        continue;
                    }
                    if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc
                        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    }
                    // Confirmation mode: Enter confirms download, any other key cancels.
                    if let Some((name, is_dir)) = confirm_download.take() {
                        if key.code == KeyCode::Enter {
                            print!("\x1b[2J\x1b[Hdownloading {name} ...\n");
                            io::stdout().flush()?;
                            if is_dir {
                                download_dir_recursive(conn, &remote_path, &name, local_dir)?;
                            } else {
                                let remote = format!("{}/{}", remote_path.trim_end_matches('/'), name);
                                let local_path = std::path::Path::new(local_dir).join(&name);
                                let mut f = fs::File::create(&local_path)?;
                                let size = thru_fs::get_on_conn(conn, &remote, &mut f)?;
                                print!("downloaded {name} ({size} bytes)\n");
                            }
                            entries = thru_fs::ls_on_conn(conn, &remote_path)?;
                            sel = 0;
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => if sel > 0 { sel -= 1; },
                        KeyCode::Down => if sel + 1 < entries.len() { sel += 1; },
                        KeyCode::Left | KeyCode::Backspace => {
                            if remote_path != "." {
                                remote_path = std::path::Path::new(&remote_path).parent()
                                    .map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".into());
                                if remote_path.is_empty() { remote_path = ".".into(); }
                                entries = thru_fs::ls_on_conn(conn, &remote_path)?;
                                sel = 0;
                            }
                        }
                        KeyCode::Right => {
                            // Right arrow enters directories only.
                            if entries.is_empty() { continue; }
                            let e = &entries[sel];
                            if e.is_dir {
                                remote_path = format!("{}/{}", remote_path.trim_end_matches('/'), e.name);
                                entries = thru_fs::ls_on_conn(conn, &remote_path)?;
                                sel = 0;
                            }
                        }
                        KeyCode::Enter => {
                            // Enter asks to download the selected entry (file or directory).
                            // Enter never enters directories — use → for that.
                            if entries.is_empty() { continue; }
                            let e = &entries[sel];
                            confirm_download = Some((e.name.clone(), e.is_dir));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = disable_raw_mode();
    // Leave alternate screen buffer and restore cursor.
    print!("\x1b[?1049l\x1b[?25h");
    let _ = io::stdout().flush();
    result
}

fn download_dir_recursive(conn: &mut Box<dyn Connection>, base: &str, dir_name: &str, local_root: &str) -> io::Result<()> {
    let remote_dir = format!("{}/{}", base.trim_end_matches('/'), dir_name);
    let local_dir = std::path::Path::new(local_root).join(dir_name);
    fs::create_dir_all(&local_dir)?;
    let entries = thru_fs::ls_on_conn(conn, &remote_dir)?;
    for e in entries {
        let remote = format!("{}/{}", remote_dir, e.name);
        if e.is_dir {
            download_dir_recursive(conn, &remote_dir, &e.name, local_dir.to_str().unwrap())?;
        } else {
            let local_path = local_dir.join(&e.name);
            let mut f = fs::File::create(&local_path)?;
            thru_fs::get_on_conn(conn, &remote, &mut f)?;
        }
    }
    Ok(())
}
