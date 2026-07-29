"""Reading JSONL transcripts off disk.

Two traps live here, and both produce confidently wrong output rather than an
error, so they are handled once in this module rather than at each call site.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any

from . import config


def _parse(data: bytes, limit: int | None) -> list[dict[str, Any]]:
    lines = data.decode("utf-8", errors="replace").splitlines()
    if limit is not None:
        lines = lines[-limit:]
    out = []
    for line in lines:
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if isinstance(event, dict):
            out.append(event)
    return out


def tail_events(path: Path) -> list[dict[str, Any]]:
    """Parsed JSONL objects from the tail of a transcript."""
    try:
        size = path.stat().st_size
    except OSError:
        return []
    try:
        with path.open("rb") as handle:
            if size <= config.TAIL_BYTES:
                data = handle.read()
            else:
                handle.seek(-config.TAIL_BYTES, os.SEEK_END)
                data = handle.read()
                # First line is almost certainly cut mid-record.
                data = data.split(b"\n", 1)[1] if b"\n" in data else data
    except OSError:
        return []
    return _parse(data, config.TAIL_LINES)


def head_events(path: Path, limit_bytes: int = 1 << 16) -> list[dict[str, Any]]:
    """Parsed objects from the START of a transcript.

    Identity lives in the first record — Codex writes `session_meta` (thread id,
    cwd, and the `parent_thread_id` that marks a spawned agent) as line one. A
    tail-only read silently loses identity on any file bigger than the tail
    window, which reads as "this session does not exist" rather than as an error.
    """
    try:
        with path.open("rb") as handle:
            data = handle.read(limit_bytes)
    except OSError:
        return []
    # Drop a trailing partial line so it does not fail to parse.
    if b"\n" in data:
        data = data.rsplit(b"\n", 1)[0]
    return _parse(data, None)


def chronological(events: list[dict[str, Any]],
                  key: str = "timestamp") -> list[dict[str, Any]]:
    """Dedupe by uuid and sort by timestamp.

    Compaction rewrites transcripts with repeated, out-of-order blocks, so raw
    file order is not chronology — scanning it directly reports model switches
    that run backwards in time.
    """
    seen: set[str] = set()
    unique = []
    for event in events:
        uuid = event.get("uuid")
        if isinstance(uuid, str):
            if uuid in seen:
                continue
            seen.add(uuid)
        unique.append(event)
    unique.sort(key=lambda e: str(e.get(key) or ""))
    return unique


def mtime_age(path: Path) -> float | None:
    try:
        return time.time() - path.stat().st_mtime
    except OSError:
        return None


def file_size(path: Path) -> int:
    try:
        return path.stat().st_size
    except OSError:
        return 0
