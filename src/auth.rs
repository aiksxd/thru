use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::exit;
use thru_core::{read_frame, write_frame};
use thru_proto_tcp::Tcp;
use thru_transport::{connect, Connection};

/// Read timeout for one-shot client operations (auth, dict, exec, fs metadata).
pub(crate) const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// --- session cache (client connection state) ---

pub(crate) fn session_file() -> PathBuf {
    std::env::temp_dir().join("thru_client_session")
}

/// Load cached session: (address, password). Password is None if server has no password.
pub(crate) fn load_session() -> Option<(String, Option<String>)> {
    let content = fs::read_to_string(session_file()).ok()?;
    // Strip UTF-8 BOM if present (some editors/tools write BOM on Windows).
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let mut lines = content.lines();
    let addr = lines.next()?.to_string();
    let pw = lines.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
    Some((addr, pw))
}

/// Save session after successful connect.
/// The file is created with owner-only permissions (0600 on Unix) so that
/// other users on the same machine cannot read the cached password.
pub(crate) fn save_session(addr: &str, password: Option<&str>) -> io::Result<()> {
    let content = format!("{}\n{}\n", addr, password.unwrap_or(""));
    let path = session_file();
    fs::write(&path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = fs::set_permissions(&path, perms) {
            eprintln!("warning: could not set session file permissions: {e}");
        }
    }
    Ok(())
}

/// Get cached password from session (for subcommands after initial connect).
pub(crate) fn session_password() -> Option<String> {
    load_session().and_then(|(_, pw)| pw)
}

// --- argument parsing helpers ---

/// Parse -p password from args, return (password, remaining args).
pub(crate) fn parse_password(args: &[String]) -> (Option<String>, Vec<String>) {
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
pub(crate) fn resolve_addr(args: &[String], default_port: u16) -> (String, Vec<String>) {
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
    if addr.is_none() {
        if let Some((saddr, _)) = load_session() {
            addr = Some(saddr);
        }
    }
    if addr.is_none() {
        addr = Some(std::env::var("THRU_SERVER").unwrap_or_else(|_| format!("127.0.0.1:{default_port}")));
    }
    (addr.unwrap(), rest)
}

/// Open an authenticated connection to the server.
/// Resolves address (--connect > cached session > THRU_SERVER > default),
/// loads cached password, connects, sets READ_TIMEOUT, and authenticates
/// non-interactively. Returns (connection, address, remaining args).
/// Exits the process on authentication failure.
pub(crate) fn open_session(args: &[String], default_port: u16) -> io::Result<(Box<dyn Connection>, String, Vec<String>)> {
    let (addr, rest) = resolve_addr(args, default_port);
    let pw = session_password();
    let mut conn = connect(&Tcp, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        exit(1);
    }
    Ok((conn, addr, rest))
}

// --- authentication handshake ---

// Authentication handshake:
//   Server -> Client: AUTH_REQUIRED + 16-byte random nonce   (if password set)
//   Server -> Client: AUTH_NONE                                  (if no password)
//   Client -> Server: AUTH_PASS + 32-byte SHA256(password || nonce)
//   Server -> Client: AUTH_OK | AUTH_FAIL
// The password itself is never transmitted over the wire.
pub(crate) const AUTH_REQUIRED: u8 = 0x40;
pub(crate) const AUTH_NONE: u8 = 0x41;
pub(crate) const AUTH_OK: u8 = 0x42;
pub(crate) const AUTH_FAIL: u8 = 0x43;
pub(crate) const AUTH_PASS: u8 = 0x44;
const AUTH_NONCE_LEN: usize = 16;
const AUTH_HASH_LEN: usize = 32; // SHA-256 digest size

pub(crate) fn auth_server(conn: &mut Box<dyn Connection>, password: Option<&str>) -> io::Result<bool> {
    match password {
        Some(pw) => {
            let nonce = generate_nonce();
            let mut frame = Vec::with_capacity(1 + AUTH_NONCE_LEN);
            frame.push(AUTH_REQUIRED);
            frame.extend_from_slice(&nonce);
            write_frame(conn, &frame)?;

            let f = read_frame(conn)?;
            if f.len() < 1 + AUTH_HASH_LEN || f[0] != AUTH_PASS {
                write_frame(conn, &[AUTH_FAIL])?;
                return Ok(false);
            }
            let client_hash = &f[1..1 + AUTH_HASH_LEN];
            let expected_hash = compute_auth_hash(pw, &nonce);
            if constant_time_eq(client_hash, &expected_hash) {
                write_frame(conn, &[AUTH_OK])?;
                conn.flush()?;
                Ok(true)
            } else {
                write_frame(conn, &[AUTH_FAIL])?;
                conn.flush()?;
                Ok(false)
            }
        }
        None => {
            write_frame(conn, &[AUTH_NONE])?;
            // Explicit flush ensures the single-byte AUTH_NONE frame is pushed
            // to the kernel immediately. Without this, small frames may be
            // delayed by Nagle's algorithm or tunnel buffering (e.g. frp),
            // causing the client's read_exact to hit EOF with UnexpectedEof.
            conn.flush()?;
            Ok(true)
        }
    }
}

/// Client-side authentication.
/// Returns (success, password_used). password_used is returned so the caller can cache it.
pub(crate) fn auth_client(conn: &mut Box<dyn Connection>, password: Option<&str>, interactive: bool) -> io::Result<(bool, Option<String>)> {
    let f = read_frame(conn)?;
    if f.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, "connection closed during auth"));
    }
    match f[0] {
        AUTH_NONE => Ok((true, None)),
        AUTH_REQUIRED => {
            if f.len() < 1 + AUTH_NONCE_LEN {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated auth nonce"));
            }
            let nonce = &f[1..1 + AUTH_NONCE_LEN];
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
            let hash = compute_auth_hash(&pw, nonce);
            let mut frame = Vec::with_capacity(1 + AUTH_HASH_LEN);
            frame.push(AUTH_PASS);
            frame.extend_from_slice(&hash);
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

// --- authentication helpers ---

fn generate_nonce() -> [u8; AUTH_NONCE_LEN] {
    use rand::RngCore;
    let mut nonce = [0u8; AUTH_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

fn compute_auth_hash(password: &str, nonce: &[u8]) -> [u8; AUTH_HASH_LEN] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(nonce);
    let result = hasher.finalize();
    let mut hash = [0u8; AUTH_HASH_LEN];
    hash.copy_from_slice(&result);
    hash
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// --- TUI helpers (RAII guards for raw mode and alternate screen) ---

/// RAII guard that enables raw mode on construction and disables it on drop.
pub(crate) struct RawModeGuard;
impl RawModeGuard {
    pub(crate) fn new() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// RAII guard that enters the alternate screen (hides cursor) on construction
/// and restores the normal screen on drop.
pub(crate) struct AlternateScreen;
impl AlternateScreen {
    pub(crate) fn new() -> io::Result<Self> {
        print!("\x1b[?1049h\x1b[?25l");
        io::stdout().flush()?;
        Ok(Self)
    }
}
impl Drop for AlternateScreen {
    fn drop(&mut self) {
        print!("\x1b[?1049l\x1b[?25h");
        let _ = io::stdout().flush();
    }
}

fn read_password() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Other, "stdin is not a terminal; use -p <password>"));
    }
    use crossterm::event::{self, KeyCode, KeyEventKind};
    eprint!("password: ");
    io::stderr().flush()?;
    let _raw = RawModeGuard::new()?;
    let mut pw = String::new();
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
    eprintln!();
    Ok(pw)
}
