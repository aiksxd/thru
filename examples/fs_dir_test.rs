// Test recursive directory download (download_dir logic).
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;
use thru_core::read_frame;
use thru_fs::{client_get_progress, client_ls, handle};
use thru_proto_tcp::Tcp;
use thru_transport::{Server, Transport};

fn main() -> std::io::Result<()> {
    let addr = "127.0.0.1:55564";
    let t: &dyn Transport = &Tcp;

    // Start fs-only server.
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

    // Recursively download "remotetest" into "dl_dir_test".
    let remote_dir = "remotetest";
    let local_dir = "dl_dir_test";
    if Path::new(local_dir).exists() {
        fs::remove_dir_all(local_dir)?;
    }
    download_dir(t, addr, remote_dir, local_dir)?;

    // Verify.
    println!("=== downloaded tree ===");
    print_tree(local_dir);
    println!();
    let f1 = fs::read_to_string(format!("{local_dir}/file1.txt"))?;
    let f2 = fs::read_to_string(format!("{local_dir}/subdir/file2.txt"))?;
    println!("file1.txt = {:?}", f1);
    println!("subdir/file2.txt = {:?}", f2);
    println!("match: {}", f1 == "file1 content" && f2 == "file2 in subdir");

    // Cleanup.
    fs::remove_dir_all(local_dir)?;
    Ok(())
}

/// Recursive directory download (mirrors main.rs download_dir).
fn download_dir(t: &dyn Transport, addr: &str, remote_dir: &str, local_dir: &str) -> std::io::Result<()> {
    fs::create_dir_all(local_dir)?;
    let entries = client_ls(t, addr, remote_dir)?;
    for e in entries {
        let remote = if remote_dir == "." || remote_dir.is_empty() {
            e.name.clone()
        } else {
            format!("{remote_dir}/{}", e.name)
        };
        let local = Path::new(local_dir).join(&e.name);
        if e.is_dir {
            download_dir(t, addr, &remote, &local.to_string_lossy())?;
        } else {
            let mut f = fs::File::create(&local)?;
            let name = e.name.clone();
            client_get_progress(t, addr, &remote, &mut f, |r, total| {
                eprint!("\r  {name}: {r}/{total}   ");
                let _ = std::io::stderr().flush();
            })?;
            eprintln!("\r  {name}: done              ");
        }
    }
    Ok(())
}

fn print_tree(dir: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            if path.is_dir() {
                println!("  {name}/");
                print_tree(&path.to_string_lossy());
            } else {
                println!("  {name}");
            }
        }
    }
}
