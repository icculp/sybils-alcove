//! Reading JSONL transcripts off disk.
//!
//! Port of `alcove/transcripts.py`. The two traps it guards are unchanged: a
//! tail read cuts the first record mid-line, and file order is not chronology
//! because compaction rewrites transcripts with repeated, out-of-order blocks.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

pub const TAIL_BYTES: u64 = 1 << 20;
pub const TAIL_LINES: usize = 4000;
const HEAD_BYTES: usize = 1 << 16;

/// Parse JSONL, skipping records that do not parse or are not objects.
///
/// A malformed line is skipped rather than fatal: transcripts are appended to
/// live, so the final line is routinely a partial write.
fn parse(data: &[u8], limit: Option<usize>) -> Vec<Value> {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.lines().collect();
    let start = match limit {
        Some(n) if lines.len() > n => lines.len() - n,
        _ => 0,
    };
    lines[start..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.is_object())
        .collect()
}

/// Parsed objects from the TAIL of a transcript.
pub fn tail_events(path: &Path) -> Vec<Value> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let mut buf = Vec::new();
    if size <= TAIL_BYTES {
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
    } else {
        if file.seek(SeekFrom::End(-(TAIL_BYTES as i64))).is_err() {
            return Vec::new();
        }
        if file.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        // First line is almost certainly cut mid-record.
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }
    parse(&buf, Some(TAIL_LINES))
}

/// Parsed objects from the START of a transcript.
///
/// Identity lives in the first record — Codex writes `session_meta` as line
/// one. A tail-only read silently loses identity on any file bigger than the
/// tail window, which reads as "this session does not exist".
pub fn head_events(path: &Path) -> Vec<Value> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut buf = vec![0u8; HEAD_BYTES];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    buf.truncate(n);
    // Drop a trailing partial line so it does not fail to parse.
    if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
        buf.truncate(pos);
    }
    parse(&buf, None)
}

/// Dedupe by uuid (first wins) and sort by a string timestamp field.
///
/// Must stay a STABLE sort on the raw string: the Python implementation sorts
/// `str(e.get(key) or "")` with a stable sort, and the equivalence gate compares
/// the resulting order.
pub fn chronological(events: Vec<Value>, key: &str) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique: Vec<Value> = Vec::with_capacity(events.len());
    for event in events {
        if let Some(uuid) = event.get("uuid").and_then(Value::as_str) {
            if !seen.insert(uuid.to_string()) {
                continue;
            }
        }
        unique.push(event);
    }
    unique.sort_by(|a, b| {
        let ka = a.get(key).and_then(Value::as_str).unwrap_or("");
        let kb = b.get(key).and_then(Value::as_str).unwrap_or("");
        ka.cmp(kb)
    });
    unique
}

pub fn file_size(path: &Path) -> u64 {
    path.metadata().map(|m| m.len()).unwrap_or(0)
}
