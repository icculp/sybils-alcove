"""Codex's own sqlite, read as optional enrichment.

Codex maintains `~/.codex/state_<N>.sqlite` alongside the rollout files. It
holds two things the transcripts do not give us cleanly:

  threads             agent_nickname, agent_role, model, reasoning_effort,
                      git_branch, cli_version, archived
  thread_spawn_edges  parent -> child, with an open/closed status

Measured against this corpus, what it actually adds over the transcripts is
narrower than it first looks — 19 sessions, 71 subagents:

    subagent status (open/closed)   0 -> 71   the whole point
    git_branch                      0 ->  3
    reasoning_effort               16 -> 18
    agent_nickname                 71 -> 71   already in the rollout head
    agent_role                     56 -> 56   already in the rollout head
    parent edges                   71 -> 71   already in the rollout head

So the reason to read this file is `status`: Codex writes no completion record
into a transcript, so an ended subagent was previously indistinguishable from an
idle one. Nicknames and roles were already available and are read here only as a
fallback. `thread_spawn_edges` added no parent link the heads did not already
have — it is kept as a backstop for a truncated head, not as the primary source.

THIS IS A PRIVATE FILE WITH NO COMPATIBILITY PROMISE. The trailing number is a
schema generation — `state_5` will become `state_6` and the columns may move.
So every function here returns empty on any failure, and the caller treats a
result as a bonus, never a requirement: when this file changes shape, the viewer
degrades to exactly what it did before, rather than breaking.

Two facts drive the reading strategy:

* The database is in WAL mode. A read-only sqlite connection to a WAL database
  must create a `-shm` file, which needs write permission on the DIRECTORY —
  which the server does not have (ProtectHome=read-only). `immutable=1` avoids
  the shm entirely, but promising sqlite that a file will not change while Codex
  is actively writing it invites torn reads.
* So we copy the main database file and open the COPY with `immutable=1`, where
  the promise is actually true. A WAL database's main file is a valid database
  at its last checkpoint, so the snapshot may lag by the un-checkpointed tail.
  That is fine for nicknames and roles, which never change after a thread
  starts; it is not a source we would use for token counts.

Deliberately NOT read: `threads.tokens_used`. One thread reports 848,292,502 —
a cumulative counter re-added per turn. Token totals come from the transcripts.
"""

from __future__ import annotations

import os
import re
import shutil
import sqlite3
import tempfile
from pathlib import Path
from typing import Any

from .. import config

# Re-snapshotting on every 3s poll would copy the file ~20 times a minute for
# data that changes when an agent is spawned. Keyed on (mtime, size).
_cache: dict[str, Any] = {"stamp": None, "copy": None, "data": None}

_GEN_RE = re.compile(r"state_(\d+)\.sqlite$")


def state_db() -> Path | None:
    """The newest `state_<N>.sqlite`, or an explicit override."""
    if config.CODEX_STATE_DB:
        path = Path(config.CODEX_STATE_DB).expanduser()
        return path if path.is_file() else None
    if not config.CODEX_HOME.is_dir():
        return None
    best: tuple[int, Path] | None = None
    try:
        for entry in config.CODEX_HOME.glob("state_*.sqlite"):
            match = _GEN_RE.search(entry.name)
            if not match or not entry.is_file():
                continue
            gen = int(match.group(1))
            if best is None or gen > best[0]:
                best = (gen, entry)
    except OSError:
        return None
    return best[1] if best else None


def _sweep(tmp: Path, keep: Path) -> None:
    """Drop snapshots left by processes that are gone.

    The name carries the pid so two servers never share one, which means a
    restart abandons its copy. Under PrivateTmp these die with the unit, but a
    plain `python3 alcove.py` would leave one behind per run.
    """
    try:
        stale = list(tmp.glob("alcove-codex-state-*.sqlite"))
    except OSError:
        return
    for entry in stale:
        if entry == keep:
            continue
        try:
            pid = int(entry.stem.rsplit("-", 1)[1])
        except (ValueError, IndexError):
            continue
        try:
            os.kill(pid, 0)
            continue  # owner still running
        except PermissionError:
            continue  # exists, someone else's
        except OSError:
            pass
        try:
            entry.unlink()
        except OSError:
            pass


def _snapshot(source: Path) -> Path | None:
    """A private copy of the database, safe to open with immutable=1."""
    try:
        stat = source.stat()
    except OSError:
        return None
    stamp = (stat.st_mtime_ns, stat.st_size)
    cached = _cache.get("copy")
    if _cache.get("stamp") == stamp and cached and Path(cached).is_file():
        return Path(cached)
    tmp = Path(tempfile.gettempdir())
    target = tmp / f"alcove-codex-state-{os.getpid()}.sqlite"
    _sweep(tmp, keep=target)
    try:
        shutil.copyfile(source, target)
    except OSError:
        return None
    _cache["stamp"] = stamp
    _cache["copy"] = str(target)
    _cache["data"] = None
    return target


def _rows(conn: sqlite3.Connection, table: str,
          columns: list[str]) -> list[dict[str, Any]]:
    """Rows as dicts, selecting only the columns that actually exist.

    A renamed or dropped column in a future generation then costs us that one
    field rather than the whole table.
    """
    try:
        have = {r[1] for r in conn.execute(f"pragma table_info({table})")}
    except sqlite3.Error:
        return []
    usable = [c for c in columns if c in have]
    if not usable:
        return []
    try:
        cursor = conn.execute(f"select {','.join(usable)} from {table}")
        return [dict(zip(usable, row)) for row in cursor]
    except sqlite3.Error:
        return []


def read() -> dict[str, Any]:
    """{"threads": {id: {...}}, "edges": {child: {parent, status}}, "source": str}.

    Empty dicts on any failure — a missing, moved, or reshaped database is a
    normal outcome, not an error worth surfacing as one.
    """
    source = state_db()
    if source is None:
        return {"threads": {}, "edges": {}, "source": ""}
    snapshot = _snapshot(source)
    if snapshot is None:
        return {"threads": {}, "edges": {}, "source": ""}
    if _cache.get("data") is not None:
        return _cache["data"]

    threads: dict[str, dict[str, Any]] = {}
    edges: dict[str, dict[str, str]] = {}
    try:
        conn = sqlite3.connect(f"file:{snapshot}?mode=ro&immutable=1", uri=True,
                               timeout=5)
    except sqlite3.Error:
        return {"threads": {}, "edges": {}, "source": ""}
    try:
        for record in _rows(conn, "threads", [
                "id", "agent_nickname", "agent_role", "model", "reasoning_effort",
                "git_branch", "cli_version", "archived", "thread_source"]):
            tid = str(record.get("id") or "")
            if tid:
                threads[tid] = record
        for record in _rows(conn, "thread_spawn_edges",
                            ["parent_thread_id", "child_thread_id", "status"]):
            child = str(record.get("child_thread_id") or "")
            if child:
                edges[child] = {
                    "parent": str(record.get("parent_thread_id") or ""),
                    "status": str(record.get("status") or ""),
                }
    except sqlite3.Error:
        return {"threads": {}, "edges": {}, "source": ""}
    finally:
        conn.close()

    data = {"threads": threads, "edges": edges, "source": str(source)}
    _cache["data"] = data
    return data
