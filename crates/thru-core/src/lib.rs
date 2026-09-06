use std::io::{self, Read, Write};

// Length-prefixed frame codec: 4-byte big-endian length + payload.

/// Maximum allowed frame payload size (64 MiB).
/// Frames larger than this are rejected to prevent memory-exhaustion attacks
/// where a peer claims a 4 GiB payload and forces an immediate large allocation.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

// Shared status codes used across dict / fs / exec response frames.
pub const ST_OK: u8 = 0;
pub const ST_NOT_FOUND: u8 = 1;
pub const ST_ERR: u8 = 2;

/// Server status query opcode (extension, not part of any sub-crate's range).
pub const OP_STATUS: u8 = 50;

/// Write a single length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame payload too large: {} bytes (max {})", data.len(), MAX_FRAME),
        ));
    }
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)
}

/// Read a single length-prefixed frame.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h)?;
    let n = u32::from_be_bytes(h) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload too large: {} bytes (max {})", n, MAX_FRAME),
        ));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

// --- wire-format encoding helpers ---

/// Extension trait for `Vec<u8>` that adds big-endian primitive writers
/// and length-prefixed string writers. Eliminates repetitive
/// `extend_from_slice(&v.to_be_bytes())` patterns across protocol crates.
pub trait BufExt {
    fn push_u16(&mut self, v: u16);
    fn push_u32(&mut self, v: u32);
    fn push_u64(&mut self, v: u64);
    /// Write a length-prefixed string: [u16 len][bytes].
    fn push_str(&mut self, s: &str);
}

impl BufExt for Vec<u8> {
    fn push_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn push_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn push_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn push_str(&mut self, s: &str) {
        self.push_u16(s.len() as u16);
        self.extend_from_slice(s.as_bytes());
    }
}
