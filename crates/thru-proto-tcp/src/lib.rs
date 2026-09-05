use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;
use thru_transport::{Connection, Listener, Transport};

// TCP protocol implementation of the thru-transport traits.

/// TCP transport handle (zero-sized).
pub struct Tcp;

/// Newtype wrapper around TcpStream to satisfy the orphan rule.
pub struct S(pub TcpStream);

impl Read for S {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> { self.0.read(b) }
}
impl Write for S {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> { self.0.write(b) }
    fn flush(&mut self) -> io::Result<()> { self.0.flush() }
}

impl Connection for S {
    fn try_clone(&self) -> io::Result<Box<dyn Connection>> {
        Ok(Box::new(S(self.0.try_clone()?)))
    }
    fn shutdown_write(&self) -> io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
    fn peer_addr(&self) -> io::Result<String> {
        self.0.peer_addr().map(|a| a.to_string())
    }
}

/// TCP listener wrapper.
struct L(TcpListener);

impl Listener for L {
    fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
        let (s, _) = self.0.accept()?;
        Ok(Box::new(S(s)))
    }
}

impl Transport for Tcp {
    fn bind(&self, addr: &str) -> io::Result<Box<dyn Listener>> {
        Ok(Box::new(L(TcpListener::bind(addr)?)))
    }
    fn connect(&self, addr: &str) -> io::Result<Box<dyn Connection>> {
        Ok(Box::new(S(TcpStream::connect(addr)?)))
    }
}
