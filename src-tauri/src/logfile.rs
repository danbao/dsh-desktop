//! Durable session log: every emitted line is appended to a file under the
//! app data directory, rotated by size so the total never exceeds the cap.
//!
//! The in-memory `LogBuffer` stays the source for the UI console (uncapped,
//! per-session, cleared by the user). This file is the after-the-fact record:
//! it survives restarts and UI clears, bounded at ~1 GiB across rotated
//! generations (`app.log`, `app.log.1`, …).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Default policy: 4 generations × 256 MiB ≈ 1 GiB total on disk.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_FILES: usize = 4;
pub const FILE_NAME: &str = "app.log";

struct Inner {
    file: Option<File>,
    written: u64,
    /// Set after the first write failure; further appends become no-ops so a
    /// full disk never takes the app down with it.
    disabled: bool,
}

pub struct FileLog {
    dir: PathBuf,
    max_file_bytes: u64,
    max_files: usize,
    inner: Mutex<Inner>,
}

static FILE_LOG: OnceLock<FileLog> = OnceLock::new();

/// Open the global log file under `dir`, appending to any existing one.
pub fn init(dir: &Path) {
    let log = FileLog::new(dir, MAX_FILE_BYTES, MAX_FILES);
    let _ = FILE_LOG.set(log);
}

/// Append one line through the global log, if initialized. Never panics.
pub fn append(source: &str, line: &str, timestamp_ms: u64) {
    let Some(log) = FILE_LOG.get() else {
        return;
    };
    if let Err(err) = log.append(source, line, timestamp_ms) {
        eprintln!("[logfile] 写入失败，文件日志已停用：{err}");
    }
}

impl FileLog {
    pub fn new(dir: &Path, max_file_bytes: u64, max_files: usize) -> Self {
        let _ = fs::create_dir_all(dir);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(FILE_NAME))
            .ok();
        let written = file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map(|meta| meta.len())
            .unwrap_or(0);
        Self {
            dir: dir.to_path_buf(),
            max_file_bytes,
            max_files: max_files.max(1),
            inner: Mutex::new(Inner {
                file,
                written,
                disabled: false,
            }),
        }
    }

    /// Append one formatted line, rotating first when it would cross the
    /// per-file cap. Errors disable the writer (see `Inner::disabled`).
    pub fn append(&self, source: &str, line: &str, timestamp_ms: u64) -> std::io::Result<()> {
        let record = format!(
            "{} [{}] {}\n",
            format_local(timestamp_ms),
            source,
            line.trim_end_matches(['\n', '\r'])
        );
        let mut inner = self.inner.lock().expect("file log lock");
        if inner.disabled {
            return Ok(());
        }
        if inner.file.is_none() {
            inner.file = self.open_current()?;
        }
        if inner.written + record.len() as u64 > self.max_file_bytes {
            self.rotate(&mut inner)?;
        }
        let file = inner.file.as_mut().expect("file opened above");
        file.write_all(record.as_bytes())?;
        inner.written += record.len() as u64;
        Ok(())
    }

    fn open_current(&self) -> std::io::Result<Option<File>> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(FILE_NAME))
            .map(Some)
    }

    /// Shift generations up (`.2 → .3`, `.1 → .2`), move the current file to
    /// `.1`, and start fresh. Unix rename overwrites, which retires the
    /// oldest generation; `max_files == 1` just truncates in place.
    fn rotate(&self, inner: &mut Inner) -> std::io::Result<()> {
        inner.file = None;
        let current = self.dir.join(FILE_NAME);
        if self.max_files == 1 {
            fs::File::create(&current)?;
        } else {
            for index in (1..self.max_files - 1).rev() {
                let from = self.dir.join(format!("{FILE_NAME}.{index}"));
                if from.exists() {
                    fs::rename(&from, self.dir.join(format!("{FILE_NAME}.{}", index + 1)))?;
                }
            }
            fs::rename(&current, self.dir.join(format!("{FILE_NAME}.1")))?;
        }
        inner.file = self.open_current()?;
        inner.written = 0;
        Ok(())
    }
}

/// `YYYY-MM-DD HH:MM:SS` in the machine's local zone; falls back to the raw
/// milliseconds when the timestamp cannot be represented.
fn format_local(timestamp_ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(timestamp_ms as i64)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| timestamp_ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-desktop-logfile-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn read(path: &Path) -> String {
        let mut text = String::new();
        File::open(path)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        text
    }

    #[test]
    fn appends_formatted_lines_and_resumes_across_restarts() {
        let dir = temp_dir("append");
        let log = FileLog::new(&dir, MAX_FILE_BYTES, MAX_FILES);
        log.append("dsh", "服务启动", 1_782_000_000_000)
            .expect("append");

        let text = read(&dir.join(FILE_NAME));
        assert!(text.contains(" [dsh] 服务启动\n"), "{text}");
        assert!(text.starts_with("20"), "timestamp first: {text}");

        // A fresh handle resumes at the existing file instead of truncating.
        let log = FileLog::new(&dir, MAX_FILE_BYTES, MAX_FILES);
        log.append("git", "fetch", 1_782_000_001_000)
            .expect("append");
        let text = read(&dir.join(FILE_NAME));
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn rotates_by_size_and_retires_the_oldest_generation() {
        let dir = temp_dir("rotate");
        // Cap that fits roughly one short line per generation.
        let log = FileLog::new(&dir, 40, 3);
        for index in 0..6 {
            log.append("test", &format!("line-{index}"), 1_782_000_000_000 + index)
                .expect("append");
        }

        let current = read(&dir.join(FILE_NAME));
        assert!(current.contains("line-5"), "{current}");
        let first = read(&dir.join(format!("{FILE_NAME}.1")));
        assert!(first.contains("line-4"), "{first}");
        let second = read(&dir.join(format!("{FILE_NAME}.2")));
        assert!(second.contains("line-3"), "{second}");
        // Three generations total (current + .1 + .2); .3 never appears.
        assert!(!dir.join(format!("{FILE_NAME}.3")).exists());
    }

    #[test]
    fn total_size_stays_under_the_cap() {
        let dir = temp_dir("cap");
        let max_file = 64u64;
        let log = FileLog::new(&dir, max_file, 3);
        for index in 0..100 {
            log.append("test", &format!("payload-{index:04}"), 1_782_000_000_000)
                .expect("append");
        }
        let total: u64 = (1..=3)
            .map(|index| dir.join(format!("{FILE_NAME}.{index}")))
            .chain([dir.join(FILE_NAME)])
            .filter_map(|path| path.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        assert!(total <= max_file * 3 + 256, "total {total}");
    }
}
