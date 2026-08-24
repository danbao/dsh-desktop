//! In-memory log history for the current application session.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// A log line as stored by the backend and delivered to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: u64,
    pub timestamp_ms: u64,
    pub source: String,
    pub line: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub has_more: bool,
}

#[derive(Default)]
struct LogHistory {
    next_id: u64,
    entries: Vec<LogEntry>,
}

/// Thread-safe, non-persistent history. It deliberately has no line cap: the
/// user can inspect the complete current session until clearing or exiting.
#[derive(Default)]
pub struct LogBuffer {
    history: Mutex<LogHistory>,
}

impl LogBuffer {
    pub fn push(&self, source: &str, line: &str) -> LogEntry {
        let mut history = self.history.lock().expect("log history lock");
        history.next_id += 1;
        let entry = LogEntry {
            id: history.next_id,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            source: source.to_string(),
            line: line.to_string(),
        };
        history.entries.push(entry.clone());
        entry
    }

    pub fn page_after(&self, after_id: Option<u64>, limit: usize) -> LogPage {
        let history = self.history.lock().expect("log history lock");
        let start = after_id
            .map(|id| history.entries.partition_point(|entry| entry.id <= id))
            .unwrap_or(0);
        let end = start.saturating_add(limit).min(history.entries.len());
        LogPage {
            entries: history.entries[start..end].to_vec(),
            has_more: end < history.entries.len(),
        }
    }

    /// Clear all entries and return the greatest ID covered by the operation.
    /// IDs are never reused, allowing the UI to preserve logs arriving after
    /// the clear without racing an in-flight event.
    pub fn clear(&self) -> u64 {
        let mut history = self.history.lock().expect("log history lock");
        let cleared_through = history.next_id;
        history.entries.clear();
        cleared_through
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::LogBuffer;

    #[test]
    fn pages_large_histories_without_dropping_entries() {
        let buffer = LogBuffer::default();
        for index in 0..10_000 {
            buffer.push("test", &format!("line {index}"));
        }

        let mut entries = Vec::new();
        let mut after_id = None;
        loop {
            let page = buffer.page_after(after_id, 2_000);
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            after_id = entries.last().map(|entry| entry.id);
        }

        assert_eq!(entries.len(), 10_000);
        assert!(entries.windows(2).all(|pair| pair[1].id == pair[0].id + 1));
        assert_eq!(entries.first().unwrap().line, "line 0");
        assert_eq!(entries.last().unwrap().line, "line 9999");
    }

    #[test]
    fn assigns_unique_ids_to_concurrent_writers() {
        let buffer = Arc::new(LogBuffer::default());
        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let buffer = Arc::clone(&buffer);
                std::thread::spawn(move || {
                    for line in 0..500 {
                        buffer.push("test", &format!("{writer}:{line}"));
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let page = buffer.page_after(None, 4_000);
        assert_eq!(page.entries.len(), 4_000);
        assert!(page
            .entries
            .windows(2)
            .all(|pair| pair[1].id == pair[0].id + 1));
    }

    #[test]
    fn clear_does_not_reuse_ids() {
        let buffer = LogBuffer::default();
        let old = buffer.push("test", "old");
        let cleared_through = buffer.clear();
        let new = buffer.push("test", "new");

        assert_eq!(cleared_through, old.id);
        assert!(new.id > cleared_through);
        let page = buffer.page_after(None, 10);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].line, "new");
    }
}
