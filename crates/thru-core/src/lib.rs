use std::io::{self, Read, Write};

// Length-prefixed frame codec: 4-byte big-endian length + payload.

/// Write a single length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)
}

/// Read a single length-prefixed frame.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h)?;
    let n = u32::from_be_bytes(h) as usize;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}
