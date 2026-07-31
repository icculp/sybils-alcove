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
//! | `event` | `"pre"` \| `"post"` \| `"stop"` \| `"subagent_stop"` |
//! | `session_id` | the session's own id — the PARENT's on a child's line |
//! | `tool` | `""` on the stop family, never null |
//! | `cwd`, `target`, `arg` | nullable; `arg` is capped at 500 chars |
//! | `ok` | bool or null — null on a `pre`, where the answer is not known yet |
//! | `tool_use_id` | string or null |
//! | `agent_id`, `agent_type` | nullable, additive; null means "the session itself" |
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
    /// Which agent acted: a child's `agent_id`, or `None` for the session's own
    /// turn. ADDITIVE and still `v: 1` — a line written before the hook learned
    /// to send it parses with `None`, which is what such a line means to a
    /// reader that cannot tell parent from child anyway. Bumping the version
    /// would have made every deployed reader skip every new line.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// `Explore`, `general-purpose`, … Only used to label a child whose parent
    /// spawn record is missing; the transcript is the better source when it
    /// exists.
    #[serde(default)]
    pub agent_type: Option<String>,
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

/// Civil date from a unix timestamp, UTC (Howard Hinnant's algorithm).
///
/// Spool files are named on the UTC date, so picking "today and yesterday"
/// is date arithmetic, not a mtime comparison: a file that has not been
/// appended to since midnight still holds this morning's events.
pub fn utc_ymd(secs: i64) -> (i64, i64, i64) {
    let days = secs.div_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One spool file's parse, cached on `(size, mtime)`.
///
/// `Arc` because this is cloned out of the cache on every collect and a busy day
/// is thousands of events: the clone must be a refcount bump, not a deep copy.
#[derive(Clone)]
pub struct FileParse {
    pub calls: std::sync::Arc<Vec<ToolCall>>,
    pub skipped: u64,
    /// `Some(reason)` when the file would not open. Never conflated with an empty
    /// parse, which is a real answer.
    pub unreadable: Option<String>,
}

pub type SpoolCache = crate::cache::ScanCache<FileParse>;

impl FileParse {
    fn of(path: &Path) -> Self {
        let mut calls = Vec::new();
        let mut skipped = 0u64;
        let mut unreadable = None;
        match std::fs::read(path) {
            Ok(bytes) => {
                // Lossy: a torn write can cut a UTF-8 sequence, and one bad byte
                // must cost one line rather than the whole file.
                let text = String::from_utf8_lossy(&bytes);
                parse_lines(&text, &mut calls, &mut skipped);
            }
            Err(e) => unreadable = Some(e.to_string()),
        }
        Self { calls: std::sync::Arc::new(calls), skipped, unreadable }
    }
}

/// The `YYYYMMDD` stamps a spool filename may carry, for `days` back including
/// today. `days = 2` is today and yesterday.
pub fn recent_stamps(now_secs: i64, days: i64) -> Vec<String> {
    (0..days.max(1))
        .map(|back| {
            let (y, m, d) = utc_ymd(now_secs - back * 86400);
            format!("{y:04}{m:02}{d:02}")
        })
        .collect()
}

/// Read only the last `days` UTC days of spool, caching each file's parse on
/// `(size, mtime)`.
///
/// Liveness only ever asks about *now*, and the whole spool grows without bound
/// — a month of it would be re-parsed on every collect to answer a question
/// about the last few minutes. Two days is the window because a session that
/// started at 23:50 UTC has its stop in tomorrow's file, and its start in
/// yesterday's.
///
/// The cache is a real win only between bursts: the current day's file is
/// appended to constantly, so it re-parses whenever it grows. That is bounded by
/// one day of hook lines (~65 KB after five busy hours), which is why re-reading
/// the whole file is preferred over byte-offset resume — the fold below is a
/// pure per-line accumulation and could be made resumable, but has not needed to
/// be.
pub fn read_window(dir: &Path, days: i64, cache: &SpoolCache) -> SpoolRead {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stamps = recent_stamps(now, days);
    let mut out = SpoolRead { dir: dir.to_path_buf(), ..Default::default() };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.errors.push(format!("{}: {e}", dir.display()));
            return out;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => {
                let path = e.path();
                let name = path.file_name().map(|n| n.to_string_lossy().to_string());
                let keep = name
                    .as_deref()
                    .map(|n| n.ends_with(".jsonl") && stamps.iter().any(|s| n.contains(s.as_str())))
                    .unwrap_or(false);
                if keep {
                    paths.push(path);
                }
            }
            Err(e) => out.errors.push(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    let mut read = 0usize;
    for path in &paths {
        // The skip count is part of the cached parse, not recomputed on a hit:
        // "0 skipped because the file is clean" and "0 skipped because we did not
        // look" are different answers, and only one of them is true here.
        let parsed = cache.get_or_scan(path, || FileParse::of(path));
        out.calls.extend(parsed.calls.iter().cloned());
        out.skipped += parsed.skipped;
        if let Some(err) = &parsed.unreadable {
            out.errors.push(format!("{}: {err}", path.display()));
        }
        read += 1;
    }
    out.files = Some(read);
    out
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

    /// Verbatim from the live spool. The stop family is the whole point of the
    /// fold, and `tool: ""` / `tool_use_id: null` on it is exactly the shape that
    /// a stricter type would have silently dropped.
    const STOP_LINE: &str = r#"{"v":1,"ts":"2026-07-30T14:38:03.588Z","harness":"claude","event":"subagent_stop","session_id":"b3d712dd-e4b8-459b-bb92-b717a5072968","tool":"","cwd":"/root","target":"ab5bd1b2bfa719c8e","arg":"/root/.claude/projects/-root/b3d712dd-e4b8-459b-bb92-b717a5072968/subagents/agent-ab5bd1b2bfa719c8e.jsonl","ok":null,"tool_use_id":null}"#;

    #[test]
    fn stop_family_lines_are_valid_not_malformed() {
        let (calls, skipped) = parse(&format!(
            "{STOP_LINE}\n{}\n",
            STOP_LINE.replace("\"subagent_stop\"", "\"stop\"")
        ));
        assert_eq!(skipped, 0, "a stop line is a v1 line, not garbage");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].event, "subagent_stop");
        assert_eq!(calls[0].tool, "");
        assert_eq!(calls[0].target.as_deref(), Some("ab5bd1b2bfa719c8e"));
        // Two events at the same instant with no tool_use_id must not collapse.
        assert_ne!(calls[0].id(), calls[1].id());
    }

    #[test]
    fn agent_fields_are_optional() {
        // Old line: no agent_id at all.
        let (old, skipped) = parse(LINE);
        assert_eq!(skipped, 0);
        assert!(old[0].agent_id.is_none(), "absent means 'the session itself'");
        let child = LINE.replace(
            r#""tool_use_id":"toolu_A""#,
            r#""tool_use_id":"toolu_A","agent_id":"ac76b5442617a9edf","agent_type":"Explore""#,
        );
        let (new, skipped) = parse(&child);
        assert_eq!(skipped, 0, "an added field does not make a line a different version");
        assert_eq!(new[0].agent_id.as_deref(), Some("ac76b5442617a9edf"));
        assert_eq!(new[0].agent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn the_window_reads_today_and_yesterday_only() {
        // 2026-07-30T12:00:00Z
        let now = 1785412800i64;
        let stamps = recent_stamps(now, 2);
        assert_eq!(stamps, vec!["20260730".to_string(), "20260729".to_string()]);
        let dir = std::env::temp_dir().join(format!("alcove-spool-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for stamp in ["20260730", "20260101"] {
            std::fs::write(dir.join(format!("claude-{stamp}.jsonl")), format!("{LINE}\n")).unwrap();
        }
        let cache = SpoolCache::default();
        let read = read_window(&dir, 2, &cache);
        // Only the file whose NAME is in the window; the January one is skipped
        // without being opened, which is the point.
        assert_eq!(read.files, Some(1));
        assert_eq!(read.calls.len(), 1);
        let again = read_window(&dir, 2, &cache);
        assert_eq!(again.calls.len(), 1);
        assert_eq!(cache.stats().0, 1, "an unchanged file is served from the cache");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_spool_is_not_an_error() {
        let read = read_dir(Path::new("/nonexistent/alcove/spool"));
        assert!(read.files.is_none(), "'not there' must be distinguishable from 'empty'");
        assert!(read.errors.is_empty());
    }
}
