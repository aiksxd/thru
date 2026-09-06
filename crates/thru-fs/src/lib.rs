use std::fs;
use std::io::{self, Read, Write};
use thru_core::{read_frame, write_frame, BufExt};
use thru_transport::{connect, Connection, Transport};

// Operation codes (20-29 reserved for fs)
pub const OP_LS: u8 = 20;
pub const OP_GET: u8 = 21;

// Status codes (re-exported from thru-core)
pub use thru_core::{ST_ERR, ST_NOT_FOUND, ST_OK};

// Entry type tags in LS response payload
pub const ET_FILE: u8 = 0;
pub const ET_DIR: u8 = 1;

/// Max bytes per data frame in GET streaming.
pub const CHUNK: usize = 1024 * 1024; // 1 MiB

/// A single directory entry returned by LS.
#[derive(Clone)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
}

// --- wire format helpers ---

/// Encode a request: [op][u16 path_len][path]
pub fn req(op: u8, path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + path.len());
    v.push(op);
    v.push_u16(path.len() as u16);
    v.extend_from_slice(path);
    v
}

/// Decode the path from a request frame.
pub fn parse_path(data: &[u8]) -> io::Result<&str> {
    if data.len() < 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated request"));
    }
    let klen = u16::from_be_bytes([data[1], data[2]]) as usize;
    if data.len() < 3 + klen {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated path"));
    }
    std::str::from_utf8(&data[3..3 + klen])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid path encoding"))
}

fn resp_err(msg: &str) -> Vec<u8> {
    let mut v = vec![ST_ERR];
    v.extend_from_slice(msg.as_bytes());
    v
}

// --- server-side handler ---

/// Handle an fs request frame.
/// GET is streaming: writes a header frame, then N data frames, then an empty terminator.
/// No path restriction: the caller has full access to the filesystem visible to the process.
pub fn handle(conn: &mut Box<dyn Connection>, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return write_frame(conn, &resp_err("empty request"));
    }
    let path = parse_path(data)?;
    match data[0] {
        OP_LS => handle_ls(conn, path),
        OP_GET => handle_get(conn, path),
        _ => write_frame(conn, &resp_err("unknown fs operation")),
    }
}

fn handle_ls(conn: &mut Box<dyn Connection>, path: &str) -> io::Result<()> {
    let p = if path.is_empty() { "." } else { path };
    let entries = match fs::read_dir(p) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return write_frame(conn, &[ST_NOT_FOUND]);
        }
        Err(e) => return write_frame(conn, &resp_err(&e.to_string())),
    };
    // Payload: [ST_OK] repeated [type][u16 name_len][name]
    let mut payload = vec![ST_OK];
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        payload.push(if is_dir { ET_DIR } else { ET_FILE });
        payload.push_str(&name);
    }
    write_frame(conn, &payload)
}

fn handle_get(conn: &mut Box<dyn Connection>, path: &str) -> io::Result<()> {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return write_frame(conn, &[ST_NOT_FOUND]);
        }
        Err(e) => return write_frame(conn, &resp_err(&e.to_string())),
    };
    let size = f.metadata()?.len();
    // Header frame: [ST_OK][u64 size]
    let mut head = vec![ST_OK];
    head.push_u64(size);
    write_frame(conn, &head)?;
    // Streaming data frames
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        write_frame(conn, &buf[..n])?;
    }
    // Terminating empty frame signals end of stream
    write_frame(conn, &[])
}

// --- client-side helpers ---

/// Send a request on an existing connection and return the first response frame.
fn call_on_conn(conn: &mut Box<dyn Connection>, request: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(conn, request)?;
    read_frame(conn)
}

/// Send a request and return (connection, first response frame).
/// Caller owns the connection for further streaming reads.
fn call(
    t: &dyn Transport,
    addr: &str,
    request: &[u8],
) -> io::Result<(Box<dyn Connection>, Vec<u8>)> {
    let mut c = connect(t, addr)?;
    let first = call_on_conn(&mut c, request)?;
    Ok((c, first))
}

fn check_status(first: &[u8]) -> io::Result<()> {
    if first.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty response"));
    }
    match first[0] {
        ST_OK => Ok(()),
        ST_NOT_FOUND => Err(io::Error::new(io::ErrorKind::NotFound, "remote path not found")),
        _ => Err(io::Error::new(
            io::ErrorKind::Other,
            String::from_utf8_lossy(&first[1..]).to_string(),
        )),
    }
}

/// Parse the u64 file size from a GET response header frame: [ST_OK][u64 size].
fn parse_size_header(frame: &[u8]) -> io::Result<u64> {
    if frame.len() < 9 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated size header"));
    }
    Ok(u64::from_be_bytes([
        frame[1], frame[2], frame[3], frame[4],
        frame[5], frame[6], frame[7], frame[8],
    ]))
}

/// List a remote directory on an existing connection.
pub fn ls_on_conn(conn: &mut Box<dyn Connection>, path: &str) -> io::Result<Vec<FsEntry>> {
    let first = call_on_conn(conn, &req(OP_LS, path.as_bytes()))?;
    check_status(&first)?;
    let mut p = &first[1..];
    let mut out = Vec::new();
    while p.len() >= 3 {
        let et = p[0];
        let nlen = u16::from_be_bytes([p[1], p[2]]) as usize;
        p = &p[3..];
        if p.len() < nlen { break; }
        let name = String::from_utf8_lossy(&p[..nlen]).to_string();
        p = &p[nlen..];
        out.push(FsEntry { name, is_dir: et == ET_DIR });
    }
    Ok(out)
}

/// Stream a remote file on an existing connection into `writer`; returns total size.
pub fn get_on_conn<W: Write>(
    conn: &mut Box<dyn Connection>,
    remote: &str,
    writer: &mut W,
) -> io::Result<u64> {
    let first = call_on_conn(conn, &req(OP_GET, remote.as_bytes()))?;
    check_status(&first)?;
    let size = parse_size_header(&first)?;
    loop {
        let chunk = read_frame(conn)?;
        if chunk.is_empty() { break; }
        writer.write_all(&chunk)?;
    }
    Ok(size)
}

/// List a remote directory.
pub fn client_ls(t: &dyn Transport, addr: &str, path: &str) -> io::Result<Vec<FsEntry>> {
    let mut c = connect(t, addr)?;
    ls_on_conn(&mut c, path)
}

/// Stream a remote file into `writer`; returns the declared total size.
pub fn client_get<W: Write>(
    t: &dyn Transport,
    addr: &str,
    remote: &str,
    writer: &mut W,
) -> io::Result<u64> {
    client_get_progress(t, addr, remote, writer, |_, _| {})
}

/// Stream a remote file with a progress callback `on_chunk(received, total)`.
pub fn client_get_progress<W: Write, F: FnMut(u64, u64)>(
    t: &dyn Transport,
    addr: &str,
    remote: &str,
    writer: &mut W,
    mut on_chunk: F,
) -> io::Result<u64> {
    let (mut c, first) = call(t, addr, &req(OP_GET, remote.as_bytes()))?;
    check_status(&first)?;
    let size = parse_size_header(&first)?;
    // Read data frames until the empty terminator
    let mut received = 0u64;
    loop {
        let chunk = read_frame(&mut c)?;
        if chunk.is_empty() { break; }
        writer.write_all(&chunk)?;
        received += chunk.len() as u64;
        on_chunk(received, size);
    }
    Ok(size)
}
