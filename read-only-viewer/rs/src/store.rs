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
use std::time::Duration;

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

-- Tool calls, from the hook spool rather than a transcript. Harness-neutral:
-- one row per spooled EVENT, mirroring the frozen line contract in spool.rs.
--
-- `id` is the spool line's own identity (see ToolCall::id): the harness's
-- tool_use_id qualified by the event, or a hash of the observation when there is
-- no tool_use_id. It is stable across runs and across rebuilds, which is what
-- makes re-reading a spool file free.
--
-- `tool_use_id` stays its own column even though it is inside `id`. It is the
-- only thing that pairs a `pre` with its `post`, and a later view wants that
-- pairing to be a join rather than string surgery on the key.
CREATE TABLE IF NOT EXISTS tool_call (
  id          TEXT PRIMARY KEY,
  harness     TEXT NOT NULL,
  session_id  TEXT NOT NULL,
  ts          TEXT,
  event       TEXT NOT NULL,
  tool        TEXT NOT NULL,
  cwd         TEXT,
  target      TEXT,
  arg         TEXT,
  ok          INTEGER,
  tool_use_id TEXT
);
CREATE INDEX IF NOT EXISTS tool_call_session_ts ON tool_call(session_id, ts);
CREATE INDEX IF NOT EXISTS tool_call_ts         ON tool_call(ts);
CREATE INDEX IF NOT EXISTS tool_call_use_id     ON tool_call(tool_use_id);

-- Process state cannot be recovered later: a transcript never records that a
-- session was alive at 3am. So it is sampled, not derived.
CREATE TABLE IF NOT EXISTS observation (
  ts TEXT, session_id TEXT, state TEXT, pids TEXT,
  PRIMARY KEY (ts, session_id)
);
"#;

/// How long either connection waits on a locked database before giving up.
///
/// A reader that lands mid-write must wait, not fail: ingest holds the write
/// lock for a fraction of a second, and `/api/activity` reporting "store
/// unavailable" because that fraction overlapped a poll is a lie about the
/// store.
///
/// **This is a pin, not a fix.** rusqlite already calls
/// `sqlite3_busy_timeout(db, 5000)` inside every `open` (0.40.1,
/// `inner_connection.rs`), so today's behaviour is unchanged — measured against a
/// 3 s exclusive lock, `/api/activity` waits 2.96 s and then serves real rows
/// both with and without this line. It is set here so the 5 s is ours rather
/// than a dependency's default, and the same measurement at 0 ms shows what that
/// default is holding up: the read open fails immediately and `/api/activity`
/// answers in 7 ms with empty rows and `"unavailable": "database is locked"` —
/// an HTTP 200 that reads as "no activity", which is the failure mode this file
/// already refuses elsewhere. Ingest fails outright the same way.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

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
        conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| Unavailable(e.to_string()))?;
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
        migrate_turn_thread_id(&conn)?;
        return Ok(conn);
    }
    if !target.exists() {
        return Err(Unavailable(format!("no store at {}", target.display())));
    }
    // Read-only, and a reader can never create the file by accident.
    let conn = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Unavailable(format!("{e} (if the store predates the rollback-journal \
                                          change, one `--ingest-only` converts it)")))?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| Unavailable(e.to_string()))?;
    Ok(conn)
}

/// Add `turn.thread_id` to a store created before the column existed.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing to a table that is already there,
/// so a store written by an older build keeps its original `turn` shape and
/// every snapshot ingest against it fails outright with "no column named
/// thread_id". The column has to be added explicitly.
///
/// Idempotent by construction: a second connect sees the column and returns
/// without touching anything.
///
/// This deliberately does NOT restore the composite key. SQLite cannot alter a
/// PRIMARY KEY, so a migrated store keeps `PRIMARY KEY (id)` and goes on
/// collapsing the ~6 cross-thread Codex collisions the composite key exists to
/// keep. Recovering them means rebuilding the table and re-ingesting, which
/// trades data for correctness and is an operator decision — see
/// `read-only-viewer/PORT.md`. Ingest working again is the point here.
fn migrate_turn_thread_id(conn: &Connection) -> Result<(), Unavailable> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(turn)")
        .map_err(|e| Unavailable(format!("turn table_info: {e}")))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| Unavailable(format!("turn table_info: {e}")))?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| Unavailable(format!("turn table_info: {e}")))?;
    if cols.iter().any(|c| c == "thread_id") {
        return Ok(());
    }
    conn.execute_batch(r#"ALTER TABLE turn ADD COLUMN thread_id TEXT NOT NULL DEFAULT ''"#)
        .map_err(|e| Unavailable(format!("turn thread_id migration: {e}")))?;
    eprintln!(
        "store: migrated `turn` — added thread_id. This db predates the (id, thread_id) \
         key, so its PRIMARY KEY stays (id); see PORT.md."
    );
    Ok(())
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

/// Write spooled tool calls. Returns rows genuinely NEW.
///
/// `INSERT OR IGNORE` on the line's own id, like every other immutable fact
/// here: a spool file re-read (a poll, a crash, a day's file read twice) costs
/// nothing and changes nothing.
pub fn ingest_tool_calls(
    conn: &mut Connection,
    calls: &[crate::spool::ToolCall],
) -> Result<i64, Unavailable> {
    let tx = conn.transaction().map_err(|e| Unavailable(e.to_string()))?;
    let before = tx.total_changes() as i64;
    {
        let mut stmt = tx
            .prepare(
                r#"INSERT OR IGNORE INTO tool_call
                     (id, harness, session_id, ts, event, tool, cwd, target, arg,
                      ok, tool_use_id)
                   VALUES (?,?,?,?,?,?,?,?,?,?,?)"#,
            )
            .map_err(|e| Unavailable(e.to_string()))?;
        for call in calls {
            stmt.execute(params![
                call.id(),
                call.harness,
                call.session_id,
                call.ts,
                call.event,
                call.tool,
                call.cwd,
                call.target,
                call.arg,
                call.ok,
                call.tool_use_id,
            ])
            .map_err(|e| Unavailable(format!("tool_call insert: {e}")))?;
        }
    }
    let new = tx.total_changes() as i64 - before;
    tx.commit().map_err(|e| Unavailable(e.to_string()))?;
    Ok(new)
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
