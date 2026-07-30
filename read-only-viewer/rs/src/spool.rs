//! The hook spool: tool calls, as they happen.
//!
//! A transcript records what a model said; it does not reliably record what the
//! harness *did* with a tool call, and nothing on disk records a tool call the
//! moment it starts. So the harness hooks append one JSON object per event to a
//! spool file, and this module reads them back. It is the only input to the
//! store that is not derived from a transcript.
//!
//! Read-only, like everything else here: the spool is written by the hooks and
//! only ever read from.
//!
//! ## The line contract (frozen)
//!
//! One JSON object per line, in `<harness>-<YYYYMMDD>.jsonl`:
//!
//! | field | |
//! |---|---|
//! | `v` | schema version, `1` |
//! | `ts` | ISO-8601 UTC, ms |
//! | `harness` | `"claude"` \| `"codex"` |
//! | `event` | `"pre"` \| `"post"` |
//! | `session_id` | |
//! | `tool` | |
//! | `cwd`, `target`, `arg` | nullable; `arg` is capped at 500 chars |
//! | `ok` | bool or null — null on a `pre`, where the answer is not known yet |
//! | `tool_use_id` | string or null |
//!
//! ## A skipped line is counted, never swallowed
//!
//! Hooks append while this reads, so a torn final line is routine and cannot be
//! fatal. But "0 tool calls because the spool is empty" and "0 tool calls
//! because every line failed to parse" are DIFFERENT ANSWERS, and this codebase
//! has already paid for conflating them once (a swallowed error made every
//! session report no pid, which read as "nothing running"). So the count of
//! skipped lines is carried out of here and printed. Same reason `files` is an
//! Option: a missing spool directory means the hooks have not run yet, which is
//! a real empty answer, and is not the same as a directory that would not open.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The producer caps `arg` here. A longer one is CLIPPED rather than rejected:
/// an over-long argument is a producer bug, and dropping the record would lose
/// the fact that the tool ran at all.
pub const MAX_ARG: usize = 500;

pub fn spool_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("ALCOVE_SPOOL") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".local/state"));
    base.join("alcove/spool")
}

/// One spooled tool-call event, exactly as the line carries it.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub v: i64,
    pub ts: String,
    pub harness: String,
    pub event: String,
    pub session_id: String,
    pub tool: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub arg: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// FNV-1a, 128-bit. Hand-rolled rather than pulling in a hash crate, and NOT
/// `DefaultHasher`: that one's output is explicitly not stable across Rust
/// releases, and an id that changes when the compiler changes would re-insert
/// every derived row after an upgrade — the opposite of idempotent.
///
/// 128 bits, not 64, because a collision here is a silently dropped tool call.
/// `turn` already shipped that bug once (6 of 1,967 rows) and the fix is
/// cheaper than the measurement was.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    let mut h: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for b in bytes {
        h ^= *b as u128;
        h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    h
}

impl ToolCall {
    /// Clip `arg` to the contract's cap, on CHARACTER boundaries.
    ///
    /// Byte slicing would panic mid-codepoint on any non-ASCII argument, and
    /// this runs on arbitrary command lines. Clipping happens before `id()` is
    /// ever called, so the id is a hash of what is actually stored.
    fn clip(&mut self) {
        if let Some(arg) = &self.arg {
            if arg.chars().count() > MAX_ARG {
                self.arg = Some(arg.chars().take(MAX_ARG).collect());
            }
        }
    }

    /// The row's primary key: stable across runs, and one id per EVENT.
    ///
    /// `tool_use_id` names the call, not the observation — a `pre` and its
    /// `post` carry the same one. Keyed on it alone they collide, and
    /// `INSERT OR IGNORE` would keep the `pre` and throw away the `post`, which
    /// is the only line that carries `ok`. That is precisely the `turn.id`
    /// collision this store already fixed by widening the key, so the event is
    /// part of the id here. The raw `tool_use_id` is stored in its own column,
    /// so pairing a pre with its post stays a join, not a guess.
    ///
    /// With no `tool_use_id` there is nothing to pair on, so identity falls back
    /// to the content of the observation. The `d:` prefix keeps a derived id
    /// from ever colliding with a harness-issued one.
    pub fn id(&self) -> String {
        match self.tool_use_id.as_deref() {
            Some(t) if !t.is_empty() => format!("{t}:{}", self.event),
            _ => {
                // Unit separator: it cannot appear in a JSON string unescaped,
                // so no combination of field values can be made to look like a
                // different combination.
                let key = [
                    self.harness.as_str(),
                    self.session_id.as_str(),
                    self.ts.as_str(),
                    self.event.as_str(),
                    self.tool.as_str(),
                    self.arg.as_deref().unwrap_or(""),
                ]
                .join("\u{1f}");
                format!("d:{:032x}", fnv1a_128(key.as_bytes()))
            }
        }
    }
}

/// What one pass over the spool found — including what it could not read.
#[derive(Debug, Default)]
pub struct SpoolRead {
    pub dir: PathBuf,
    /// `None` means the directory is not there: the hooks have not run yet.
    /// `Some(n)` means n spool files were read. An absent spool and an empty
    /// one are the same answer; neither is the same as an unreadable one.
    pub files: Option<usize>,
    pub calls: Vec<ToolCall>,
    /// Lines that did not parse, or carried a version this build does not
    /// understand. Reported, never swallowed.
    pub skipped: u64,
    /// Directories or files that would not open. Empty is the normal case.
    pub errors: Vec<String>,
}

/// Parse one spool file's bytes.
///
/// Blank lines are not counted as skips — a trailing newline is not a lost
/// record. Everything else that fails to become a v1 `ToolCall` is.
fn parse_lines(data: &str, out: &mut Vec<ToolCall>, skipped: &mut u64) {
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ToolCall>(line) {
            // A future version may rename or re-mean a field, so a line this
            // build cannot vouch for is skipped and counted rather than read
            // optimistically.
            Ok(call) if call.v == 1 => {
                let mut call = call;
                call.clip();
                out.push(call);
            }
            _ => *skipped += 1,
        }
    }
}

/// Read every spool file. Never panics, never hides a read failure.
pub fn read_all() -> SpoolRead {
    read_dir(&spool_dir())
}

pub fn read_dir(dir: &Path) -> SpoolRead {
    let mut out = SpoolRead { dir: dir.to_path_buf(), ..Default::default() };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.errors.push(format!("{}: {e}", dir.display()));
            return out;
        }
    };
    // Sorted so a run is reproducible: readdir order is filesystem order, and
    // two runs over the same spool must produce the same rows in the same way.
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    paths.push(path);
                }
            }
            Err(e) => out.errors.push(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    let mut read = 0usize;
    for path in &paths {
        match std::fs::read(path) {
            Ok(bytes) => {
                read += 1;
                // Lossy, deliberately: a torn write can cut a UTF-8 sequence,
                // and one bad byte must cost one line, not the whole file.
                let text = String::from_utf8_lossy(&bytes);
                parse_lines(&text, &mut out.calls, &mut out.skipped);
            }
            Err(e) => out.errors.push(format!("{}: {e}", path.display())),
        }
    }
    out.files = Some(read);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Vec<ToolCall>, u64) {
        let mut calls = Vec::new();
        let mut skipped = 0;
        parse_lines(text, &mut calls, &mut skipped);
        (calls, skipped)
    }

    const LINE: &str = r#"{"v":1,"ts":"2026-07-30T10:00:00.000Z","harness":"claude","event":"pre","session_id":"s1","tool":"Bash","cwd":"/root","target":null,"arg":"ls","ok":null,"tool_use_id":"toolu_A"}"#;

    #[test]
    fn garbage_is_counted_not_fatal() {
        let (calls, skipped) = parse(&format!("{LINE}\n{{not json\n\n{LINE}\n"));
        assert_eq!(calls.len(), 2);
        assert_eq!(skipped, 1, "the blank line is not a skip; the garbage is");
    }

    #[test]
    fn unknown_version_is_a_skip() {
        let (calls, skipped) = parse(&LINE.replace(r#""v":1"#, r#""v":2"#));
        assert!(calls.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn pre_and_post_of_one_call_are_two_ids() {
        let (calls, _) = parse(&format!("{LINE}\n{}\n", LINE.replace("\"pre\"", "\"post\"")));
        assert_eq!(calls[0].id(), "toolu_A:pre");
        assert_eq!(calls[1].id(), "toolu_A:post");
    }

    #[test]
    fn a_repeated_line_has_one_id() {
        let (calls, _) = parse(&format!("{LINE}\n{LINE}\n"));
        assert_eq!(calls[0].id(), calls[1].id());
    }

    #[test]
    fn missing_tool_use_id_falls_back_to_content() {
        let bare = LINE.replace(r#","tool_use_id":"toolu_A""#, "");
        let (calls, skipped) = parse(&format!("{bare}\n{}\n", bare.replace("\"pre\"", "\"post\"")));
        assert_eq!(skipped, 0, "tool_use_id is optional, not required");
        assert!(calls[0].id().starts_with("d:"));
        assert_ne!(calls[0].id(), calls[1].id(), "event is part of the identity");
        // Same bytes in, same id out — across processes, not just within one.
        let (again, _) = parse(&bare);
        assert_eq!(calls[0].id(), again[0].id());
    }

    #[test]
    fn an_over_long_arg_is_clipped_not_dropped() {
        let long = "é".repeat(3000);
        let (calls, skipped) = parse(&LINE.replace(r#""arg":"ls""#, &format!(r#""arg":"{long}""#)));
        assert_eq!(skipped, 0);
        assert_eq!(calls[0].arg.as_deref().unwrap().chars().count(), MAX_ARG);
    }

    #[test]
    fn a_missing_spool_is_not_an_error() {
        let read = read_dir(Path::new("/nonexistent/alcove/spool"));
        assert!(read.files.is_none(), "'not there' must be distinguishable from 'empty'");
        assert!(read.errors.is_empty());
    }
}
