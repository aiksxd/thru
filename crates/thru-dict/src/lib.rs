use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::time::Instant;
use thru_core::{read_frame, write_frame, BufExt};
use thru_transport::{connect, Connection};

// Operation codes
pub const OP_GET: u8 = 0;
pub const OP_SET: u8 = 1;
pub const OP_APPEND: u8 = 2;
pub const OP_DEL: u8 = 3;
pub const OP_KEYS: u8 = 4;
pub const OP_ALL: u8 = 5;

// Response status codes (re-exported from thru-core for convenience)
pub use thru_core::{ST_ERR, ST_NOT_FOUND, ST_OK};

/// Maximum size of a single dictionary value (10 MiB).
pub const MAX_VALUE_SIZE: usize = 10 * 1024 * 1024;
/// Maximum total size of all values stored in the dictionary (1 GiB).
pub const MAX_TOTAL_SIZE: usize = 1024 * 1024 * 1024;

// --- wire format helpers ---

/// Encode a request: [op][u16 key_len][key][value]
pub fn req(op: u8, key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + key.len() + value.len());
    v.push(op);
    v.push_u16(key.len() as u16);
    v.extend_from_slice(key);
    v.extend_from_slice(value);
    v
}

/// Encode a response: [status][payload]
pub fn resp(status: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + payload.len());
    v.push(status);
    v.extend_from_slice(payload);
    v
}

// --- server-side shared dictionary ---

/// A single dictionary entry with its last-access timestamp for LRU eviction.
struct Entry {
    value: Vec<u8>,
    last_access: Instant,
}

/// Internal dictionary state guarded by a single mutex.
struct DictInner {
    map: HashMap<String, Entry>,
    /// Sum of all value lengths in bytes (key overhead not tracked).
    total_bytes: usize,
}

pub struct Dict {
    inner: Mutex<DictInner>,
}

impl Dict {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DictInner {
                map: HashMap::new(),
                total_bytes: 0,
            }),
        }
    }

    /// Evict least-recently-used entries until `needed` additional bytes fit
    /// within MAX_TOTAL_SIZE. Returns true if enough space was freed.
    /// Caller must hold the inner lock.
    fn evict_until(inner: &mut DictInner, needed: usize) -> bool {
        if inner.total_bytes.saturating_add(needed) <= MAX_TOTAL_SIZE {
            return true;
        }
        // Sort keys by last_access (oldest first) — O(n log n) is acceptable
        // given the 1 GiB cap (at most ~1M entries at 1 KiB each).
        let mut keys: Vec<(String, Instant)> = inner
            .map
            .iter()
            .map(|(k, e)| (k.clone(), e.last_access))
            .collect();
        keys.sort_by_key(|(_, t)| *t);

        for (k, _) in keys {
            if inner.total_bytes.saturating_add(needed) <= MAX_TOTAL_SIZE {
                return true;
            }
            if let Some(entry) = inner.map.remove(&k) {
                inner.total_bytes = inner.total_bytes.saturating_sub(entry.value.len());
            }
        }
        inner.total_bytes.saturating_add(needed) <= MAX_TOTAL_SIZE
    }

    /// Handle one request frame and return a response frame.
    pub fn handle(&self, data: &[u8]) -> Vec<u8> {
        if data.len() < 3 {
            return resp(ST_ERR, b"truncated request");
        }
        let op = data[0];
        let klen = u16::from_be_bytes([data[1], data[2]]) as usize;
        if data.len() < 3 + klen {
            return resp(ST_ERR, b"truncated key");
        }
        let key = match std::str::from_utf8(&data[3..3 + klen]) {
            Ok(k) => k.to_string(),
            Err(_) => return resp(ST_ERR, b"invalid key encoding"),
        };
        let value = &data[3 + klen..];
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return resp(ST_ERR, b"dict internal lock poisoned"),
        };

        match op {
            OP_GET => match inner.map.get_mut(&key) {
                Some(e) => {
                    e.last_access = Instant::now();
                    resp(ST_OK, &e.value)
                }
                None => resp(ST_NOT_FOUND, &[]),
            },
            OP_SET => {
                if value.len() > MAX_VALUE_SIZE {
                    return resp(ST_ERR, b"value exceeds 10MB limit");
                }
                // Remove old value first so its space is not double-counted.
                if let Some(old) = inner.map.remove(&key) {
                    inner.total_bytes = inner.total_bytes.saturating_sub(old.value.len());
                }
                if !Self::evict_until(&mut inner, value.len()) {
                    return resp(ST_ERR, b"dict out of memory (1GB limit, LRU eviction failed)");
                }
                inner.total_bytes += value.len();
                inner.map.insert(
                    key,
                    Entry {
                        value: value.to_vec(),
                        last_access: Instant::now(),
                    },
                );
                resp(ST_OK, &[])
            }
            OP_APPEND => {
                let current_len = inner.map.get(&key).map(|e| e.value.len()).unwrap_or(0);
                let new_len = current_len.saturating_add(value.len());
                if new_len > MAX_VALUE_SIZE {
                    return resp(ST_ERR, b"value exceeds 10MB limit after append");
                }
                // Only the *additional* bytes need to be accommodated.
                if !Self::evict_until(&mut inner, value.len()) {
                    return resp(ST_ERR, b"dict out of memory (1GB limit, LRU eviction failed)");
                }
                let entry = inner.map.entry(key).or_insert_with(|| Entry {
                    value: Vec::new(),
                    last_access: Instant::now(),
                });
                entry.value.extend_from_slice(value);
                entry.last_access = Instant::now();
                inner.total_bytes = inner.total_bytes.saturating_add(value.len());
                resp(ST_OK, &[])
            }
            OP_DEL => {
                if let Some(old) = inner.map.remove(&key) {
                    inner.total_bytes = inner.total_bytes.saturating_sub(old.value.len());
                }
                resp(ST_OK, &[])
            }
            OP_KEYS => {
                let payload = inner.map.keys().cloned().collect::<Vec<_>>().join("\n");
                resp(ST_OK, payload.as_bytes())
            }
            OP_ALL => {
                // NOTE: OP_ALL returns all entries in a single frame. If the
                // total payload exceeds thru_core::MAX_FRAME (64 MiB), the
                // write will fail. Use KEYS + individual GET for large datasets.
                let mut p = Vec::new();
                for (k, e) in inner.map.iter() {
                    p.push_str(k);
                    p.push_u32(e.value.len() as u32);
                    p.extend_from_slice(&e.value);
                }
                resp(ST_OK, &p)
            }
            _ => resp(ST_ERR, b"unknown operation"),
        }
    }
}

// --- client-side helpers ---

/// Send a request on an existing connection and read the response.
pub fn call_on_conn(conn: &mut Box<dyn Connection>, request: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(conn, request)?;
    read_frame(conn)
}

fn call(t: &dyn thru_transport::Transport, addr: &str, request: &[u8]) -> io::Result<Vec<u8>> {
    let mut c = connect(t, addr)?;
    call_on_conn(&mut c, request)
}

fn check_ok(r: &[u8]) -> io::Result<()> {
    if r.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty response"));
    }
    if r[0] == ST_OK {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, String::from_utf8_lossy(&r[1..]).to_string()))
    }
}

/// Get a value on an existing connection; returns None if key missing.
pub fn get_on_conn(conn: &mut Box<dyn Connection>, key: &str) -> io::Result<Option<Vec<u8>>> {
    let r = call_on_conn(conn, &req(OP_GET, key.as_bytes(), &[]))?;
    if r.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty response"));
    }
    match r[0] {
        ST_OK => Ok(Some(r[1..].to_vec())),
        ST_NOT_FOUND => Ok(None),
        _ => Err(io::Error::new(io::ErrorKind::Other, String::from_utf8_lossy(&r[1..]).to_string())),
    }
}

pub fn set_on_conn(conn: &mut Box<dyn Connection>, key: &str, value: &[u8]) -> io::Result<()> {
    let r = call_on_conn(conn, &req(OP_SET, key.as_bytes(), value))?;
    check_ok(&r)
}

pub fn append_on_conn(conn: &mut Box<dyn Connection>, key: &str, value: &[u8]) -> io::Result<()> {
    let r = call_on_conn(conn, &req(OP_APPEND, key.as_bytes(), value))?;
    check_ok(&r)
}

pub fn keys_on_conn(conn: &mut Box<dyn Connection>) -> io::Result<Vec<String>> {
    let r = call_on_conn(conn, &req(OP_KEYS, &[], &[]))?;
    check_ok(&r)?;
    let payload = &r[1..];
    if payload.is_empty() {
        return Ok(vec![]);
    }
    Ok(String::from_utf8_lossy(payload).split('\n').map(|s| s.to_string()).collect())
}

/// Get a value; returns None if the key does not exist.
pub fn client_get(t: &dyn thru_transport::Transport, addr: &str, key: &str) -> io::Result<Option<Vec<u8>>> {
    let mut c = connect(t, addr)?;
    get_on_conn(&mut c, key)
}

pub fn client_set(t: &dyn thru_transport::Transport, addr: &str, key: &str, value: &[u8]) -> io::Result<()> {
    let mut c = connect(t, addr)?;
    set_on_conn(&mut c, key, value)
}

pub fn client_append(t: &dyn thru_transport::Transport, addr: &str, key: &str, value: &[u8]) -> io::Result<()> {
    let mut c = connect(t, addr)?;
    append_on_conn(&mut c, key, value)
}

pub fn client_keys(t: &dyn thru_transport::Transport, addr: &str) -> io::Result<Vec<String>> {
    let mut c = connect(t, addr)?;
    keys_on_conn(&mut c)
}

/// Get all key-value pairs.
pub fn client_all(t: &dyn thru_transport::Transport, addr: &str) -> io::Result<Vec<(String, Vec<u8>)>> {
    let r = call(t, addr, &req(OP_ALL, &[], &[]))?;
    check_ok(&r)?;
    let mut p = &r[1..];
    let mut out = Vec::new();
    while p.len() >= 2 {
        let klen = u16::from_be_bytes([p[0], p[1]]) as usize;
        p = &p[2..];
        if p.len() < klen { break; }
        let key = String::from_utf8_lossy(&p[..klen]).to_string();
        p = &p[klen..];
        if p.len() < 4 { break; }
        let vlen = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) as usize;
        p = &p[4..];
        if p.len() < vlen { break; }
        let value = p[..vlen].to_vec();
        p = &p[vlen..];
        out.push((key, value));
    }
    Ok(out)
}
