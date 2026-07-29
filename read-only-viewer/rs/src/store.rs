//! Local SQLite store of derived facts. Port of `alcove/store.py`.
//!
//! Every number in the live view is computed by tailing the last ~1 MB of each
//! transcript, so totals are recent-window and history beyond it is
//! unrecoverable. An activity chart over days cannot come from a sliding window.
//!
//! The design rests on one property: **every fact ingested here has a natural
//! id, so ingestion is idempotent.** Claude assistant messages carry
//! `message.id`; Codex assistant messages carry `payload.id`; compactions and
//! selections key on (session, timestamp). With `INSERT OR IGNORE` on those
//! keys, re-scanning overlapping windows is free and harmless — which is what
//! makes incremental backfill, crash recovery, and a poll loop all the same
//! operation.
//!
//! Scope note: this module writes only its own derived cache. It never writes a
//! transcript, agent state, or anything an agent reads back.

use std::path::PathBuf;

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

pub const SCHEMA: &str = r#"
-- The key is (id, thread_id), NOT id alone. A spawned Codex agent inherits the
-- parent's replayed history, so the same assistant message id appears in the
-- parent AND in every child, all stored under the parent's session_id. Keyed on
-- id alone they collide and INSERT OR IGNORE keeps whichever lands first:
-- measured at 6 rows silently dropped of 1,967, worst id appearing 7 times
-- across 7 threads, with the survivor depending on iteration order.
-- `thread_id` is the thread that actually produced the turn — the session for a
-- main thread, the subagent's own id for a child.
CREATE TABLE IF NOT EXISTS turn (
  id          TEXT NOT NULL,
  thread_id   TEXT NOT NULL DEFAULT '',
  session_id  TEXT NOT NULL,
  harness     TEXT NOT NULL,
  ts          TEXT,
  model       TEXT,
  input       INTEGER,
  output      INTEGER,
  cache_read  INTEGER,
  cache_write INTEGER,
  is_subagent INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id, thread_id)
);
CREATE INDEX IF NOT EXISTS turn_session_ts ON turn(session_id, ts);
CREATE INDEX IF NOT EXISTS turn_ts         ON turn(ts);

CREATE TABLE IF NOT EXISTS session (
  id         TEXT PRIMARY KEY,
  harness    TEXT,
  project    TEXT,
  cwd        TEXT,
  branch     TEXT,
  first_seen TEXT,
  last_seen  TEXT
);

CREATE TABLE IF NOT EXISTS selection (
  session_id TEXT, ts TEXT, model TEXT, requested TEXT,
  PRIMARY KEY (session_id, ts, model)
);

CREATE TABLE IF NOT EXISTS compaction (
  session_id TEXT, ts TEXT, trigger TEXT, pre_tokens INTEGER,
  PRIMARY KEY (session_id, ts)
);

CREATE TABLE IF NOT EXISTS subagent (
  id TEXT PRIMARY KEY, session_id TEXT, harness TEXT, model TEXT,
  role TEXT, status TEXT, first_seen TEXT, last_seen TEXT
);

-- Process state cannot be recovered later: a transcript never records that a
-- session was alive at 3am. So it is sampled, not derived.
CREATE TABLE IF NOT EXISTS observation (
  ts TEXT, session_id TEXT, state TEXT, pids TEXT,
  PRIMARY KEY (ts, session_id)
);
"#;

#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn db_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("ALCOVE_DB") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".local/state"));
    base.join("alcove/alcove.db")
}

/// Open the store. Read-only unless a writer explicitly asks otherwise.
///
/// The server must never open this read-write. It runs under a unit that sets
/// `ProtectHome=read-only`, because serving a page needs no write access — and a
/// read-write open there fails with "unable to open database file".
pub fn connect(write: bool) -> Result<Connection, Unavailable> {
    let target = db_path();
    if write {
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&target).map_err(|e| Unavailable(e.to_string()))?;
        // NOTE: SQL below uses raw strings, never backslash line-continuation.
        // Rust's `\` strips the newline AND the next line's leading whitespace, so
        // "DO UPDATE SET\\n  last_seen" becomes "SETlast_seen" — invalid SQL that
        // silently produced an empty `session` table until the errors stopped
        // being swallowed.
        // NOT WAL, deliberately. A read-only connection to a WAL database has to
        // create a `-shm` file, which needs write permission on the DIRECTORY —
        // so WAL silently requires every reader to be a writer. Rollback journal
        // keeps "the server needs no write access anywhere" true.
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        // The pragma reports failure by RETURNING the mode it left in place
        // rather than raising, so read it back.
        let mode: String = conn
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .unwrap_or_default();
        if mode.to_lowercase() != "delete" {
            eprintln!(
                "warning: store journal_mode is {mode:?}, not 'delete'. A read-only \
                 reader cannot open a WAL database. Re-run with nothing else attached."
            );
        }
        conn.execute_batch(SCHEMA).map_err(|e| Unavailable(e.to_string()))?;
        return Ok(conn);
    }
    if !target.exists() {
        return Err(Unavailable(format!("no store at {}", target.display())));
    }
    // Read-only, and a reader can never create the file by accident.
    Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Unavailable(format!("{e} (if the store predates the rollback-journal \
                                          change, one `--ingest-only` converts it)")))
}

#[derive(Default, Debug)]
pub struct Counts {
    pub turn_new: i64,
    pub selection_new: i64,
    pub compaction_new: i64,
    pub observation_new: i64,
    pub session_seen: i64,
    pub subagent_seen: i64,
}

fn turn_params(
    row: &Value,
    sid: &str,
    thread_id: &str,
    harness: &str,
    is_sub: i64,
) -> Option<Vec<Value>> {
    let id = row.get("id")?.as_str()?.to_string();
    Some(vec![
        json!(id),
        json!(thread_id),
        json!(sid),
        json!(harness),
        row.get("ts").cloned().unwrap_or(Value::Null),
        row.get("model").cloned().unwrap_or(Value::Null),
        row.get("input").cloned().unwrap_or(Value::Null),
        row.get("output").cloned().unwrap_or(Value::Null),
        row.get("cache_read").cloned().unwrap_or(Value::Null),
        row.get("cache_write").cloned().unwrap_or(Value::Null),
        json!(is_sub),
    ])
}

fn bind(v: &Value) -> Box<dyn rusqlite::ToSql> {
    match v {
        Value::Null => Box::new(Option::<String>::None),
        Value::Bool(b) => Box::new(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Box::new(i)
            } else {
                Box::new(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => Box::new(s.clone()),
        other => Box::new(other.to_string()),
    }
}

/// Write a snapshot's facts. Safe to call repeatedly on overlapping data.
///
/// Immutable facts report rows genuinely NEW; session and subagent are upserts,
/// so their change count includes updates and is reported as "seen" rather than
/// passed off as new.
pub fn ingest(conn: &mut Connection, snapshot: &Value) -> Result<Counts, Unavailable> {
    let mut counts = Counts::default();
    let now = snapshot.get("generated_at").and_then(Value::as_str).unwrap_or("").to_string();
    let empty = Vec::new();
    let sessions = snapshot.get("sessions").and_then(Value::as_array).unwrap_or(&empty);

    let tx = conn.transaction().map_err(|e| Unavailable(e.to_string()))?;
    {
        let mut before = tx.total_changes() as i64;
        {
            let mut stmt = tx
                .prepare(
                    r#"INSERT OR IGNORE INTO turn
                         (id, thread_id, session_id, harness, ts, model, input,
                          output, cache_read, cache_write, is_subagent)
                       VALUES (?,?,?,?,?,?,?,?,?,?,?)"#,
                )
                .map_err(|e| Unavailable(e.to_string()))?;
            for session in sessions {
                let sid = session.get("session_id").and_then(Value::as_str).unwrap_or("");
                let harness = session.get("harness").and_then(Value::as_str).unwrap_or("");
                let own = session.get("turn_rows").and_then(Value::as_array).cloned()
                    .unwrap_or_default();
                for row in &own {
                    // A main thread owns its own turns.
                    if let Some(p) = turn_params(row, sid, sid, harness, 0) {
                        let boxed: Vec<Box<dyn rusqlite::ToSql>> = p.iter().map(bind).collect();
                        let refs: Vec<&dyn rusqlite::ToSql> =
                            boxed.iter().map(|b| b.as_ref()).collect();
                        stmt.execute(refs.as_slice())
                            .map_err(|e| Unavailable(format!("turn insert: {e}")))?;
                    }
                }
                for sub in
                    session.get("subagents").and_then(Value::as_array).unwrap_or(&empty)
                {
                    let child = sub.get("id").and_then(Value::as_str).unwrap_or("");
                    for row in sub.get("turn_rows").and_then(Value::as_array).unwrap_or(&empty) {
                        // The OWNING thread is the subagent, not its parent.
                        if let Some(p) = turn_params(row, sid, child, harness, 1) {
                            let boxed: Vec<Box<dyn rusqlite::ToSql>> =
                                p.iter().map(bind).collect();
                            let refs: Vec<&dyn rusqlite::ToSql> =
                                boxed.iter().map(|b| b.as_ref()).collect();
                            stmt.execute(refs.as_slice())
                            .map_err(|e| Unavailable(format!("turn insert: {e}")))?;
                        }
                    }
                }
            }
        }
        counts.turn_new = tx.total_changes() as i64 - before;

        for session in sessions {
            let sid = session.get("session_id").and_then(Value::as_str).unwrap_or("");
            let harness = session.get("harness").and_then(Value::as_str).unwrap_or("");
            let s = |k: &str| session.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let last = {
                let v = s("last_ts");
                if v.is_empty() { now.clone() } else { v }
            };

            // First seen is kept; last seen advances. Never regress first_seen.
            before = tx.total_changes() as i64;
            tx.execute(
                r#"INSERT INTO session
                     (id, harness, project, cwd, branch, first_seen, last_seen)
                   VALUES (?,?,?,?,?,?,?)
                   ON CONFLICT(id) DO UPDATE SET
                     last_seen = excluded.last_seen,
                     project   = excluded.project,
                     cwd       = excluded.cwd,
                     branch    = excluded.branch"#,
                params![sid, harness, s("project"), s("cwd"), s("branch"), last, last],
            )
            .map_err(|e| Unavailable(format!("session upsert: {e}")))?;
            counts.session_seen += tx.total_changes() as i64 - before;

            before = tx.total_changes() as i64;
            for sel in session.get("selections").and_then(Value::as_array).unwrap_or(&empty) {
                let g = |k: &str| sel.get(k).and_then(Value::as_str).unwrap_or("");
                tx.execute(
                    r#"INSERT OR IGNORE INTO selection (session_id, ts, model, requested)
                       VALUES (?,?,?,?)"#,
                    params![sid, g("at"), g("model"), g("requested")],
                )
                .map_err(|e| Unavailable(format!("selection insert: {e}")))?;
            }
            counts.selection_new += tx.total_changes() as i64 - before;

            before = tx.total_changes() as i64;
            for c in session.get("compactions").and_then(Value::as_array).unwrap_or(&empty) {
                let g = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
                let pre = c.get("pre_tokens").and_then(Value::as_i64);
                tx.execute(
                    r#"INSERT OR IGNORE INTO compaction (session_id, ts, trigger, pre_tokens)
                       VALUES (?,?,?,?)"#,
                    params![sid, g("at"), g("trigger"), pre],
                )
                .map_err(|e| Unavailable(format!("compaction insert: {e}")))?;
            }
            counts.compaction_new += tx.total_changes() as i64 - before;

            before = tx.total_changes() as i64;
            for sub in session.get("subagents").and_then(Value::as_array).unwrap_or(&empty) {
                let g = |k: &str| sub.get(k).and_then(Value::as_str).unwrap_or("");
                tx.execute(
                    r#"INSERT INTO subagent
                         (id, session_id, harness, model, role, status, first_seen, last_seen)
                       VALUES (?,?,?,?,?,?,?,?)
                       ON CONFLICT(id) DO UPDATE SET
                         model     = excluded.model,
                         status    = excluded.status,
                         last_seen = excluded.last_seen"#,
                    params![g("id"), sid, harness, g("model"), g("role"), g("status"), now, now],
                )
                .map_err(|e| Unavailable(format!("subagent upsert: {e}")))?;
            }
            counts.subagent_seen += tx.total_changes() as i64 - before;

            // One observation per snapshot per session: a sample of something
            // unrecoverable, so it is inserted, not deduped away.
            before = tx.total_changes() as i64;
            let pids = session
                .get("pids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter().filter_map(Value::as_i64).map(|p| p.to_string()).collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            tx.execute(
                r#"INSERT OR IGNORE INTO observation (ts, session_id, state, pids)
                   VALUES (?,?,?,?)"#,
                params![now, sid, s("state"), pids],
            )
            .map_err(|e| Unavailable(format!("observation insert: {e}")))?;
            counts.observation_new += tx.total_changes() as i64 - before;
        }
    }
    tx.commit().map_err(|e| Unavailable(e.to_string()))?;
    Ok(counts)
}

/// Turns and output tokens per day per harness — the activity chart.
///
/// Reads the store, never the transcripts, so it is not bounded by the tail
/// window the live view is stuck with.
pub fn daily_activity(conn: &Connection, days: i64) -> Result<Vec<Value>, Unavailable> {
    let mut stmt = conn
        .prepare(
            r#"SELECT substr(ts,1,10) AS day, harness, COUNT(*) AS turns,
                      COALESCE(SUM(output),0) AS output,
                      COUNT(DISTINCT session_id) AS sessions
                 FROM turn WHERE ts >= date('now', ?1)
                GROUP BY day, harness ORDER BY day"#,
        )
        .map_err(|e| Unavailable(e.to_string()))?;
    let rows = stmt
        .query_map(params![format!("-{days} days")], |r| {
            Ok(json!({
                "day": r.get::<_, String>(0)?,
                "harness": r.get::<_, String>(1)?,
                "turns": r.get::<_, i64>(2)?,
                "output": r.get::<_, i64>(3)?,
                "sessions": r.get::<_, i64>(4)?,
            }))
        })
        .map_err(|e| Unavailable(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| Unavailable(e.to_string()))
}

/// Lifetime figures, which the live view cannot compute.
pub fn totals(conn: &Connection) -> Result<Value, Unavailable> {
    conn.query_row(
        r#"SELECT COUNT(*), COALESCE(SUM(output),0), COUNT(DISTINCT session_id),
                  MIN(ts), MAX(ts) FROM turn"#,
        [],
        |r| {
            Ok(json!({
                "turns": r.get::<_, i64>(0)?,
                "output": r.get::<_, i64>(1)?,
                "sessions": r.get::<_, i64>(2)?,
                "first_ts": r.get::<_, Option<String>>(3)?,
                "last_ts": r.get::<_, Option<String>>(4)?,
            }))
        },
    )
    .map_err(|e| Unavailable(e.to_string()))
}

/// Drop old process samples.
///
/// One observation per session per snapshot: at a fast poll that is ~1.3M rows a
/// day, which is a footgun rather than a history.
pub fn prune_observations(conn: &Connection, days: i64) -> Result<usize, Unavailable> {
    conn.execute("DELETE FROM observation WHERE ts < date('now', ?1)", params![format!("-{days} days")])
        .map_err(|e| Unavailable(e.to_string()))
}
