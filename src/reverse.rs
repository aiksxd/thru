use std::fs;
use std::io;
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use thru_core::{read_frame, write_frame, BufExt};
use thru_exec;
use thru_fs;
use thru_transport::{connect, Connection, Transport};
use thru_proto_tcp::Tcp;

use crate::auth::{auth_client, parse_password, resolve_addr, save_session, session_password, READ_TIMEOUT};
use crate::server::{reverse_pid_file, spawn_daemon, PORT};

// --- reverse-tunnel opcodes (60-69) ---
pub(crate) const OP_REVERSE_REGISTER: u8 = 60;
pub(crate) const OP_REVERSE_LIST: u8 = 61;
pub(crate) const OP_REVERSE_PROXY_LS: u8 = 62;
pub(crate) const OP_REVERSE_PROXY_GET: u8 = 63;
pub(crate) const OP_DEVICE_LIST: u8 = 64;
pub(crate) const OP_SET_TARGET: u8 = 65;
pub(crate) const OP_PING: u8 = 70;

// --- reverse-tunnel server-side helpers ---

/// A device record: persists across disconnects, keyed by (name, user).
pub(crate) struct DeviceRecord {
    pub(crate) name: String,
    pub(crate) user: String,
    pub(crate) addr: String,
    pub(crate) connected_at: SystemTime,
    pub(crate) last_seen: SystemTime,
    pub(crate) active: bool,
    pub(crate) conn: Option<Arc<Mutex<Box<dyn Connection>>>>,
}

/// Shared, ordered list of all known devices (including offline ones).
pub(crate) type DeviceList = Arc<Mutex<Vec<DeviceRecord>>>;

/// Parse two length-prefixed strings from a frame: [op][u16 len][s1][u16 len][s2].
fn parse_two_strings(data: &[u8]) -> Option<(String, String)> {
    if data.len() < 3 {
        return None;
    }
    let nlen = u16::from_be_bytes([data[1], data[2]]) as usize;
    if data.len() < 3 + nlen + 2 {
        return None;
    }
    let first = String::from_utf8_lossy(&data[3..3 + nlen]).to_string();
    let p = &data[3 + nlen..];
    let slen = u16::from_be_bytes([p[0], p[1]]) as usize;
    if p.len() < 2 + slen {
        return None;
    }
    let second = String::from_utf8_lossy(&p[2..2 + slen]).to_string();
    Some((first, second))
}

/// Parse (name, user) from an OP_REVERSE_REGISTER frame.
/// Returns None if the frame is malformed (caller should reject the connection).
pub(crate) fn parse_reverse_registration(data: &[u8]) -> Option<(String, String)> {
    parse_two_strings(data)
}

/// Parse (client_name, path) from an OP_REVERSE_PROXY_* frame.
fn parse_reverse_proxy_req(data: &[u8]) -> Option<(String, String)> {
    parse_two_strings(data)
}

/// Build an error response frame: [ST_ERR][message]
pub(crate) fn reverse_err(msg: &str) -> Vec<u8> {
    let mut v = vec![thru_core::ST_ERR];
    v.extend_from_slice(msg.as_bytes());
    v
}

/// Build a proxy request frame: [op][u16 name_len][name][u16 path_len][path]
fn make_proxy_frame(op: u8, name: &str, path: &str) -> Vec<u8> {
    let mut v = vec![op];
    v.push_str(name);
    v.push_str(path);
    v
}

/// Get the default target device: the earliest active reverse client, or "server" if none.
pub(crate) fn default_target(devices: &DeviceList) -> (String, String) {
    let devs = devices.lock().unwrap();
    match devs.iter().find(|d| d.active) {
        Some(d) => (d.name.clone(), d.user.clone()),
        None => ("server".to_string(), String::new()),
    }
}

/// Dispatch an fs request according to the connection's target device.
pub(crate) fn dispatch_fs(
    conn: &mut Box<dyn Connection>,
    f: &[u8],
    devices: &DeviceList,
    target_device: &mut Option<(String, String)>,
) {
    let target = target_device.get_or_insert_with(|| default_target(devices));
    let (name, user) = &target;

    if name == "server" {
        let _ = thru_fs::handle(conn, f);
        return;
    }

    let is_active = devices
        .lock()
        .unwrap()
        .iter()
        .any(|d| d.name == *name && d.user == *user && d.active);
    if !is_active {
        let _ = write_frame(
            conn,
            &reverse_err(&format!(
                "device '{name}' ({user}) is offline, use 'thru device' to select another"
            )),
        );
        return;
    }

    let path = thru_fs::parse_path(f).unwrap_or(".");
    match f[0] {
        thru_fs::OP_LS => {
            let proxy = make_proxy_frame(OP_REVERSE_PROXY_LS, name, path);
            handle_reverse_proxy_ls(conn, &proxy, devices);
        }
        thru_fs::OP_GET => {
            let proxy = make_proxy_frame(OP_REVERSE_PROXY_GET, name, path);
            handle_reverse_proxy_get(conn, &proxy, devices);
        }
        _ => {
            let _ = thru_fs::handle(conn, f);
        }
    }
}

/// OP_DEVICE_LIST: respond with all devices (server + all reverse clients).
pub(crate) fn handle_device_list(
    conn: &mut Box<dyn Connection>,
    devices: &DeviceList,
    server_user: &str,
    server_addr: &str,
    server_started: u64,
) {
    let mut resp = vec![thru_core::ST_OK];
    resp.push(0); // type = server
    resp.push_str("server");
    resp.push_str(server_user);
    resp.push_str(server_addr);
    resp.push_u64(server_started);
    resp.push(1); // active = true
    let devs = devices.lock().unwrap();
    for d in devs.iter() {
        resp.push(1); // type = reverse
        resp.push_str(&d.name);
        resp.push_str(&d.user);
        resp.push_str(&d.addr);
        let ts = d
            .connected_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        resp.push_u64(ts);
        resp.push(if d.active { 1 } else { 0 });
    }
    drop(devs);
    let _ = write_frame(conn, &resp);
}

/// OP_SET_TARGET: set the target device for subsequent fs requests.
pub(crate) fn handle_set_target(
    conn: &mut Box<dyn Connection>,
    data: &[u8],
    target_device: &mut Option<(String, String)>,
    devices: &DeviceList,
) {
    let Some((name, user)) = parse_two_strings(data) else {
        let _ = write_frame(conn, &reverse_err("invalid target request"));
        return;
    };
    let valid = (name == "server" && user.is_empty())
        || devices
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.name == name && d.user == user);
    if valid {
        *target_device = Some((name, user));
        let _ = write_frame(conn, &[thru_core::ST_OK]);
    } else {
        let _ = write_frame(conn, &reverse_err(&format!("device '{name}' ({user}) not found")));
    }
}

/// OP_REVERSE_LIST: list active reverse client names (legacy opcode).
pub(crate) fn handle_reverse_list(conn: &mut Box<dyn Connection>, devices: &DeviceList) {
    let devs = devices.lock().unwrap();
    let mut resp = vec![thru_core::ST_OK];
    for d in devs.iter().filter(|d| d.active) {
        resp.push_str(&d.name);
    }
    drop(devs);
    let _ = write_frame(conn, &resp);
}

/// Find an active device's connection by (name, user).
fn find_device_conn(devices: &DeviceList, name: &str, user: &str) -> Option<Arc<Mutex<Box<dyn Connection>>>> {
    let devs = devices.lock().unwrap();
    devs.iter()
        .find(|d| d.name == name && d.user == user && d.active)
        .and_then(|d| d.conn.clone())
}

/// Mark a device as inactive after a proxy failure.
pub(crate) fn mark_device_offline(devices: &DeviceList, name: &str, user: &str) {
    let mut devs = devices.lock().unwrap();
    if let Some(d) = devs.iter_mut().find(|d| d.name == name && d.user == user) {
        d.active = false;
        d.conn = None;
        d.last_seen = SystemTime::now();
    }
}

/// Spawn a silent heartbeat monitor for a reverse client connection.
pub(crate) fn spawn_heartbeat_monitor(
    conn: Arc<Mutex<Box<dyn Connection>>>,
    name: String,
    user: String,
    devices: DeviceList,
) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(15));
            let mut c = match conn.lock() {
                Ok(c) => c,
                Err(_) => break,
            };
            if write_frame(&mut *c, &[OP_PING]).is_err() {
                drop(c);
                let still_current = {
                    let devs = devices.lock().unwrap();
                    devs.iter()
                        .find(|d| d.name == name && d.user == user)
                        .and_then(|d| d.conn.as_ref())
                        .map(|current| Arc::ptr_eq(current, &conn))
                        .unwrap_or(false)
                };
                if still_current {
                    mark_device_offline(&devices, &name, &user);
                }
                break;
            }
        }
    });
}

/// Resolve a named reverse client for proxying.
fn resolve_proxy_target(
    conn: &mut Box<dyn Connection>,
    data: &[u8],
    devices: &DeviceList,
) -> Option<(Arc<Mutex<Box<dyn Connection>>>, String, String, String)> {
    let (name, path) = parse_reverse_proxy_req(data)?;
    let (dev_name, dev_user) = {
        let devs = devices.lock().unwrap();
        match devs.iter().find(|d| d.name == name && d.active) {
            Some(d) => (d.name.clone(), d.user.clone()),
            None => {
                let _ = write_frame(conn, &reverse_err(&format!("reverse client '{name}' not found")));
                return None;
            }
        }
    };
    let client_conn = find_device_conn(devices, &dev_name, &dev_user)?;
    Some((client_conn, dev_name, dev_user, path))
}

/// Send a request frame to a reverse client and handle a write failure.
fn send_proxy_request<'a>(
    conn: &mut Box<dyn Connection>,
    client_conn: &'a Arc<Mutex<Box<dyn Connection>>>,
    dev_name: &str,
    dev_user: &str,
    devices: &DeviceList,
    request: &[u8],
) -> Option<std::sync::MutexGuard<'a, Box<dyn Connection>>> {
    let mut rc = client_conn.lock().unwrap();
    if write_frame(&mut *rc, request).is_err() {
        drop(rc);
        mark_device_offline(devices, dev_name, dev_user);
        let _ = write_frame(conn, &reverse_err("reverse client disconnected"));
        return None;
    }
    Some(rc)
}

/// OP_REVERSE_PROXY_LS: forward an LS request to a named reverse client.
pub(crate) fn handle_reverse_proxy_ls(
    conn: &mut Box<dyn Connection>,
    data: &[u8],
    devices: &DeviceList,
) {
    let Some((client_conn, dev_name, dev_user, path)) = resolve_proxy_target(conn, data, devices) else {
        return;
    };
    let request = thru_fs::req(thru_fs::OP_LS, path.as_bytes());
    let Some(mut rc) = send_proxy_request(conn, &client_conn, &dev_name, &dev_user, devices, &request) else {
        return;
    };
    match read_frame(&mut *rc) {
        Ok(response) => {
            let _ = write_frame(conn, &response);
        }
        Err(_) => {
            drop(rc);
            mark_device_offline(devices, &dev_name, &dev_user);
            let _ = write_frame(conn, &reverse_err("reverse client disconnected"));
        }
    }
}

/// OP_REVERSE_PROXY_GET: forward a streaming GET to a named reverse client.
pub(crate) fn handle_reverse_proxy_get(
    conn: &mut Box<dyn Connection>,
    data: &[u8],
    devices: &DeviceList,
) {
    let Some((client_conn, dev_name, dev_user, path)) = resolve_proxy_target(conn, data, devices) else {
        return;
    };
    let request = thru_fs::req(thru_fs::OP_GET, path.as_bytes());
    let Some(mut rc) = send_proxy_request(conn, &client_conn, &dev_name, &dev_user, devices, &request) else {
        return;
    };
    let header = match read_frame(&mut *rc) {
        Ok(h) => h,
        Err(_) => {
            drop(rc);
            mark_device_offline(devices, &dev_name, &dev_user);
            let _ = write_frame(conn, &reverse_err("reverse client disconnected"));
            return;
        }
    };
    if write_frame(conn, &header).is_err() {
        return;
    }
    if header.first() != Some(&thru_core::ST_OK) {
        return;
    }
    loop {
        let chunk = match read_frame(&mut *rc) {
            Ok(c) => c,
            Err(_) => {
                drop(rc);
                mark_device_offline(devices, &dev_name, &dev_user);
                let _ = write_frame(conn, &[]);
                break;
            }
        };
        let is_empty = chunk.is_empty();
        if write_frame(conn, &chunk).is_err() {
            break;
        }
        if is_empty {
            break;
        }
    }
}

// --- client connect: establish and cache session ---

pub(crate) fn connect_cmd(args: &[String]) -> io::Result<()> {
    if std::env::var("THRU_REVERSE_DAEMON").is_ok() {
        return run_reverse_daemon(args);
    }

    let (password, rest) = parse_password(args);
    let addr = match rest.iter().find(|a| a.contains(':')) {
        Some(a) => a.clone(),
        None => {
            eprintln!("usage: thru <host:port> [-p password]");
            exit(1);
        }
    };

    let rpf = reverse_pid_file();
    if rpf.exists() {
        if let Ok(pid_str) = fs::read_to_string(&rpf) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                eprintln!("thru reverse connection already running (pid {pid}). use 'thru stop' first.");
                exit(1);
            }
        }
    }

    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(Some(READ_TIMEOUT))?;
    let (ok, pw_used) = auth_client(&mut conn, password.as_deref(), true)?;
    if !ok {
        exit(1);
    }
    save_session(&addr, pw_used.as_deref())?;
    let status_line = match query_server_status(&mut conn) {
        Ok(status) => format!(
            "connected to {addr} (server pid: {}, {} active connection{})",
            status.pid,
            status.connections.len(),
            if status.connections.len() == 1 { "" } else { "s" }
        ),
        Err(_) => format!("connected to {addr}"),
    };
    drop(conn);

    let pid = spawn_daemon(args, "THRU_REVERSE_DAEMON", "1")?;
    fs::write(&rpf, pid.to_string())?;
    println!("{status_line}");
    println!("reverse connection established (pid {pid}) — server can now pull files from this machine");
    std::process::exit(0);
}

/// Persistent reverse-connection daemon: register with the server and handle
/// incoming requests (fs / dict / exec) until disconnected.
fn run_reverse_daemon(args: &[String]) -> io::Result<()> {
    let (addr, _) = resolve_addr(args, PORT);
    let pw = session_password();
    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());

    let t: &dyn Transport = &Tcp;
    let mut conn = connect(t, &addr)?;
    conn.set_read_timeout(None)?;
    let (ok, _) = auth_client(&mut conn, pw.as_deref(), false)?;
    if !ok {
        let _ = fs::remove_file(reverse_pid_file());
        exit(1);
    }

    let mut reg = vec![OP_REVERSE_REGISTER];
    reg.push_str(&name);
    reg.push_str(&user);
    write_frame(&mut conn, &reg)?;

    loop {
        let f = match read_frame(&mut conn) {
            Ok(f) => f,
            Err(_) => break,
        };
        if f.is_empty() {
            continue;
        }
        match f[0] {
            op if (thru_fs::OP_LS..=thru_fs::OP_GET).contains(&op) => {
                let _ = thru_fs::handle(&mut conn, &f);
            }
            thru_exec::OP_SHELL_OPEN => {
                let _ = thru_exec::handle_shell_open(&mut conn, &f);
            }
            thru_exec::OP_EXEC_SCRIPT => {
                let _ = thru_exec::handle_exec_script(&mut conn, &f);
            }
            _ => {}
        }
    }
    let _ = fs::remove_file(reverse_pid_file());
    Ok(())
}

// --- server status query (opcode 50) ---

pub(crate) struct ServerStatus {
    pub(crate) pid: u32,
    pub(crate) connections: Vec<String>,
}

pub(crate) fn query_server_status(conn: &mut Box<dyn Connection>) -> io::Result<ServerStatus> {
    write_frame(conn, &[50u8])?;
    let resp = read_frame(conn)?;
    if resp.len() < 9 || resp[0] != thru_core::ST_OK {
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
