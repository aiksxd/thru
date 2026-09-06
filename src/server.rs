use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;
use thru_core::{read_frame, write_frame, BufExt};
use thru_dict::Dict;
use thru_transport::{Connection, Server, Transport};
use thru_proto_tcp::Tcp;
use crate::auth::{auth_server, open_session};
use crate::reverse::{
    dispatch_fs, handle_device_list, handle_reverse_list,
    handle_reverse_proxy_get, handle_reverse_proxy_ls, handle_set_target,
    parse_reverse_registration, spawn_heartbeat_monitor,
    DeviceList, DeviceRecord, OP_DEVICE_LIST, OP_REVERSE_LIST, OP_REVERSE_PROXY_GET,
    OP_REVERSE_PROXY_LS, OP_REVERSE_REGISTER, OP_SET_TARGET, query_server_status,
};

pub(crate) const PORT: u16 = 61696;

pub(crate) fn pid_file() -> PathBuf {
    std::env::temp_dir().join("thru_server.pid")
}

pub(crate) fn reverse_pid_file() -> PathBuf {
    std::env::temp_dir().join("thru_reverse.pid")
}

pub(crate) fn transport(p: &str) -> Box<dyn Transport> {
    match p {
        "tcp" => Box::new(Tcp),
        _ => panic!("unsupported protocol: -{p}"),
    }
}

/// Spawn a detached child process that re-runs the current executable with
/// the given args and an environment variable set.
pub(crate) fn spawn_daemon(args: &[String], env_key: &str, env_val: &str) -> io::Result<u32> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env(env_key, env_val)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Forcefully terminate a process by PID (cross-platform).
pub(crate) fn force_kill(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .is_ok()
    }
}

// --- daemon / server ---

pub(crate) fn server_cmd(args: &[String]) -> io::Result<()> {
    if std::env::var("THRU_DAEMON").is_ok() {
        return run_server(args);
    }

    let pf = pid_file();
    if pf.exists() {
        if let Ok(pid_str) = fs::read_to_string(&pf) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                eprintln!("thru server already running (pid {pid}). use 'thru stop' first.");
                exit(1);
            }
        }
    }

    let port = args.iter().find(|a| !a.starts_with('-') && a.as_str() != "-p")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(PORT);

    let pid = spawn_daemon(args, "THRU_DAEMON", "1")?;
    fs::write(&pf, pid.to_string())?;
    println!("thru server started on port {port} (pid {pid})");
    std::process::exit(0);
}

fn run_server(args: &[String]) -> io::Result<()> {
    let mut password: Option<String> = None;
    let mut port = PORT;
    let mut max_devices: usize = 32;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-p" && i + 1 < args.len() {
            password = Some(args[i + 1].clone());
            i += 2;
        } else if (a == "-m" || a == "--max-devices") && i + 1 < args.len() {
            max_devices = args[i + 1].parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput,
                    format!("invalid max-devices value: '{}'", args[i + 1]))
            })?;
            i += 2;
        } else if a.starts_with('-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                format!("unknown server option: '{a}' (expected -p, -m, or a port number)")));
        } else {
            port = a.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput,
                    format!("invalid port number: '{a}'"))
            })?;
            i += 1;
        }
    }

    let t = transport("tcp");
    let t: &dyn Transport = &*t;
    let addr = format!("0.0.0.0:{port}");
    let mut s = Server::bind(t, &addr)?;
    let dict = Arc::new(Dict::new());
    let server_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let server_started = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let server_addr = addr.clone();
    let connections: Arc<Mutex<HashMap<u64, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let conn_id_counter = Arc::new(AtomicU64::new(0));
    let reverse_devices: DeviceList = Arc::new(Mutex::new(Vec::new()));

    loop {
        let mut conn = match s.accept() {
            Ok(c) => c,
            Err(e) => {
                // Transient errors (EINTR, ECONNABORTED) are safe to retry immediately.
                // For anything else, sleep briefly to avoid CPU-spinning on a fatal listener error.
                let kind = e.kind();
                if kind != io::ErrorKind::Interrupted && kind != io::ErrorKind::ConnectionAborted {
                    eprintln!("accept error: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                continue;
            }
        };
        let pw = password.clone();
        let auth_ok = match auth_server(&mut conn, pw.as_deref()) {
            Ok(b) => b,
            Err(_) => false,
        };
        if !auth_ok {
            continue;
        }
        let first = match read_frame(&mut conn) {
            Ok(f) => f,
            Err(_) => continue,
        };

        // --- reverse client registration ---
        if first.first() == Some(&OP_REVERSE_REGISTER) {
            let (name, user) = match parse_reverse_registration(&first) {
                Some(v) => v,
                None => {
                    eprintln!("rejected malformed reverse registration from {}",
                        conn.peer_addr().unwrap_or_else(|_| "unknown".to_string()));
                    continue;
                }
            };
            let remote = conn.peer_addr().unwrap_or_else(|_| "unknown".to_string());
            let now = SystemTime::now();
            let mut devices = reverse_devices.lock().unwrap();
            let active_count = devices.iter().filter(|d| d.active).count();
            if max_devices > 0 && active_count >= max_devices {
                eprintln!("reverse client rejected (max {max_devices} devices): {name} ({user}@{remote})");
                let _ = write_frame(&mut conn, &crate::reverse::reverse_err(&format!("max devices reached ({max_devices})")));
                continue;
            }
            let conn_arc = Arc::new(Mutex::new(conn));
            if let Some(idx) = devices.iter().position(|d| d.name == name && d.user == user) {
                eprintln!("reverse client reconnected: {name} ({user}@{remote}) at position {idx}");
                devices[idx].addr = remote.clone();
                devices[idx].last_seen = now;
                devices[idx].active = true;
                devices[idx].conn = Some(conn_arc.clone());
            } else {
                let idx = devices.len();
                eprintln!("reverse client registered: {name} ({user}@{remote}) at position {idx}");
                devices.push(DeviceRecord {
                    name: name.clone(),
                    user: user.clone(),
                    addr: remote,
                    connected_at: now,
                    last_seen: now,
                    active: true,
                    conn: Some(conn_arc.clone()),
                });
            }
            drop(devices);
            spawn_heartbeat_monitor(conn_arc, name.clone(), user.clone(), reverse_devices.clone());
            continue;
        }

        // --- normal client ---
        let id = conn_id_counter.fetch_add(1, Ordering::SeqCst);
        let remote = conn.peer_addr().unwrap_or_else(|_| "unknown".to_string());
        connections.lock().unwrap().insert(id, remote);

        let dict = dict.clone();
        let connections = connections.clone();
        let reverse_devices = reverse_devices.clone();
        let server_user = server_user.clone();
        let server_addr = server_addr.clone();
        thread::spawn(move || {
            let mut target_device: Option<(String, String)> = None;
            let mut pending = Some(first);
            loop {
                let f = match pending.take() {
                    Some(f) => f,
                    None => match read_frame(&mut conn) {
                        Ok(f) => f,
                        Err(_) => break,
                    },
                };
                if f.is_empty() { continue; }
                if dispatch_basic(&mut conn, &f, &dict, &reverse_devices, &mut target_device) {
                    continue;
                }
                match f[0] {
                    thru_core::OP_STATUS => {
                        let pid = std::process::id();
                        let conns = connections.lock().unwrap();
                        let count = conns.len() as u32;
                        let mut resp = vec![thru_core::ST_OK];
                        resp.push_u32(pid);
                        resp.push_u32(count);
                        for peer in conns.values() {
                            resp.push_str(peer);
                        }
                        drop(conns);
                        log_err(write_frame(&mut conn, &resp), "status response write");
                    }
                    OP_REVERSE_LIST => handle_reverse_list(&mut conn, &reverse_devices),
                    OP_REVERSE_PROXY_LS => handle_reverse_proxy_ls(&mut conn, &f, &reverse_devices),
                    OP_REVERSE_PROXY_GET => handle_reverse_proxy_get(&mut conn, &f, &reverse_devices),
                    OP_DEVICE_LIST => handle_device_list(&mut conn, &reverse_devices, &server_user, &server_addr, server_started),
                    OP_SET_TARGET => handle_set_target(&mut conn, &f, &mut target_device, &reverse_devices),
                    _ => {}
                }
            }
            connections.lock().unwrap().remove(&id);
        });
    }
}

/// Log an I/O error with context, used where a write failure means the client
/// has disconnected and we should silently drop the connection rather than panic.
fn log_err<T>(r: io::Result<T>, ctx: &str) {
    if let Err(e) = r {
        eprintln!("{ctx}: {e}");
    }
}

/// Dispatch basic protocol opcodes (dict 0-9, fs 20-29, exec 30/32).
fn dispatch_basic(
    conn: &mut Box<dyn Connection>,
    f: &[u8],
    dict: &Dict,
    reverse_devices: &DeviceList,
    target_device: &mut Option<(String, String)>,
) -> bool {
    match f[0] {
        op if (thru_dict::OP_GET..=thru_dict::OP_ALL).contains(&op) => {
            let r = dict.handle(f);
            log_err(write_frame(conn, &r), "dict response write");
            true
        }
        op if (thru_fs::OP_LS..=thru_fs::OP_GET).contains(&op) => {
            dispatch_fs(conn, f, reverse_devices, target_device);
            true
        }
        thru_exec::OP_SHELL_OPEN => {
            log_err(thru_exec::handle_shell_open(conn, f), "shell open");
            true
        }
        thru_exec::OP_EXEC_SCRIPT => {
            log_err(thru_exec::handle_exec_script(conn, f), "exec script");
            true
        }
        _ => false,
    }
}

pub(crate) fn stop_server() -> io::Result<()> {
    let pf = pid_file();
    if pf.exists() {
        let pid_str = fs::read_to_string(&pf)?;
        let pid = pid_str.trim().parse::<u32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pid file"))?;
        if force_kill(pid) {
            println!("thru server stopped (pid {pid})");
        } else {
            eprintln!("warning: failed to kill server process (pid {pid}), it may have already exited");
        }
        let _ = fs::remove_file(&pf);
    } else {
        eprintln!("no thru server running");
    }

    let rpf = reverse_pid_file();
    if rpf.exists() {
        if let Ok(pid_str) = fs::read_to_string(&rpf) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if force_kill(pid) {
                    println!("reverse connection stopped (pid {pid})");
                } else {
                    eprintln!("warning: failed to kill reverse connection (pid {pid})");
                }
            }
        }
        let _ = fs::remove_file(&rpf);
    }
    Ok(())
}

/// `thru pid` — show server PID, active connection count, and remote IPs.
pub(crate) fn pid_cmd(args: &[String]) -> io::Result<()> {
    let pf = pid_file();
    if !pf.exists() {
        eprintln!("no thru server running");
        exit(1);
    }
    let local_pid = fs::read_to_string(&pf)?.trim().to_string();

    let (mut conn, _, _) = match open_session(args, PORT) {
        Ok(c) => c,
        Err(e) => {
            println!("server pid: {local_pid}");
            eprintln!("could not connect: {e}");
            exit(1);
        }
    };
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
    write_frame(&mut conn, &[OP_REVERSE_LIST])?;
    match read_frame(&mut conn) {
        Ok(resp) if !resp.is_empty() && resp[0] == thru_core::ST_OK => {
            let mut p = &resp[1..];
            let mut names = Vec::new();
            while p.len() >= 2 {
                let nlen = u16::from_be_bytes([p[0], p[1]]) as usize;
                p = &p[2..];
                if p.len() < nlen { break; }
                names.push(String::from_utf8_lossy(&p[..nlen]).to_string());
                p = &p[nlen..];
            }
            if !names.is_empty() {
                println!("reverse clients ({}):", names.len());
                for n in &names {
                    println!("  {n}");
                }
            }
        }
        _ => {}
    }
    Ok(())
}
