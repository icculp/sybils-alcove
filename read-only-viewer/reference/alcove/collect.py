"""Assembling one snapshot across every source.

The only place that decides session *state*, because that decision needs both a
transcript fact (was it written recently) and a process fact (does a pid own it),
and those come from different sources.
"""

from __future__ import annotations

import threading
import time
from typing import Any

from . import config
from .sources.claude import collect_claude
from .sources.codex import collect_codex
from .sources.process import claude_bin, codex_process_count, running_pids

_cache: dict[str, Any] = {"at": 0.0, "data": None, "cost": 0.0}
# Single-flight: without this every waiting request starts its OWN collect.
# With a 2s TTL, a 3s poll and a 5s collect, each poll missed the cache and
# launched another scan while the previous ones were still running; they
# contended, each got slower, and one request was measured at 59.7s.
_refresh = threading.Lock()

# State outranks file age: a running session whose transcript has been quiet for
# a day still belongs above a finished one that wrote a minute ago.
_RANK = {"running": 0, "writing": 1, "unknown": 2, "ended": 3}


def collect() -> dict[str, Any]:
    pids, pid_source = running_pids()
    sessions = collect_claude() + collect_codex()
    for session in sessions:
        proc = pids.get(session["session_id"]) or {}
        session["pids"] = proc.get("pids") or []
        session["agent_name"] = proc.get("name") or ""
        session["kind"] = proc.get("kind") or ""
        session["switches"] = max(0, len(session["timeline"]) - 1)
        # Four distinct facts, never collapsed into one "live" flag:
        #   running  — a process owns this session id right now (authoritative)
        #   writing  — no owning process, but the transcript moved recently
        #   ended    — neither; the transcript is all that is left
        #   unknown  — the pid lookup failed, so absence proves nothing
        if session["pids"]:
            session["state"] = "running"
        elif session["harness"] == "claude" and pid_source != "ok":
            session["state"] = "unknown"
        elif session["live"]:
            session["state"] = "writing"
        else:
            session["state"] = "ended"
        # Codex has no per-session pid, so its transcript freshness is the only
        # signal available — mark it inferred rather than implying certainty.
        session["state_inferred"] = session["harness"] != "claude"
        # A process can own a session for a day without the model writing a word.
        # "running" then means the window is open, NOT that work is happening,
        # and rendering it the same as a session mid-turn makes every abandoned
        # terminal look busy. Codex never looked wrong here only because it has
        # no pid to hold it green.
        session["quiet"] = session["state"] == "running" and not session["live"]
    sessions.sort(key=lambda s: (_RANK.get(s["state"], 9),
                                 s["age_s"] if s["age_s"] is not None else 1e18))
    return {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "live_window_s": config.LIVE_WINDOW_S,
        "tail_lines": config.TAIL_LINES,
        "pid_source": pid_source,
        "claude_bin": claude_bin() or None,
        "codex_processes": codex_process_count(),
        "sessions": sessions,
    }


def _fresh(now: float) -> bool:
    """Serve the cache for at least as long as the last collect TOOK.

    A fixed 2s TTL against a 5s collect means the scan never stops running: the
    result is stale before it is stored. Scaling the window to the observed cost
    keeps the duty cycle bounded no matter how large the corpus grows.
    """
    if _cache["data"] is None:
        return False
    ttl = max(config.CACHE_TTL_S, _cache["cost"])
    return now - _cache["at"] <= ttl


def cached() -> dict[str, Any]:
    if _fresh(time.time()):
        return _cache["data"]
    # One collect at a time. Everyone else waits here and then re-checks, so a
    # queue of waiting requests costs one scan rather than one scan each.
    with _refresh:
        if _fresh(time.time()):
            return _cache["data"]
        started = time.time()
        data = collect()
        _cache["data"] = data
        _cache["cost"] = time.time() - started
        _cache["at"] = time.time()
        return data


def public(snapshot: dict[str, Any]) -> dict[str, Any]:
    """The API payload: keys prefixed with `_` are ingest-only and stripped.

    Per-turn rows exist so the store can key on them; shipping thousands of them
    to a browser that polls every 3 seconds would be absurd.
    """
    def clean(obj: Any) -> Any:
        if isinstance(obj, dict):
            return {k: clean(v) for k, v in obj.items() if not k.startswith("_")}
        if isinstance(obj, list):
            return [clean(x) for x in obj]
        return obj

    return clean(snapshot)
