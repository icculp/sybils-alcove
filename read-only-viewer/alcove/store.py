"""Local SQLite store of derived facts.

Why this exists: every number in the live view is computed by tailing the last
~1MB of each transcript, so totals are recent-window and history beyond that
window is unrecoverable. An activity chart over days cannot come from a sliding
window.

The design rests on one property: **every fact ingested here has a natural id, so
ingestion is idempotent.** Claude assistant messages carry `message.id`; Codex
assistant messages carry `payload.id`; compactions and selections key on
(session, timestamp). With `INSERT OR IGNORE` on those keys, re-scanning
overlapping windows is free and harmless, which is what makes incremental
backfill, crash recovery, and a 2-second poll loop all the same operation.

`sqlite3` is in the standard library, so this costs no dependency.

Scope note: this module writes only its own derived cache. It never writes a
transcript, agent state, or anything an agent reads back.
"""

from __future__ import annotations

import os
import sqlite3
from pathlib import Path
from typing import Any, Iterable

SCHEMA = """
PRAGMA journal_mode=WAL;

CREATE TABLE IF NOT EXISTS turn (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL,
  harness     TEXT NOT NULL,
  ts          TEXT,
  model       TEXT,
  input       INTEGER,
  output      INTEGER,
  cache_read  INTEGER,
  cache_write INTEGER,
  -- 1 when the turn belongs to a subagent rather than the main thread, so
  -- parent totals can exclude work its children did.
  is_subagent INTEGER NOT NULL DEFAULT 0
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
  session_id TEXT,
  ts         TEXT,
  model      TEXT,
  requested  TEXT,
  PRIMARY KEY (session_id, ts, model)
);

CREATE TABLE IF NOT EXISTS compaction (
  session_id TEXT,
  ts         TEXT,
  trigger    TEXT,
  pre_tokens INTEGER,
  PRIMARY KEY (session_id, ts)
);

CREATE TABLE IF NOT EXISTS subagent (
  id         TEXT PRIMARY KEY,
  session_id TEXT,
  harness    TEXT,
  model      TEXT,
  role       TEXT,
  status     TEXT,
  first_seen TEXT,
  last_seen  TEXT
);

-- Process state cannot be recovered later: a transcript never records that a
-- session was alive at 3am. So it is sampled, not derived.
CREATE TABLE IF NOT EXISTS observation (
  ts         TEXT,
  session_id TEXT,
  state      TEXT,
  pids       TEXT,
  PRIMARY KEY (ts, session_id)
);
"""


def db_path() -> Path:
    """Where the store lives. XDG state dir, overridable."""
    explicit = os.environ.get("ALCOVE_DB", "")
    if explicit:
        return Path(explicit).expanduser()
    base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state"))
    return (base / "alcove" / "alcove.db").expanduser()


def connect(path: Path | None = None) -> sqlite3.Connection:
    target = path or db_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(target, timeout=10)
    conn.row_factory = sqlite3.Row
    conn.executescript(SCHEMA)
    return conn


# ------------------------------------------------------------------ ingestion

def _turn_rows(session: dict[str, Any]) -> Iterable[tuple]:
    sid, harness = session["session_id"], session["harness"]
    for row in session.get("_turn_rows") or []:
        yield (row["id"], sid, harness, row.get("ts"), row.get("model"),
               row.get("input"), row.get("output"), row.get("cache_read"),
               row.get("cache_write"), 0)
    for sub in session.get("subagents") or []:
        for row in sub.get("_turn_rows") or []:
            yield (row["id"], sid, harness, row.get("ts"), row.get("model"),
                   row.get("input"), row.get("output"), row.get("cache_read"),
                   row.get("cache_write"), 1)


def ingest(conn: sqlite3.Connection, snapshot: dict[str, Any]) -> dict[str, int]:
    """Write a snapshot's facts. Safe to call repeatedly on overlapping data.

    Returns how many rows were genuinely new, which is the number to watch when
    checking that a re-scan is a no-op.
    """
    # Named honestly: immutable facts report rows genuinely NEW, while session
    # and subagent are upserts, so their change count includes updates and is
    # reported as "seen" rather than passed off as new.
    counts = {"turn_new": 0, "selection_new": 0, "compaction_new": 0,
              "observation_new": 0, "session_seen": 0, "subagent_seen": 0}
    now = snapshot["generated_at"]
    with conn:  # one transaction; a partial ingest is re-runnable anyway
        rows = list(_turn_rows_all(snapshot))
        if rows:
            before = conn.total_changes
            conn.executemany(
                "INSERT OR IGNORE INTO turn (id, session_id, harness, ts, model,"
                " input, output, cache_read, cache_write, is_subagent)"
                " VALUES (?,?,?,?,?,?,?,?,?,?)", rows)
            counts["turn_new"] = conn.total_changes - before

        for session in snapshot["sessions"]:
            sid = session["session_id"]
            # First seen is kept; last seen advances. Never regress first_seen.
            before = conn.total_changes
            conn.execute(
                "INSERT INTO session (id, harness, project, cwd, branch,"
                " first_seen, last_seen) VALUES (?,?,?,?,?,?,?)"
                " ON CONFLICT(id) DO UPDATE SET last_seen=excluded.last_seen,"
                " project=excluded.project, cwd=excluded.cwd,"
                " branch=excluded.branch",
                (sid, session["harness"], session.get("project"),
                 session.get("cwd"), session.get("branch"),
                 session.get("last_ts") or now, session.get("last_ts") or now))
            counts["session_seen"] += conn.total_changes - before

            before = conn.total_changes
            conn.executemany(
                "INSERT OR IGNORE INTO selection (session_id, ts, model,"
                " requested) VALUES (?,?,?,?)",
                [(sid, s["at"], s["model"], s.get("requested"))
                 for s in session.get("selections") or []])
            counts["selection_new"] += conn.total_changes - before

            before = conn.total_changes
            conn.executemany(
                "INSERT OR IGNORE INTO compaction (session_id, ts, trigger,"
                " pre_tokens) VALUES (?,?,?,?)",
                [(sid, c["at"], c.get("trigger"), c.get("pre_tokens"))
                 for c in session.get("compactions") or []])
            counts["compaction_new"] += conn.total_changes - before

            before = conn.total_changes
            for sub in session.get("subagents") or []:
                conn.execute(
                    "INSERT INTO subagent (id, session_id, harness, model, role,"
                    " status, first_seen, last_seen) VALUES (?,?,?,?,?,?,?,?)"
                    " ON CONFLICT(id) DO UPDATE SET model=excluded.model,"
                    " status=excluded.status, last_seen=excluded.last_seen",
                    (sub["id"], sid, session["harness"], sub.get("model"),
                     sub.get("role"), sub.get("status"), now, now))
            counts["subagent_seen"] += conn.total_changes - before

            # One observation per snapshot per session: this is a sample of
            # something unrecoverable, so it is inserted, not deduped away.
            before = conn.total_changes
            conn.execute(
                "INSERT OR IGNORE INTO observation (ts, session_id, state, pids)"
                " VALUES (?,?,?,?)",
                (now, sid, session.get("state"),
                 ",".join(str(p) for p in session.get("pids") or [])))
            counts["observation_new"] += conn.total_changes - before
    return counts


def _turn_rows_all(snapshot: dict[str, Any]) -> Iterable[tuple]:
    for session in snapshot["sessions"]:
        yield from _turn_rows(session)


# --------------------------------------------------------------------- queries

def prune_observations(conn: sqlite3.Connection, days: int = 90) -> int:
    """Drop old process samples.

    One observation per session per snapshot: at a 2-second poll that is ~1.3M
    rows a day, which is a footgun rather than a history. Sample from the
    ingester on a slow cadence and prune on a schedule.
    """
    with conn:
        cur = conn.execute("DELETE FROM observation WHERE ts < date('now', ?)",
                           (f"-{int(days)} days",))
    return cur.rowcount


def daily_activity(conn: sqlite3.Connection, days: int = 30) -> list[dict[str, Any]]:
    """Turns and output tokens per day per harness — the activity chart.

    Reads the store, never the transcripts, so it is not bounded by the tail
    window the live view is stuck with.
    """
    rows = conn.execute(
        "SELECT substr(ts,1,10) AS day, harness,"
        "       COUNT(*) AS turns,"
        "       COALESCE(SUM(output),0) AS output,"
        "       COUNT(DISTINCT session_id) AS sessions"
        "  FROM turn"
        " WHERE ts >= date('now', ?)"
        " GROUP BY day, harness"
        " ORDER BY day", (f"-{int(days)} days",)).fetchall()
    return [dict(r) for r in rows]


def totals(conn: sqlite3.Connection) -> dict[str, Any]:
    """Lifetime figures, which the live view cannot compute."""
    row = conn.execute(
        "SELECT COUNT(*) AS turns, COALESCE(SUM(output),0) AS output,"
        "       COUNT(DISTINCT session_id) AS sessions,"
        "       MIN(ts) AS first_ts, MAX(ts) AS last_ts FROM turn").fetchone()
    return dict(row)
