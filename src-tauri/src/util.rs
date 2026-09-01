//! Shared helpers for streamed subprocesses and loopback HTTP probes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use tauri::{Emitter, Manager};

use std::sync::Arc;

use crate::{logfile, service::AppState};

/// Emit one log line to the UI console, the durable log file, and stderr.
pub fn emit_log(app: &tauri::AppHandle, source: &str, line: &str) {
    let state = app.state::<Arc<AppState>>();
    let entry = state.logs.push(source, line);
    logfile::append(source, line, entry.timestamp_ms);
    let _ = app.emit("log", entry);
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

    let started = Instant::now();
    let mut next_heartbeat = Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let elapsed = started.elapsed();
        if elapsed >= next_heartbeat {
            emit_log(
                app,
                source,
                &format!("仍在执行（已用时 {}）", format_elapsed(elapsed)),
            );
            next_heartbeat += Duration::from_secs(15);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    t_out.join().expect("stdout reader");
    t_err.join().expect("stderr reader");

    if !status.success() {
        return Err(anyhow!(
            "{} 退出码 {}（用时 {}，详情见日志）",
            source,
            status.code().unwrap_or(-1),
            format_elapsed(started.elapsed()),
        ));
    }
    Ok(())
}

pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    }
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

/// Whether the loopback port answers with any HTTP status line. The probe
/// only proves that an HTTP server is serving this port: newer harness
/// builds gate `/` behind a token and answer `401` to anonymous probes,
/// which still means "up". Readiness is not the place to re-check auth.
pub fn http_replies(port: u16) -> bool {
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
    head.starts_with("HTTP/1.1 ") || head.starts_with("HTTP/1.0 ")
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

#[cfg(test)]
mod tests {
    use super::{format_elapsed, http_replies};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn formats_command_elapsed_time_for_progress_logs() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0 秒");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59 秒");
        assert_eq!(format_elapsed(Duration::from_secs(61)), "1 分 1 秒");
        assert_eq!(format_elapsed(Duration::from_secs(3_605)), "60 分 5 秒");
    }

    /// Respond to one TCP request with the given raw head, then close.
    fn serve_raw_head(head: &'static [u8]) -> u16 {
        let listener = TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(head);
            }
        });
        port
    }

    #[test]
    fn http_replies_accepts_any_http_status_as_readiness() {
        // 200 responder
        assert!(http_replies(serve_raw_head(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )));
        // Token-gated servers answer 401 to anonymous probes — still "up".
        assert!(http_replies(serve_raw_head(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )));
        // A redirect (303 with Location) is equally proof of a live server.
        assert!(http_replies(serve_raw_head(
            b"HTTP/1.1 303 See Other\r\nLocation: /app\r\nConnection: close\r\n\r\n"
        )));
        // Garbage that is not HTTP must not count.
        assert!(!http_replies(serve_raw_head(b"not an http server\r\n\r\n")));
    }
}
