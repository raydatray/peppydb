//! The simulator's trace: every observation in order, and its hash.
//!
//! Two runs of one seed must record the same events in the same order, so
//! the hash of the trace is the determinism check (plan 0028, "The
//! simulator"). The safety checks read the same events.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

/// An append-only list of events with a running hash. Clones share it.
#[derive(Clone, Default)]
pub struct Trace {
    inner: Arc<Mutex<TraceInner>>,
}

#[derive(Default)]
struct TraceInner {
    hash: u64,
    count: u64,
    lines: Option<Vec<String>>,
}

/// FNV-1a, 64-bit: stable across platforms and Rust versions.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    if hash == 0 {
        hash = 0xcbf2_9ce4_8422_2325;
    }
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

impl Trace {
    /// A trace that also keeps every line, to find where two runs diverge.
    pub fn keeping_lines() -> Self {
        let trace = Self::default();
        trace.lock().lines = Some(Vec::new());
        trace
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TraceInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record one event.
    pub fn record(&self, event: impl fmt::Display) {
        let line = event.to_string();
        let mut inner = self.lock();
        inner.hash = fnv1a(inner.hash, line.as_bytes());
        inner.hash = fnv1a(inner.hash, b"\n");
        inner.count += 1;
        if let Some(lines) = &mut inner.lines {
            lines.push(line);
        }
    }

    pub fn hash(&self) -> u64 {
        self.lock().hash
    }

    pub fn len(&self) -> u64 {
        self.lock().count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The recorded lines, if this trace keeps them.
    pub fn lines(&self) -> Option<Vec<String>> {
        self.lock().lines.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_depends_on_content_and_order() {
        let run = |events: &[&str]| {
            let trace = Trace::default();
            events.iter().for_each(|e| trace.record(e));
            trace.hash()
        };
        assert_eq!(run(&["a", "b"]), run(&["a", "b"]));
        assert_ne!(run(&["a", "b"]), run(&["b", "a"]));
        assert_ne!(run(&["ab"]), run(&["a", "b"]));
    }
}
