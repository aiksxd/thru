// End-to-end test for thru-fs: starts an fs-only server in a thread,
// then exercises client_ls and client_get including a 2.5 MiB file.
use std::thread;
use std::time::Duration;
use thru_core::read_frame;
use thru_fs::{client_get, client_ls, handle};
use thru_proto_tcp::Tcp;
use thru_transport::{Server, Transport};

fn main() -> std::io::Result<()> {
    let addr = "127.0.0.1:55561";
    let t: &dyn Transport = &Tcp;

    // Start a minimal fs-only server in a background thread.
    let srv_t = Tcp;
    let srv_addr = addr.to_string();
    thread::spawn(move || {
        let mut s = Server::bind(&srv_t, &srv_addr).unwrap();
        loop {
            if let Ok(mut c) = s.accept() {
                thread::spawn(move || {
                    while let Ok(f) = read_frame(&mut c) {
                        if !f.is_empty() && (20..=29).contains(&f[0]) {
                            let _ = handle(&mut c, &f);
                        }
                    }
                });
            }
        }
    });
    thread::sleep(Duration::from_millis(300));

    // --- LS ---
    println!("=== LS . ===");
    match client_ls(t, addr, ".") {
        Ok(entries) => {
            for e in &entries {
                println!("  {} {}", if e.is_dir { "DIR " } else { "FILE" }, e.name);
            }
            println!("  total: {} entries", entries.len());
        }
        Err(e) => println!("  ERROR: {e}"),
    }

    // --- GET small file ---
    println!("=== GET testfile.txt ===");
    let mut buf = Vec::new();
    match client_get(t, addr, "testfile.txt", &mut buf) {
        Ok(size) => {
            println!("  declared size: {size}");
            println!("  received: {} bytes", buf.len());
            println!("  content: {:?}", String::from_utf8_lossy(&buf));
        }
        Err(e) => println!("  ERROR: {e}"),
    }

    // --- GET big file (2.5 MiB, exercises multi-frame streaming) ---
    println!("=== GET bigfile.bin (2.5 MiB) ===");
    let mut buf = Vec::new();
    match client_get(t, addr, "bigfile.bin", &mut buf) {
        Ok(size) => {
            println!("  declared size: {size}");
            println!("  received: {} bytes", buf.len());
            println!("  size match: {}", size == buf.len() as u64);
        }
        Err(e) => println!("  ERROR: {e}"),
    }

    // --- GET nonexistent file (expect NotFound) ---
    println!("=== GET nonexistent.xyz ===");
    let mut buf = Vec::new();
    match client_get(t, addr, "nonexistent.xyz", &mut buf) {
        Ok(size) => println!("  UNEXPECTED success: {size}"),
        Err(e) => println!("  expected error: {e} (kind={:?})", e.kind()),
    }

    println!("\nAll tests completed.");
    Ok(())
}
