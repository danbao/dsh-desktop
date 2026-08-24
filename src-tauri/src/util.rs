//! Shared helpers for streamed subprocesses and loopback HTTP probes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context};
use tauri::Emitter;

/// Emit one log line to the UI console (and stderr).
pub fn emit_log(app: &tauri::AppHandle, source: &str, line: &str) {
    let _ = app.emit("log", serde_json::json!({ "source": source, "line": line }));
    eprintln!("[{source}] {line}");
}

/// Run a command, streaming its stdout/stderr lines as `log` events; fail on a
/// nonzero exit with the last output lines attached for diagnosis.
pub fn stream_command(
    cmd: &mut Command,
    app: &tauri::AppHandle,
    source: &str,
) -> anyhow::Result<()> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("无法启动 {}", cmd.get_program().to_string_lossy()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let app_out = app.clone();
    let src_out = source.to_string();
    let t_out = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        pump_lines(&mut reader, &app_out, &src_out)
    });
    let app_err = app.clone();
    let src_err = source.to_string();
    let t_err = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        pump_lines(&mut reader, &app_err, &src_err)
    });

    // Keep the tail of each stream for error reporting after the joins below.
    let status = child.wait()?;
    t_out.join().expect("stdout reader");
    t_err.join().expect("stderr reader");

    if !status.success() {
        return Err(anyhow!(
            "{} 退出码 {}（详情见日志）",
            source,
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Pump one subprocess stream into the log, line by line.
pub fn pump_lines<R: BufRead>(reader: &mut R, app: &tauri::AppHandle, source: &str) {
    let mut buf = Vec::new();
    loop {
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let owned = String::from_utf8_lossy(&buf).into_owned();
                let line = owned.trim_end_matches(['\n', '\r']);
                if !line.is_empty() {
                    emit_log(app, source, line);
                }
            }
        }
    }
}

/// Whether `GET /` on this loopback port answers `200`.
pub fn http_ok(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) else {
        return false;
    };
    let request = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 64];
    let n = stream.read(&mut head).unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

/// Whether a loopback listener can still bind this port right now.
pub fn port_free(port: u16) -> bool {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_ok()
}

/// Send a signal to a whole process group (`-pgid`); children of the spawned
/// service die with it.
pub fn kill_group(pgid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pgid as i32), sig);
    }
}
