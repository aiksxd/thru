use std::process::exit;

mod auth;
mod client;
mod reverse;
mod server;

// CLI entry point.
//   server:  thru [port] [-p password]          (daemonizes, returns immediately)
//   connect: thru <host:port> [-p password]     (authenticate & cache session for later commands)
//   stop:    thru stop
//   shell:   thru shell [--connect addr]
//   exec:    thru exec <command> [--connect addr]
//   dict:    thru dict get/set/list ... [--connect addr]
//   fetch2:  thru fetch2 <local_dir> [--connect addr]
//   device:  thru device <local_dir> [--connect addr]

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("dict") => return client::dict_cmd(&args[1..]),
        Some("fetch2") => return client::fetch2_cmd(&args[1..]),
        Some("device") => return client::device_cmd(&args[1..]),
        Some("shell") => return client::shell_cmd(&args[1..]),
        Some("exec") => return client::exec_cmd(&args[1..]),
        Some("stop") => return server::stop_server(),
        Some("pid") => return server::pid_cmd(&args[1..]),
        Some("help") | Some("--help") | Some("-h") => { print_help(); return Ok(()); }
        _ => {}
    }

    // Not a subcommand — must be either server (numeric port) or client connect (host:port).
    let target = args.iter().find(|a| !a.starts_with('-') && a.as_str() != "-p");
    if let Some(t) = target {
        if t.contains(':') {
            if let Err(e) = validate_addr(t) {
                eprintln!("{e}");
                exit(1);
            }
            return reverse::connect_cmd(&args);
        }
        if t.parse::<u16>().is_err() {
            eprintln!("invalid argument: '{t}' — expected a port number (server) or host:port (client connect)");
            exit(1);
        }
    }
    server::server_cmd(&args)
}

/// Validate that an address string is well-formed "host:port" with a numeric port.
fn validate_addr(addr: &str) -> std::io::Result<()> {
    let parts: Vec<&str> = addr.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': expected host:port")));
    }
    let port_str = parts[0];
    let host = parts[1];
    if host.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': host is empty")));
    }
    if port_str.parse::<u16>().is_err() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
            format!("invalid address '{addr}': port '{port_str}' is not a valid number (1-65535)")));
    }
    Ok(())
}

fn print_help() {
    println!("thru — cross-platform device communication");
    println!();
    println!("USAGE:");
    println!("  thru [port] [-p password] [-m N]    Start server (daemon, returns immediately; -m max devices)");
    println!("  thru <host:port> [-p password]      Connect to server and keep reverse connection (password only here)");
    println!("  thru stop                             Stop the running server and reverse connection");
    println!("  thru pid                              Show server PID, active connections and device list");
    println!("  thru shell [--connect addr]          Interactive remote terminal");
    println!("  thru exec <command> [--connect addr] Execute command on remote side");
    println!("  thru dict get <key> [--connect addr]       Read a dictionary value");
    println!("  thru dict set <key> <value> [--connect addr]  Write a dictionary value");
    println!("  thru dict <key> [value] [-a] [--connect addr]  Shorthand: read if no value, write if value given (-a = append)");
    println!("  thru dict list [-v|-kv] [--connect addr]    List keys (-v = values only, -kv = key<TAB>value)");
    println!("  thru fetch2 <local_dir>                Download files from the default target device");
    println!("  thru device <local_dir>                Interactively select a device, then browse/download its files");
    println!("  thru help                              Show this help");
}
