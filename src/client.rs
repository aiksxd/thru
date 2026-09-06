use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::process::exit;
use std::time::SystemTime;
use thru_core::{read_frame, write_frame, BufExt};
use thru_dict;
use thru_exec;
use thru_fs;
use thru_transport::Connection;

use crate::auth::{open_session, AlternateScreen, RawModeGuard};
use crate::reverse::{OP_DEVICE_LIST, OP_SET_TARGET};
use crate::server::PORT;

// --- shell: interactive remote terminal ---

pub(crate) fn shell_cmd(args: &[String]) -> io::Result<()> {
    let (mut conn, _, _) = open_session(args, PORT)?;
    conn.set_read_timeout(None)?;
    remote_shell_interactive(&mut conn)
}

fn remote_shell_interactive(conn: &mut Box<dyn Connection>) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use crossterm::terminal::size;

    let (cols, rows) = size().unwrap_or((80, 24));
    let mut open = vec![thru_exec::OP_SHELL_OPEN];
    open.extend_from_slice(&cols.to_be_bytes());
    open.extend_from_slice(&rows.to_be_bytes());
    write_frame(conn, &open)?;

    let _raw = RawModeGuard::new()?;

    let mut read_conn = conn.try_clone()?;
    let reader_handle = std::thread::spawn(move || {
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
            if ctrl && c.is_ascii() {
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

pub(crate) fn exec_cmd(args: &[String]) -> io::Result<()> {
    let (mut conn, _, rest) = open_session(args, PORT)?;
    if rest.is_empty() {
        eprintln!("usage: thru exec <command> [--connect addr]");
        exit(1);
    }
    let command = rest.join(" ");
    let (output, code) = thru_exec::exec_script_on_conn(&mut conn, &command)?;
    io::stdout().write_all(output.as_bytes())?;
    if code != 0 {
        exit(code);
    }
    Ok(())
}

// --- dict subcommand ---

pub(crate) fn dict_cmd(args: &[String]) -> io::Result<()> {
    let (mut conn, _, rest) = open_session(args, PORT)?;

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
    let is_get = rest.first().map(|s| s.as_str()) == Some("get");
    let is_set = rest.first().map(|s| s.as_str()) == Some("set");
    let args: &[String] = if is_get || is_set { &rest[1..] } else { &rest };
    let key = match args.iter().find(|s| !s.starts_with('-')) {
        Some(k) => k.clone(),
        None => { eprintln!("usage: thru dict get <key> | thru dict set <key> <value> | thru dict [-a] <key> [value] | thru dict list [-v|-kv]"); exit(2); }
    };
    let key_pos = args.iter().position(|s| s == &key).unwrap();
    let has_value = key_pos + 1 < args.len();

    if is_get {
        match thru_dict::get_on_conn(&mut conn, &key)? {
            Some(v) => { io::stdout().write_all(&v)?; io::stdout().write_all(b"\n")?; }
            None => { eprintln!("key not found: {key}"); exit(1); }
        }
        return Ok(());
    }

    if has_value {
        // Filter out flag arguments (-a) from the value portion.
        let value = args[key_pos + 1..].iter()
            .filter(|s| s.as_str() != "-a")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if append {
            thru_dict::append_on_conn(&mut conn, &key, value.as_bytes())?;
        } else {
            thru_dict::set_on_conn(&mut conn, &key, value.as_bytes())?;
        }
        eprintln!("OK: {key}");
        return Ok(());
    }

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

pub(crate) fn fetch2_cmd(args: &[String]) -> io::Result<()> {
    let (_, rest) = crate::auth::resolve_addr(args, PORT);
    let local_dir = match rest.first() {
        Some(d) => d.clone(),
        None => { eprintln!("usage: thru fetch2 <local_dir> [--connect addr]"); exit(2); }
    };

    if !Path::new(&local_dir).exists() {
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
    // Verify write permission by attempting to open a unique temp file for writing.
    // Using create_new ensures we never overwrite an existing file, and the RAII guard
    // removes it even if the program panics.
    let probe = Path::new(&local_dir).join(format!(".thru_probe_{}", std::process::id()));
    match fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => { let _ = fs::remove_file(&probe); }
        Err(e) => {
            eprintln!("no write permission for '{local_dir}': {e}");
            exit(1);
        }
    }

    let (mut conn, _, _) = open_session(args, PORT)?;

    write_frame(&mut conn, &[OP_DEVICE_LIST])?;
    let dev_resp = read_frame(&mut conn)?;
    let devices = parse_device_list(&dev_resp);
    let (self_name, self_user) = self_identity();
    let is_self = |d: &DeviceInfo| d.name == self_name && d.user == self_user;
    let default = devices.iter().find(|d| d.active && d.kind == 1 && !is_self(d))
        .or_else(|| devices.iter().find(|d| d.active && !is_self(d)))
        .map(|d| (d.name.clone(), d.user.clone()))
        .unwrap_or_else(|| ("server".to_string(), String::new()));
    let mut tgt = vec![OP_SET_TARGET];
    tgt.push_str(&default.0);
    tgt.push_str(&default.1);
    write_frame(&mut conn, &tgt)?;
    let _ = read_frame(&mut conn)?;
    let target_label = if default.0 == "server" {
        "[server] local filesystem".to_string()
    } else {
        format!("[remote] {} ({}@{})", default.0, default.1,
            devices.iter().find(|d| d.name == default.0).map(|d| d.addr.as_str()).unwrap_or(""))
    };

    if !io::stdin().is_terminal() {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        // Strip UTF-8 BOM if present (PowerShell 5.1 pipe adds BOM).
        let remote = line.trim().strip_prefix('\u{feff}').unwrap_or(line.trim());
        if remote.is_empty() {
            eprintln!("no remote path on stdin");
            exit(1);
        }
        let fname = Path::new(remote).file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".into());
        let local_path = Path::new(&local_dir).join(&fname);
        let mut f = fs::File::create(&local_path)?;
        match thru_fs::get_on_conn(&mut conn, remote, &mut f) {
            Ok(size) => {
                eprintln!("downloaded {fname} ({size} bytes) from {target_label}");
            }
            Err(e) => {
                // Remove the empty file created before download attempt.
                drop(f);
                let _ = fs::remove_file(&local_path);
                return Err(e);
            }
        }
        return Ok(());
    }

    fetch2_interactive(&mut conn, &local_dir, &target_label)
}

fn fetch2_interactive(conn: &mut Box<dyn Connection>, local_dir: &str, target_label: &str) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    conn.set_read_timeout(None)?;

    let mut remote_path = String::from(".");
    let mut entries = thru_fs::ls_on_conn(conn, &remote_path)?;
    let mut sel = 0usize;
    let mut confirm_download: Option<(String, bool)> = None;

    let _alt = AlternateScreen::new()?;
    let _raw = RawModeGuard::new()?;

    let result = (|| -> io::Result<()> {
        loop {
            let mut out = String::new();
            out.push_str(&format!("target: {target_label}\n"));
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
            // In raw mode, \n does not return cursor to column 0 (ONLCR disabled).
            // Use \r\n so each line starts at the left margin.
            print!("\x1b[2J\x1b[H{}", out.replace('\n', "\r\n"));
            io::stdout().flush()?;

            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        continue;
                    }
                    if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc
                        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    }
                    if let Some((name, is_dir)) = confirm_download.take() {
                        if key.code == KeyCode::Enter {
                            print!("\x1b[2J\x1b[Hdownloading {name} ...\r\n");
                            io::stdout().flush()?;
                            if is_dir {
                                download_dir_recursive(conn, &remote_path, &name, Path::new(local_dir))?;
                            } else {
                                let remote = format!("{}/{}", remote_path.trim_end_matches('/'), name);
                                let local_path = Path::new(local_dir).join(&name);
                                let mut f = fs::File::create(&local_path)?;
                                let size = thru_fs::get_on_conn(conn, &remote, &mut f)?;
                                print!("downloaded {name} ({size} bytes)\r\n");
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
                                remote_path = Path::new(&remote_path).parent()
                                    .map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".into());
                                if remote_path.is_empty() { remote_path = ".".into(); }
                                entries = thru_fs::ls_on_conn(conn, &remote_path)?;
                                sel = 0;
                            }
                        }
                        KeyCode::Right => {
                            if entries.is_empty() { continue; }
                            let e = &entries[sel];
                            if e.is_dir {
                                remote_path = format!("{}/{}", remote_path.trim_end_matches('/'), e.name);
                                entries = thru_fs::ls_on_conn(conn, &remote_path)?;
                                sel = 0;
                            }
                        }
                        KeyCode::Enter => {
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
    result
}

fn download_dir_recursive(conn: &mut Box<dyn Connection>, base: &str, dir_name: &str, local_root: &Path) -> io::Result<()> {
    let remote_dir = format!("{}/{}", base.trim_end_matches('/'), dir_name);
    let local_dir = local_root.join(dir_name);
    fs::create_dir_all(&local_dir)?;
    let entries = thru_fs::ls_on_conn(conn, &remote_dir)?;
    for e in entries {
        let remote = format!("{}/{}", remote_dir, e.name);
        if e.is_dir {
            download_dir_recursive(conn, &remote_dir, &e.name, &local_dir)?;
        } else {
            let local_path = local_dir.join(&e.name);
            let mut f = fs::File::create(&local_path)?;
            thru_fs::get_on_conn(conn, &remote, &mut f)?;
        }
    }
    Ok(())
}

// --- device subcommand ---

pub(crate) struct DeviceInfo {
    pub(crate) kind: u8,
    pub(crate) name: String,
    pub(crate) user: String,
    pub(crate) addr: String,
    pub(crate) connected_at: u64,
    pub(crate) active: bool,
}

fn parse_device_list(data: &[u8]) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    if data.is_empty() || data[0] != thru_core::ST_OK {
        return out;
    }
    let mut p = &data[1..];
    while p.len() >= 1 {
        let kind = p[0];
        p = &p[1..];
        let read_str = |p: &mut &[u8]| -> Option<String> {
            if p.len() < 2 { return None; }
            let len = u16::from_be_bytes([p[0], p[1]]) as usize;
            *p = &p[2..];
            if p.len() < len { return None; }
            let s = String::from_utf8_lossy(&p[..len]).to_string();
            *p = &p[len..];
            Some(s)
        };
        let name = match read_str(&mut p) { Some(s) => s, None => break };
        let user = match read_str(&mut p) { Some(s) => s, None => break };
        let addr = match read_str(&mut p) { Some(s) => s, None => break };
        if p.len() < 8 { break; }
        let connected_at = u64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]);
        p = &p[8..];
        if p.is_empty() { break; }
        let active = p[0] != 0;
        p = &p[1..];
        out.push(DeviceInfo { kind, name, user, addr, connected_at, active });
    }
    out
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

fn self_identity() -> (String, String) {
    if crate::server::pid_file().exists() {
        ("server".to_string(), String::new())
    } else {
        let name = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "anonymous".to_string());
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        (name, user)
    }
}

fn select_device_interactive(
    devices: &[DeviceInfo],
    self_name: &str,
    self_user: &str,
) -> io::Result<Option<usize>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let mut sel = devices
        .iter()
        .position(|d| d.active && !(d.name == self_name && d.user == self_user))
        .unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let _alt = AlternateScreen::new()?;
    let _raw = RawModeGuard::new()?;

    let result = loop {
        let mut out = String::new();
        out.push_str("Available devices (↑↓ select, Enter confirm, q/Esc quit)\n");
        out.push_str(&format!(
            "{:<4} {:<26} {:<14} {:<22} {:<10} {}\n",
            "", "DEVICE", "USER", "ADDRESS", "STATUS", "UPTIME"
        ));
        for (i, d) in devices.iter().enumerate() {
            let mark = if i == sel { ">" } else { " " };
            let tag = if d.kind == 0 { "[server]" } else { "[remote]" };
            let is_self = d.name == self_name && d.user == self_user;
            let dev_col = if is_self {
                format!("{tag} {} (this device)", d.name)
            } else {
                format!("{tag} {}", d.name)
            };
            let status = if d.active { "active" } else { "offline" };
            let uptime = if d.active {
                format_duration(now.saturating_sub(d.connected_at))
            } else {
                format!("last seen {}", format_duration(now.saturating_sub(d.connected_at)))
            };
            if !d.active || is_self {
                out.push_str(&format!(
                    "\x1b[2m{mark:<4} {:<26} {:<14} {:<22} {:<10} {}\x1b[0m\n",
                    dev_col, d.user, d.addr, status, uptime
                ));
            } else {
                out.push_str(&format!(
                    "{mark:<4} {:<26} {:<14} {:<22} {:<10} {}\n",
                    dev_col, d.user, d.addr, status, uptime
                ));
            }
        }
        print!("\x1b[2J\x1b[H{}", out.replace('\n', "\r\n"));
        io::stdout().flush()?;

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break None,
                    KeyCode::Up => {
                        if sel > 0 { sel -= 1; }
                    }
                    KeyCode::Down => {
                        if sel + 1 < devices.len() { sel += 1; }
                    }
                    KeyCode::Enter => {
                        let d = &devices[sel];
                        if d.active && !(d.name == self_name && d.user == self_user) {
                            break Some(sel);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    };

    Ok(result)
}

pub(crate) fn device_cmd(args: &[String]) -> io::Result<()> {
    let (mut conn, _, rest) = open_session(args, PORT)?;
    let local_dir = rest.first().cloned().unwrap_or_else(|| ".".to_string());

    write_frame(&mut conn, &[OP_DEVICE_LIST])?;
    let resp = read_frame(&mut conn)?;
    let devices = parse_device_list(&resp);
    if devices.is_empty() {
        eprintln!("no devices available");
        exit(1);
    }

    let (self_name, self_user) = self_identity();
    let sel = match select_device_interactive(&devices, &self_name, &self_user)? {
        Some(s) => s,
        None => return Ok(()),
    };
    let device = &devices[sel];

    let mut target = vec![OP_SET_TARGET];
    target.push_str(&device.name);
    target.push_str(&device.user);
    write_frame(&mut conn, &target)?;
    let ack = read_frame(&mut conn)?;
    if ack.is_empty() || ack[0] != thru_core::ST_OK {
        let msg = String::from_utf8_lossy(&ack[1..]);
        eprintln!("failed to select device: {msg}");
        exit(1);
    }

    if !Path::new(&local_dir).exists() {
        fs::create_dir_all(&local_dir)?;
    }

    let tag = if device.kind == 0 { "server" } else { "remote" };
    let target_label = format!("[{tag}] {} ({}@{})", device.name, device.user, device.addr);
    eprintln!("selected device: {target_label}");

    fetch2_interactive(&mut conn, &local_dir, &target_label)
}
