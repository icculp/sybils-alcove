//! Codex's own sqlite, read as optional enrichment. Port of
//! `alcove/sources/codex_state.py`.
//!
//! Measured against this corpus, what it adds over the transcripts is narrow —
//! 19 sessions, 71 subagents:
//!
//!     subagent status (open/closed)   0 -> 71   the whole point
//!     git_branch                      0 ->  3
//!     reasoning_effort               16 -> 18
//!     agent_nickname                 71 -> 71   already in the rollout head
//!     agent_role                     56 -> 56   already in the rollout head
//!     parent edges                   71 -> 71   already in the rollout head
//!
//! So the reason to read it is `status`: Codex writes no completion record into
//! a transcript, so an ended subagent was indistinguishable from an idle one.
//!
//! THIS IS A PRIVATE FILE WITH NO COMPATIBILITY PROMISE. The trailing number is
//! a schema generation — `state_5` will become `state_6`. Every function returns
//! empty on any failure, and the caller treats a result as a bonus.
//!
//! Reading strategy, forced by two facts:
//!
//! * The database is WAL. A read-only connection to a WAL database must create a
//!   `-shm`, which needs write permission on the DIRECTORY — which the server
//!   does not have. `immutable=1` avoids the shm, but promising sqlite a file
//!   will not change while Codex is writing it invites torn reads.
//! * So the main file is COPIED and the copy opened immutable, where the promise
//!   is true. A WAL main file is valid as of its last checkpoint, so the snapshot
//!   may lag by the un-checkpointed tail. Fine for nicknames and roles, which
//!   never change after a thread starts.
//!
//! Deliberately NOT read: `threads.tokens_used`. One thread reports 848,292,502 —
//! a cumulative counter re-added per turn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};

#[derive(Default, Clone)]
pub struct Thread {
    pub nickname: String,
    pub role: String,
    pub model: String,
    pub effort: String,
    pub branch: String,
    pub archived: bool,
}

#[derive(Default, Clone)]
pub struct Edge {
    pub parent: String,
    pub status: String,
}

#[derive(Default, Clone)]
pub struct State {
    pub threads: HashMap<String, Thread>,
    pub edges: HashMap<String, Edge>,
}

struct Cache {
    stamp: Option<(u64, i64)>,
    data: Option<State>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { stamp: None, data: None });

fn codex_home() -> PathBuf {
    if let Ok(v) = std::env::var("ALCOVE_CODEX_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into())).join(".codex")
}

/// The newest `state_<N>.sqlite`, or an explicit override.
///
/// Taking the HIGHEST generation means a bump to `state_6` is picked up with no
/// code change.
pub fn state_db() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ALCOVE_CODEX_STATE_DB") {
        if !explicit.is_empty() {
            let p = PathBuf::from(explicit);
            return p.is_file().then_some(p);
        }
    }
    let home = codex_home();
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(home).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("state_") else { continue };
        let Some(gen) = rest.strip_suffix(".sqlite") else { continue };
        let Ok(gen) = gen.parse::<u64>() else { continue };
        if path.is_file() && best.as_ref().map(|(g, _)| gen > *g).unwrap_or(true) {
            best = Some((gen, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Drop snapshots left by processes that are gone.
///
/// The name carries the pid so two servers never share one, which means a
/// restart abandons its copy.
fn sweep(tmp: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if !name.starts_with("alcove-codex-state-") || path == keep {
            continue;
        }
        let Some(pid) = name
            .trim_start_matches("alcove-codex-state-")
            .trim_end_matches(".sqlite")
            .parse::<i32>()
            .ok()
        else {
            continue;
        };
        if crate::process::pid_alive(pid as i64) {
            continue; // owner still running
        }
        let _ = std::fs::remove_file(path);
    }
}

/// A private copy of the database, safe to open with `immutable=1`.
fn snapshot(source: &std::path::Path) -> Option<PathBuf> {
    let meta = source.metadata().ok()?;
    let stamp = (meta.len(), meta.modified().ok().and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_nanos() as i64)
    })?);
    let tmp = std::env::temp_dir();
    let target = tmp.join(format!("alcove-codex-state-{}.sqlite", std::process::id()));
    {
        let cache = CACHE.lock().ok()?;
        if cache.stamp == Some(stamp) && target.is_file() && cache.data.is_some() {
            return Some(target);
        }
    }
    sweep(&tmp, &target);
    std::fs::copy(source, &target).ok()?;
    if let Ok(mut cache) = CACHE.lock() {
        cache.stamp = Some(stamp);
        cache.data = None;
    }
    Some(target)
}

/// Select only the columns that actually exist, so a renamed column in a future
/// generation costs that one field rather than the whole table.
fn present(conn: &Connection, table: &str, wanted: &[&str]) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return Vec::new();
    };
    let have: Vec<String> = rows.flatten().collect();
    wanted.iter().filter(|c| have.iter().any(|h| h == *c)).map(|c| c.to_string()).collect()
}

/// Empty on any failure — a missing, moved, or reshaped database is a normal
/// outcome, not an error worth surfacing as one.
pub fn read() -> State {
    let Some(source) = state_db() else { return State::default() };
    let Some(snap) = snapshot(&source) else { return State::default() };
    if let Ok(cache) = CACHE.lock() {
        if let Some(data) = cache.data.clone() {
            return data;
        }
    }
    let uri = format!("file:{}?mode=ro&immutable=1", snap.display());
    let Ok(conn) = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return State::default();
    };

    let mut state = State::default();
    let cols = present(
        &conn,
        "threads",
        &["id", "agent_nickname", "agent_role", "model", "reasoning_effort", "git_branch",
          "archived"],
    );
    if cols.iter().any(|c| c == "id") {
        let sql = format!("SELECT {} FROM threads", cols.join(","));
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let idx = |name: &str| cols.iter().position(|c| c == name);
            let _ = stmt.query_map([], |row| {
                let get = |name: &str| -> String {
                    idx(name)
                        .and_then(|i| row.get::<_, Option<String>>(i).ok().flatten())
                        .unwrap_or_default()
                };
                let id = get("id");
                if !id.is_empty() {
                    state.threads.insert(
                        id,
                        Thread {
                            nickname: get("agent_nickname"),
                            role: get("agent_role"),
                            model: get("model"),
                            effort: get("reasoning_effort"),
                            branch: get("git_branch"),
                            archived: idx("archived")
                                .and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten())
                                .unwrap_or(0)
                                != 0,
                        },
                    );
                }
                Ok(())
            }).map(|rows| rows.count());
        }
    }

    let cols = present(
        &conn,
        "thread_spawn_edges",
        &["parent_thread_id", "child_thread_id", "status"],
    );
    if cols.iter().any(|c| c == "child_thread_id") {
        let sql = format!("SELECT {} FROM thread_spawn_edges", cols.join(","));
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let idx = |name: &str| cols.iter().position(|c| c == name);
            let _ = stmt.query_map([], |row| {
                let get = |name: &str| -> String {
                    idx(name)
                        .and_then(|i| row.get::<_, Option<String>>(i).ok().flatten())
                        .unwrap_or_default()
                };
                let child = get("child_thread_id");
                if !child.is_empty() {
                    state.edges.insert(
                        child,
                        Edge { parent: get("parent_thread_id"), status: get("status") },
                    );
                }
                Ok(())
            }).map(|rows| rows.count());
        }
    }

    if let Ok(mut cache) = CACHE.lock() {
        cache.data = Some(state.clone());
    }
    state
}
