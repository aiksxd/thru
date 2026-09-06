use std::fs;
use std::io::{self, Read, Write};
use std::process::Command;
use std::thread;
use thru_core::{read_frame, write_frame};
use thru_transport::Connection;
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

// Operation codes (30-39 reserved for exec/pty)
pub const OP_SHELL_OPEN: u8 = 30;
pub const OP_RESIZE: u8 = 31;
pub const OP_EXEC_SCRIPT: u8 = 32;

// Status codes (re-exported from thru-core)
pub use thru_core::{ST_ERR, ST_OK};

// --- shell resolution: inherit the environment that started thru ---

/// Resolve the shell to spawn: THRU_SHELL > platform default.
/// Unix: $SHELL or /bin/sh; Windows: %COMSPEC% (cmd.exe).
pub fn default_shell() -> String {
    if let Ok(s) = std::env::var("THRU_SHELL") {
        if !s.is_empty() {
            return s;
        }
    }
    if cfg!(unix) {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    } else {
        // Windows: default to %COMSPEC% (cmd.exe) for predictable script behavior.
        // PowerShell can be selected via THRU_SHELL=powershell.exe.
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

// --- PTY session ---

/// Build a PtySize with zero pixel dimensions (the common case).
fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

/// A live PTY session: master pty + child shell + split reader/writer.
pub struct PtySession {
    master: Box<dyn MasterPty>,
    child: Box<dyn Child>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
}

impl PtySession {
    /// Create a PTY and spawn the inherited shell at the current working directory.
    pub fn new(cols: u16, rows: u16) -> io::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(pty_size(rows, cols))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let shell = default_shell();
        let mut cmd = CommandBuilder::new(&shell);
        // Inherit the working directory of the thru process.
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        // Environment variables are inherited by default.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(Self {
            master: pair.master,
            child,
            reader: Some(reader),
            writer: Some(writer),
        })
    }

    pub fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    pub fn take_writer(&mut self) -> Option<Box<dyn Write + Send>> {
        self.writer.take()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(pty_size(rows, cols))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child
            .wait()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

// --- wire helpers ---

fn parse_size(data: &[u8]) -> io::Result<(u16, u16)> {
    if data.len() < 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated size"));
    }
    let cols = u16::from_be_bytes([data[1], data[2]]);
    let rows = u16::from_be_bytes([data[3], data[4]]);
    Ok((cols, rows))
}

// --- server-side handlers ---

/// Interactive PTY shell: SHELL_OPEN then bidirectional raw byte stream.
/// RESIZE frames (op 31) are intercepted; all other frames are keystrokes.
/// Empty frame from client = disconnect.
pub fn handle_shell_open(conn: &mut Box<dyn Connection>, data: &[u8]) -> io::Result<()> {
    let (cols, rows) = parse_size(data)?;
    let mut session = PtySession::new(cols, rows)?;

    // PTY output -> client frames (own thread).
    let mut out_conn = conn.try_clone()?;
    let mut pty_reader = session
        .take_reader()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no pty reader"))?;
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_frame(&mut out_conn, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Client frames -> PTY input (or RESIZE handling).
    let mut pty_writer = session
        .take_writer()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no pty writer"))?;
    loop {
        if session.try_wait()?.is_some() {
            break;
        }
        match read_frame(conn) {
            Ok(frame) => {
                if frame.is_empty() {
                    break; // client requested disconnect
                }
                if frame[0] == OP_RESIZE {
                    if let Ok((c, r)) = parse_size(&frame) {
                        let _ = session.resize(c, r);
                    }
                } else {
                    pty_writer.write_all(&frame)?;
                    pty_writer.flush()?;
                }
            }
            Err(_) => break,
        }
    }

    let _ = reader_handle.join();
    Ok(())
}

/// Non-interactive script execution.
/// Uses std::process::Command (not PTY) for reliability:
///   Unix:    <user-shell> -e -c "script"  (set -e = abort on first error; shell from THRU_SHELL/$SHELL)
///   Windows: temp .bat with `if errorlevel 1 exit` after each command
/// Streams stdout+stderr back; final non-empty frame is the i32 exit code.
pub fn handle_exec_script(conn: &mut Box<dyn Connection>, data: &[u8]) -> io::Result<()> {
    if data.len() < 5 {
        return write_frame(conn, &[ST_ERR]);
    }
    let script_len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
    if data.len() < 5 + script_len {
        return write_frame(conn, &[ST_ERR]);
    }
    let script = String::from_utf8_lossy(&data[5..5 + script_len]).to_string();

    let output = if cfg!(unix) {
        // Use the user's resolved shell (THRU_SHELL > $SHELL > /bin/sh)
        // instead of hardcoding sh, so zsh/bash builtins and aliases work.
        let shell = default_shell();
        Command::new(&shell)
            .arg("-e")
            .arg("-c")
            .arg(&script)
            .output()?
    } else {
        // Windows: write a temp .bat with error-abort after each command.
        // Use PID + monotonic counter for uniqueness (concurrent exec_script calls
        // in the same process would otherwise overwrite each other's .bat file).
        static BAT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let counter = BAT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let bat_path = std::env::temp_dir().join(format!("thru_exec_{}_{}.bat", std::process::id(), counter));
        let mut bat = String::from("@echo off\r\n");
        for line in script.lines() {
            if !line.trim().is_empty() {
                bat.push_str(line);
                bat.push_str("\r\nif errorlevel 1 exit\r\n");
            }
        }
        fs::write(&bat_path, bat)?;
        // RAII guard ensures the temp .bat is removed even if Command::output panics.
        struct BatGuard(std::path::PathBuf);
        impl Drop for BatGuard {
            fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
        }
        let _guard = BatGuard(bat_path.clone());
        let out = Command::new("cmd.exe")
            .arg("/Q")
            .arg("/C")
            .arg(&bat_path)
            .output()?;
        out
    };

    // Stream stdout then stderr as output frames.
    if !output.stdout.is_empty() {
        write_frame(conn, &output.stdout)?;
    }
    if !output.stderr.is_empty() {
        write_frame(conn, &output.stderr)?;
    }

    let code = output.status.code().unwrap_or(1);
    // Final frame: i32 exit code (big-endian).
    write_frame(conn, &(code as i32).to_be_bytes())?;
    write_frame(conn, &[]) // terminator
}

// --- client-side helpers ---

/// Execute a script on an existing connection; returns (combined_output, exit_code).
pub fn exec_script_on_conn(conn: &mut Box<dyn Connection>, script: &str) -> io::Result<(String, i32)> {
    let mut frame = vec![OP_EXEC_SCRIPT];
    frame.extend_from_slice(&(script.len() as u32).to_be_bytes());
    frame.extend_from_slice(script.as_bytes());
    write_frame(conn, &frame)?;

    let mut frames: Vec<Vec<u8>> = Vec::new();
    loop {
        let f = read_frame(conn)?;
        if f.is_empty() { break; }
        frames.push(f);
    }

    let exit_code = if let Some(last) = frames.last() {
        if last.len() == 4 {
            i32::from_be_bytes([last[0], last[1], last[2], last[3]])
        } else { 1 }
    } else { 1 };

    let mut output = Vec::new();
    for f in &frames[..frames.len().saturating_sub(1)] {
        output.extend_from_slice(f);
    }
    Ok((String::from_utf8_lossy(&output).to_string(), exit_code))
}
