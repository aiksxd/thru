use std::io::{self, Read, Write};
use std::time::Duration;

// Unified connection abstraction layer.
// Every protocol implements Transport -> Listener -> Connection.

/// A bidirectional byte stream that can be cloned and half-closed.
pub trait Connection: Read + Write + Send {
    /// Clone the connection for concurrent read/write access.
    fn try_clone(&self) -> io::Result<Box<dyn Connection>>;
    /// Shut down the write direction; signals EOF to the peer.
    fn shutdown_write(&self) -> io::Result<()>;
    /// Set the read timeout. None disables the timeout (blocking forever).
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// Get the remote peer address as a string (ip:port).
    fn peer_addr(&self) -> io::Result<String>;
}

/// Accepts incoming connections.
pub trait Listener: Send {
    fn accept(&mut self) -> io::Result<Box<dyn Connection>>;
}

/// Protocol entry point: bind a listener or dial an address.
pub trait Transport: Send + Sync {
    fn bind(&self, addr: &str) -> io::Result<Box<dyn Listener>>;
    fn connect(&self, addr: &str) -> io::Result<Box<dyn Connection>>;
}

/// Server wrapper around a Listener.
pub struct Server {
    l: Box<dyn Listener>,
}

impl Server {
    /// Bind a server via the given transport.
    pub fn bind(t: &dyn Transport, addr: &str) -> io::Result<Self> {
        Ok(Self { l: t.bind(addr)? })
    }
    /// Accept the next incoming connection.
    pub fn accept(&mut self) -> io::Result<Box<dyn Connection>> {
        self.l.accept()
    }
}

/// Dial an address via the given transport.
pub fn connect(t: &dyn Transport, addr: &str) -> io::Result<Box<dyn Connection>> {
    t.connect(addr)
}
