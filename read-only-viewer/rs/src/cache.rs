//! Incremental scanning — the reason the port is worth anything.
//!
//! The Python re-reads and re-parses every transcript tail on every collect:
//! measured at 146 MB across 263 files, every 3 seconds, to discover about 2 KB
//! of genuinely new events. 99.9984% of that work is redundant, and with a 2 s
//! cache TTL against a 3 s poll the server spends ~80% of wall-clock re-parsing
//! bytes it already parsed.
//!
//! This caches each file's scan keyed on `(size, mtime)` and re-reads only files
//! that actually moved.
//!
//! It is deliberately NOT byte-offset incrementality. The scanners compute
//! aggregates over the whole tail window — turn counts, cumulative usage, the
//! model timeline, compaction resets — so resuming mid-file would mean making
//! every one of them resumable, which is a far larger change with real
//! correctness risk. This version rests on a single provable claim:
//!
//!     a file whose size AND mtime are unchanged yields the same scan
//!
//! and the equivalence gate checks exactly that by comparing a warm process
//! against a cold one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    size: u64,
    mtime: Option<SystemTime>,
}

pub fn stamp(path: &Path) -> Stamp {
    match path.metadata() {
        Ok(m) => Stamp { size: m.len(), mtime: m.modified().ok() },
        // A file we cannot stat gets a stamp that never matches, so it is
        // rescanned rather than silently served from a stale entry.
        Err(_) => Stamp { size: u64::MAX, mtime: None },
    }
}

/// A scan cache keyed by path. `V` is whatever the caller's scan produces.
pub struct ScanCache<V> {
    inner: Mutex<HashMap<PathBuf, (Stamp, V)>>,
    hits: Mutex<(u64, u64)>, // (hits, misses)
}

impl<V: Clone> Default for ScanCache<V> {
    fn default() -> Self {
        Self { inner: Mutex::new(HashMap::new()), hits: Mutex::new((0, 0)) }
    }
}

impl<V: Clone> ScanCache<V> {
    /// Return the cached scan when the file has not moved, else run `scan`.
    pub fn get_or_scan<F: FnOnce() -> V>(&self, path: &Path, scan: F) -> V {
        let now = stamp(path);
        if let Ok(map) = self.inner.lock() {
            if let Some((seen, value)) = map.get(path) {
                if *seen == now && now.mtime.is_some() {
                    if let Ok(mut h) = self.hits.lock() {
                        h.0 += 1;
                    }
                    return value.clone();
                }
            }
        }
        if let Ok(mut h) = self.hits.lock() {
            h.1 += 1;
        }
        let value = scan();
        if let Ok(mut map) = self.inner.lock() {
            map.insert(path.to_path_buf(), (now, value.clone()));
        }
        value
    }

    /// Forget entries whose file is gone, so a long-lived server does not grow
    /// an entry per transcript ever seen.
    ///
    /// Keyed on the file still EXISTING, not on a caller-supplied list of live
    /// paths. The list version silently evicted every Codex child rollout — they
    /// are nested under their parent and their paths never reached the caller —
    /// so 74 of 304 files were rescanned every single round and the hit rate sat
    /// at 62.8% instead of ~99%.
    pub fn evict_missing(&self) {
        if let Ok(mut map) = self.inner.lock() {
            map.retain(|path, _| path.exists());
        }
    }

    pub fn stats(&self) -> (u64, u64) {
        self.hits.lock().map(|h| *h).unwrap_or((0, 0))
    }
}
