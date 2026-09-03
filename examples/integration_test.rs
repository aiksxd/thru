// Full integration test: dict + fs + exec against a live server.
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use thru_core::{read_frame, write_frame};
use thru_dict::Dict;
use thru_fs::{client_get, client_ls};
use thru_proto_tcp::Tcp;
use thru_transport::{Server, Transport};

const ADDR: &str = "127.0.0.1:55580";
const T: &dyn Transport = &Tcp;

fn main() -> std::io::Result<()> {
    // Prepare test files.
    fs::write("integration_small.txt", b"hello integration test\n")?;
    let big_data: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    fs::write("integration_big.bin", &big_data)?;

    // Start full server in a thread (all opcodes: dict 0-9, fs 20-29, exec 30/32).
    let dict = Arc::new(Dict::new());
    let dict_clone = dict.clone();
    thread::spawn(move || {
        let mut s = Server::bind(T, ADDR).expect("bind");
        loop {
            if let Ok(mut c) = s.accept() {
                let dict = dict_clone.clone();
                thread::spawn(move || {
                    while let Ok(f) = read_frame(&mut c) {
                        if f.is_empty() { continue; }
                        match f[0] {
                            0..=9 => {
                                let r = dict.handle(&f);
                                let _ = write_frame(&mut c, &r);
                            }
                            20..=29 => {
                                let _ = thru_fs::handle(&mut c, &f);
                            }
                            30 => {
                                let _ = thru_exec::handle_shell_open(&mut c, &f);
                            }
                            32 => {
                                let _ = thru_exec::handle_exec_script(&mut c, &f);
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
    });
    thread::sleep(Duration::from_millis(500));

    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut check = |name: &str, ok: bool| {
        if ok { pass += 1; println!("  PASS: {name}"); }
        else { fail += 1; println!("  FAIL: {name}"); }
    };

    // ===== 1. DICT =====
    println!("\n=== 1. Public Dictionary (thru-dict) ===");

    // set
    thru_dict::client_set(T, ADDR, "msgA", b"hello dict")?;
    check("set msgA", true);

    // get
    let v = thru_dict::client_get(T, ADDR, "msgA")?;
    check("get msgA = 'hello dict'", v.as_deref() == Some(b"hello dict"));

    // append
    thru_dict::client_append(T, ADDR, "msgA", b" appended")?;
    let v = thru_dict::client_get(T, ADDR, "msgA")?;
    check("append msgA = 'hello dict appended'", v.as_deref() == Some(b"hello dict appended"));

    // list keys
    let keys = thru_dict::client_keys(T, ADDR)?;
    check("list contains msgA", keys.contains(&"msgA".to_string()));

    // get nonexistent
    let v = thru_dict::client_get(T, ADDR, "no_such_key")?;
    check("get nonexistent = None", v.is_none());

    // binary value
    let bin = vec![0u8, 1, 2, 255, 128];
    thru_dict::client_set(T, ADDR, "binKey", &bin)?;
    let v = thru_dict::client_get(T, ADDR, "binKey")?;
    check("binary value round-trip", v.as_deref() == Some(bin.as_slice()));

    // ===== 2. FILE TRANSFER =====
    println!("\n=== 2. File Transfer (thru-fs) ===");

    // ls
    let entries = client_ls(T, ADDR, ".")?;
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    check("ls contains integration_small.txt", names.contains(&"integration_small.txt"));
    check("ls contains integration_big.bin", names.contains(&"integration_big.bin"));

    // get small file
    let mut buf = Vec::new();
    let size = client_get(T, ADDR, "integration_small.txt", &mut buf)?;
    check("get small: size=23", size == 23);
    check("get small: content match", buf == b"hello integration test\n");

    // get big file (5 MiB, multi-chunk streaming)
    let mut buf = Vec::new();
    let size = client_get(T, ADDR, "integration_big.bin", &mut buf)?;
    check("get big: size=5242880", size == 5 * 1024 * 1024);
    check("get big: content match (5MB)", buf == big_data);

    // get nonexistent file
    let mut buf = Vec::new();
    let result = client_get(T, ADDR, "no_such_file.xyz", &mut buf);
    check("get nonexistent = NotFound error", result.is_err());

    // ===== 3. REMOTE COMMAND EXEC =====
    println!("\n=== 3. Remote Command Execution (thru-exec) ===");

    // simple command
    let (out, code) = exec_script("echo hello_exec")?;
    check("exec: echo outputs 'hello_exec'", out.contains("hello_exec"));
    check("exec: exit code 0", code == 0);

    // cd + command relay (state persists within script)
    let (out, code) = exec_script("cd src\necho in_dir=%CD%")?;
    check("exec: cd relay works", out.contains("in_dir=") && out.contains("src"));
    check("exec: cd relay exit 0", code == 0);

    // error abort: command after failure should NOT execute
    let (out, code) = exec_script("echo before_step\nnonexistent_cmd_xyz\necho after_step")?;
    check("exec: 'before' output present", out.contains("before_step"));
    check("exec: 'after' output absent (aborted)", !out.contains("after_step"));
    check("exec: nonzero exit on failure", code != 0);

    // multi-line script with multiple successful commands
    let (out, code) = exec_script("echo line1\necho line2\necho line3")?;
    check("exec: multi-line all output", out.contains("line1") && out.contains("line2") && out.contains("line3"));
    check("exec: multi-line exit 0", code == 0);

    // ===== SUMMARY =====
    println!("\n{}", "=".repeat(50));
    println!("  RESULTS: {pass} passed, {fail} failed");
    println!("{}", "=".repeat(50));

    // Cleanup
    let _ = fs::remove_file("integration_small.txt");
    let _ = fs::remove_file("integration_big.bin");

    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Execute a script via EXEC_SCRIPT, return (combined_output, exit_code).
fn exec_script(script: &str) -> std::io::Result<(String, i32)> {
    use thru_transport::connect;
    let mut conn = connect(T, ADDR)?;
    let mut frame = vec![thru_exec::OP_EXEC_SCRIPT];
    frame.extend_from_slice(&(script.len() as u32).to_be_bytes());
    frame.extend_from_slice(script.as_bytes());
    write_frame(&mut conn, &frame)?;

    let mut frames: Vec<Vec<u8>> = Vec::new();
    loop {
        let f = read_frame(&mut conn)?;
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
