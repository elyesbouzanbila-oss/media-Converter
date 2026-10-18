//! A bounded in-memory log backing the console tab.
//!
//! `tracing` records land here through a `MakeWriter` adapter, so once the
//! real engine (M1) logs through the facade, its output shows up in the UI's
//! log tab with no plumbing change. The ring keeps the last `CAP` lines;
//! older lines are dropped and counted, because a converter that silently
//! discards diagnostics is worse than one that says so.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const CAP: usize = 10_000;

#[derive(Debug)]
pub struct LogRing {
    lines: VecDeque<String>,
    cap: usize,
    dropped: u64,
    born: Instant,
    partial: String,
}

impl LogRing {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(1024),
            cap: CAP,
            dropped: 0,
            born: Instant::now(),
            partial: String::new(),
        }
    }

    /// Timestamp prefix relative to app start: stable across the whole run and
    /// free of any date-format dependency until the project has one.
    fn stamp(&self, s: &str) -> String {
        format!("[+{:>7.1}s] {}", self.born.elapsed().as_secs_f32(), s)
    }

    pub fn push(&mut self, line: impl Into<String>) {
        self.lines.push_back(line.into());
        if self.lines.len() > self.cap {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }

    /// Push with the elapsed-time prefix; the console's one-line world.
    pub fn push_timed(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.push(self.stamp(&line));
    }

    /// Cap override, only used by tests that must not push 10 000 lines.
    #[cfg(test)]
    fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
    }

    /// Number of lines silently dropped by the cap (never silent to the UI:
    /// the console hat shows this total when nonzero).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Last `n` lines, oldest first. Cloned so the caller can paint without
    /// holding the lock across widget building.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let start = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(start).cloned().collect()
    }
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// tracing plumbing
// ---------------------------------------------------------------------------

/// Shared handle: the app holds one, `tracing` holds one, whichever.
#[derive(Clone)]
pub struct LogHandle(pub Arc<Mutex<LogRing>>);

impl LogHandle {
    pub fn new(ring: Arc<Mutex<LogRing>>) -> Self {
        Self(ring)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, LogRing> {
        self.0.lock().unwrap()
    }
}

impl io::Write for LogHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // `tracing` records arrive in pieces; only whole lines go to the ring.
        let mut ring = self.0.lock().unwrap();
        ring.partial.push_str(&String::from_utf8_lossy(buf));
        while let Some(pos) = ring.partial.find('\n') {
            let line: String = ring.partial.drain(..=pos).collect();
            let line = line.trim_end();
            if !line.trim().is_empty() {
                let stamped = ring.stamp(line);
                ring.push(stamped);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogHandle {
    type Writer = LogHandle;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the tracing subscriber writing into a fresh ring. Returns the
/// handle the UI will read. `RUST_LOG` wins when set, `info` otherwise.
pub fn install_tracing() -> LogHandle {
    use tracing_subscriber::EnvFilter;

    let ring = Arc::new(Mutex::new(LogRing::new()));
    let handle = LogHandle::new(Arc::clone(&ring));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(handle.clone())
        .try_init();
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_drops_oldest_and_counts() {
        let mut ring = LogRing::new();
        ring.set_cap(3);
        for i in 0..5 {
            ring.push(format!("line {i}"));
        }
        let tail = ring.tail(10);
        assert_eq!(tail, vec!["line 2", "line 3", "line 4"]);
        assert_eq!(ring.dropped(), 2);
    }

    #[test]
    fn writer_buffers_partial_records() {
        let handle = LogHandle::new(Arc::new(Mutex::new(LogRing::new())));
        io::Write::write(&mut handle.clone(), b"err: demo failed ").unwrap();
        assert!(handle.lock().tail(10).is_empty(), "no complete line yet");
        io::Write::write(&mut handle.clone(), b"at step 3\n(torch)\n").unwrap();
        let tail = handle.lock().tail(10);
        assert_eq!(tail.len(), 2);
        assert!(tail[0].contains("err: demo failed"));
    }
}
