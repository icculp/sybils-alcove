#!/usr/bin/env python3
"""Sybil's Alcove — a live view of local coding-agent sessions and their subagents.

Answers the question the built-in tooling does not: *which model is actually
serving this session right now, what did it spawn, and what is that costing?*

`claude agents --json` reports pid/cwd/sessionId and nothing else — no model, no
tokens, no subagents. A serving model can also change mid-session without the
switch appearing in the conversation, so the transcript is the only honest
record of what ran.

Everything here is read from transcript files on disk, which means a subagent
shows up as soon as it writes its first event rather than when it finishes:

  Claude Code
    ~/.claude/projects/<project>/<session-id>.jsonl          main thread
    ~/.claude/projects/<project>/<session-id>/subagents/
        agent-<agentId>.jsonl                                one per subagent

  Codex
    ~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<id>.jsonl    one per session;
        spawned agents are sibling files linked by parent_thread_id

Read-only by design: it opens transcripts, never writes them, and never calls a
model API. Binds 127.0.0.1 unless told otherwise — a private overlay is not
authentication, and this exposes prompts.
"""

from __future__ import annotations

import hmac
import json
import os
import secrets
import subprocess
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

CLAUDE_ROOT = Path(os.environ.get("ALCOVE_CLAUDE_ROOT", Path.home() / ".claude" / "projects"))
CODEX_ROOT = Path(os.environ.get("ALCOVE_CODEX_ROOT", Path.home() / ".codex" / "sessions"))
PORT = int(os.environ.get("ALCOVE_PORT", "8899"))
# Localhost by default. Set ALCOVE_BIND=0.0.0.0 deliberately, knowing this page
# shows task prompts and there is no auth in front of it.
BIND = os.environ.get("ALCOVE_BIND", "127.0.0.1")
# No transcript write in this long => treated as idle. Generous on purpose: a
# long tool call or a thinking turn writes nothing.
LIVE_WINDOW_S = float(os.environ.get("ALCOVE_LIVE_WINDOW_S", "300"))
# Transcripts reach 100MB+; only the tail is read, so totals are recent-window.
TAIL_LINES = int(os.environ.get("ALCOVE_TAIL_LINES", "4000"))
TAIL_BYTES = int(os.environ.get("ALCOVE_TAIL_BYTES", str(1 << 20)))
CACHE_TTL_S = 2.0

# Shared secret required when not bound to loopback. Empty + non-local bind is
# refused at startup rather than served open (see main()).
TOKEN = os.environ.get("ALCOVE_TOKEN", "")
COOKIE = "alcove_token"
LOCAL_BINDS = {"127.0.0.1", "localhost", "::1"}


def is_local_bind() -> bool:
    return BIND in LOCAL_BINDS


def token_ok(supplied: str) -> bool:
    """Constant-time compare so a wrong guess leaks no timing signal."""
    return bool(TOKEN) and hmac.compare_digest(supplied, TOKEN)

_cache: dict[str, Any] = {"at": 0.0, "data": None}


# --------------------------------------------------------------------------- io

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
            if size <= TAIL_BYTES:
                data = handle.read()
            else:
                handle.seek(-TAIL_BYTES, os.SEEK_END)
                data = handle.read()
                # First line is almost certainly cut mid-record.
                data = data.split(b"\n", 1)[1] if b"\n" in data else data
    except OSError:
        return []
    return _parse(data, TAIL_LINES)


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


def chronological(events: list[dict[str, Any]], key: str = "timestamp") -> list[dict[str, Any]]:
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


# ------------------------------------------------------------------------ usage

def new_usage() -> dict[str, int]:
    return {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "reasoning": 0}


def add_anthropic_usage(total: dict[str, int], usage: Any) -> None:
    if not isinstance(usage, dict):
        return
    total["input"] += int(usage.get("input_tokens") or 0)
    total["output"] += int(usage.get("output_tokens") or 0)
    total["cache_read"] += int(usage.get("cache_read_input_tokens") or 0)
    total["cache_write"] += int(usage.get("cache_creation_input_tokens") or 0)


def is_real_model(value: Any) -> bool:
    """`<synthetic>` marks harness-injected messages, not a served model.

    Counting it manufactures phantom switch pairs.
    """
    return bool(value) and not str(value).startswith("<")


def live_first(item: dict[str, Any]) -> tuple[bool, float]:
    """Running first, then freshest. Used for the session list AND the subagent
    drilldown so the eye moves the same way at both levels; a subagent with no
    transcript has no age and sorts last.
    """
    age = item.get("age_s")
    return (not item.get("live"), age if age is not None else 1e18)


def push_model(timeline: list[dict[str, str]], model: str, at: str) -> None:
    if not timeline or timeline[-1]["model"] != model:
        timeline.append({"model": model, "at": at})


# ----------------------------------------------------------------- claude code

def scan_claude(path: Path, *, main_thread_only: bool) -> dict[str, Any]:
    timeline: list[dict[str, str]] = []
    usage = new_usage()
    ctx_usage = new_usage()
    turns = ctx_turns = 0
    compactions: list[dict[str, Any]] = []
    last_ts = cwd = branch = effort = ""
    for event in chronological(tail_events(path)):
        if event.get("cwd") and not cwd:
            cwd = str(event["cwd"])
        if event.get("gitBranch") and not branch:
            branch = str(event["gitBranch"])
        ts = str(event.get("timestamp") or "")
        # A compact boundary means everything before it has left the context
        # window. A total that spans the boundary describes a context that no
        # longer exists, so keep a second set of counters that resets here.
        if event.get("subtype") == "compact_boundary":
            meta = event.get("compactMetadata")
            meta = meta if isinstance(meta, dict) else {}
            compactions.append({
                "at": ts, "trigger": str(meta.get("trigger") or ""),
                "pre_tokens": meta.get("preTokens"),
            })
            ctx_usage = new_usage()
            ctx_turns = 0
            continue
        if event.get("type") != "assistant":
            continue
        # A parent's own totals must not absorb its subagents' turns.
        if main_thread_only and event.get("isSidechain"):
            continue
        message = event.get("message")
        if not isinstance(message, dict):
            continue
        if ts:
            last_ts = ts
        level = event.get("effort")
        if isinstance(level, dict) and level.get("level"):
            effort = str(level["level"])
        add_anthropic_usage(usage, message.get("usage"))
        add_anthropic_usage(ctx_usage, message.get("usage"))
        model = message.get("model")
        if not is_real_model(model):
            continue
        turns += 1
        ctx_turns += 1
        push_model(timeline, str(model), ts)
    return {
        "timeline": timeline, "model": timeline[-1]["model"] if timeline else "",
        "usage": usage, "turns": turns, "last_ts": last_ts,
        "cwd": cwd, "branch": branch, "effort": effort,
        "compactions": compactions,
        "usage_since_compact": ctx_usage if compactions else None,
        "turns_since_compact": ctx_turns if compactions else None,
    }


def claude_spawn_records(transcript: Path) -> dict[str, dict[str, Any]]:
    """Per-subagent records the parent wrote, keyed by agentId.

    Written when a subagent is launched or completes, so this enriches the
    child-transcript reading; it never gates whether a subagent is shown.
    """
    out: dict[str, dict[str, Any]] = {}
    for event in tail_events(transcript):
        result = event.get("toolUseResult")
        if not isinstance(result, dict) or not result.get("agentId"):
            continue
        out[str(result["agentId"])] = {
            "agent_type": result.get("agentType") or "",
            "resolved_model": result.get("resolvedModel") or "",
            "status": result.get("status") or "",
            "reported_tokens": result.get("totalTokens"),
            "tool_uses": result.get("totalToolUseCount"),
            "duration_ms": result.get("totalDurationMs"),
            "task": (str(result.get("prompt") or ""))[:240],
        }
    return out


def collect_claude() -> list[dict[str, Any]]:
    if not CLAUDE_ROOT.is_dir():
        return []
    sessions = []
    for project in sorted(p for p in CLAUDE_ROOT.iterdir() if p.is_dir()):
        for transcript in sorted(project.glob("*.jsonl")):
            sid = transcript.stem
            info = scan_claude(transcript, main_thread_only=True)
            sub_dir = project / sid / "subagents"
            records = claude_spawn_records(transcript) if sub_dir.is_dir() else {}
            subs = []
            for child in sorted(sub_dir.glob("agent-*.jsonl")) if sub_dir.is_dir() else []:
                agent_id = child.stem[len("agent-"):]
                child_info = scan_claude(child, main_thread_only=False)
                record = records.get(agent_id, {})
                age = mtime_age(child)
                subs.append({
                    "id": agent_id, "label": agent_id[:12],
                    # Child transcript wins: written from the first event, so a
                    # running subagent reports its model before any parent record.
                    "model": child_info["model"] or record.get("resolved_model", ""),
                    "record_model": record.get("resolved_model", ""),
                    "role": record.get("agent_type", ""),
                    "status": record.get("status", ""),
                    "timeline": child_info["timeline"], "usage": child_info["usage"],
                    "turns": child_info["turns"],
                    "reported_tokens": record.get("reported_tokens"),
                    "tool_uses": record.get("tool_uses"), "task": record.get("task", ""),
                    "age_s": age, "live": age is not None and age < LIVE_WINDOW_S,
                    "size": file_size(child),
                })
            # A record with no transcript is still a spawn that happened.
            seen = {s["id"] for s in subs}
            for agent_id, record in records.items():
                if agent_id in seen:
                    continue
                subs.append({
                    "id": agent_id, "label": agent_id[:12],
                    "model": record.get("resolved_model", ""),
                    "record_model": record.get("resolved_model", ""),
                    "role": record.get("agent_type", ""), "status": record.get("status", ""),
                    "timeline": [], "usage": new_usage(), "turns": 0,
                    "reported_tokens": record.get("reported_tokens"),
                    "tool_uses": record.get("tool_uses"), "task": record.get("task", ""),
                    "age_s": None, "live": False, "size": 0, "no_transcript": True,
                })
            # Running subagents first: a busy session has dozens of finished ones
            # and glob order (agent id) would bury the two you care about.
            subs.sort(key=live_first)
            age = mtime_age(transcript)
            sessions.append({
                "harness": "claude", "session_id": sid, "label": sid[:8],
                "project": project.name, "cwd": info["cwd"], "branch": info["branch"],
                "effort": info["effort"], "model": info["model"],
                "timeline": info["timeline"], "usage": info["usage"],
                "turns": info["turns"], "last_ts": info["last_ts"], "age_s": age,
                "live": age is not None and age < LIVE_WINDOW_S,
                "compactions": info["compactions"],
                "usage_since_compact": info["usage_since_compact"],
                "turns_since_compact": info["turns_since_compact"],
                "subagents": subs, "path": str(transcript),
            })
    return sessions


# ------------------------------------------------------------------------ codex

def scan_codex(path: Path) -> dict[str, Any]:
    """One Codex rollout file.

    Model/effort come from `turn_context` events. Token totals come from the
    last `token_count` event, whose `total_token_usage` is already cumulative
    for the session — summing them would multiply-count.
    """
    timeline: list[dict[str, str]] = []
    usage = new_usage()
    turns = ctx_turns = 0
    compactions: list[dict[str, Any]] = []
    usage_at_compact: dict[str, int] | None = None
    last_ts = cwd = effort = role = nickname = ""
    sid = parent = ""
    context_window = None
    # Identity from the head (line 1), activity from the tail. `payload.id` is
    # the thread's OWN id; `payload.session_id` on a spawned agent is the
    # PARENT's, so reading session_id first would collapse children into parents.
    for event in head_events(path):
        if event.get("type") != "session_meta":
            continue
        payload = event.get("payload")
        if not isinstance(payload, dict):
            continue
        sid = str(payload.get("id") or payload.get("session_id") or "")
        cwd = str(payload.get("cwd") or "")
        role = str(payload.get("agent_role") or "")
        nickname = str(payload.get("agent_nickname") or "")
        parent = str(payload.get("parent_thread_id") or "")
        source = payload.get("source")
        if not parent and isinstance(source, dict):
            spawn = (source.get("subagent") or {}).get("thread_spawn") or {}
            parent = str(spawn.get("parent_thread_id") or "")
            role = role or str(spawn.get("agent_role") or "")
            nickname = nickname or str(spawn.get("agent_nickname") or "")
        break
    for event in chronological(tail_events(path)):
        kind = event.get("type")
        payload = event.get("payload")
        ts = str(event.get("timestamp") or "")
        # Codex marks one compaction twice — a `compacted` record and an
        # `event_msg`/`context_compacted` — so dedupe by timestamp. Its token
        # totals are cumulative snapshots, so the post-boundary figure is a
        # subtraction, not a reset. No pre-context size is recorded, unlike
        # Claude's `preTokens`, so don't invent one.
        if kind == "compacted" or (isinstance(payload, dict)
                                   and payload.get("type") == "context_compacted"):
            # The paired markers land milliseconds apart, so compare at second
            # granularity — two real compactions in one second is not a thing.
            if not compactions or compactions[-1]["at"][:19] != ts[:19]:
                compactions.append({"at": ts, "trigger": "", "pre_tokens": None})
            usage_at_compact = dict(usage)
            ctx_turns = 0
            continue
        if not isinstance(payload, dict):
            continue
        if kind == "turn_context":
            model = payload.get("model")
            if payload.get("effort"):
                effort = str(payload["effort"])
            if is_real_model(model):
                turns += 1
                ctx_turns += 1
                if ts:
                    last_ts = ts
                push_model(timeline, str(model), ts)
        elif kind == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info")
            if not isinstance(info, dict):
                continue
            total = info.get("total_token_usage")
            if isinstance(total, dict):
                # Cumulative snapshot: replace, never accumulate.
                usage = {
                    "input": int(total.get("input_tokens") or 0),
                    "output": int(total.get("output_tokens") or 0),
                    "cache_read": int(total.get("cached_input_tokens") or 0),
                    "cache_write": int(total.get("cache_write_input_tokens") or 0),
                    "reasoning": int(total.get("reasoning_output_tokens") or 0),
                }
            if info.get("model_context_window"):
                context_window = info["model_context_window"]
    return {
        "session_id": sid, "parent": parent, "role": role, "nickname": nickname,
        "timeline": timeline, "model": timeline[-1]["model"] if timeline else "",
        "usage": usage, "turns": turns, "last_ts": last_ts, "cwd": cwd,
        "effort": effort, "context_window": context_window,
        "compactions": compactions,
        "usage_since_compact": (
            {k: max(0, usage[k] - usage_at_compact.get(k, 0)) for k in usage}
            if usage_at_compact is not None else None),
        "turns_since_compact": ctx_turns if compactions else None,
    }


def collect_codex() -> list[dict[str, Any]]:
    """Codex sessions, with spawned agents nested under their parent.

    Unlike Claude, a Codex subagent writes a full sibling transcript with its own
    thread id; the link back is `parent_thread_id` in its `session_meta`.
    """
    if not CODEX_ROOT.is_dir():
        return []
    # A Codex thread can span several rollout files (resume, rollback). Merge by
    # thread id, newest file wins for current model/effort, and keep the largest
    # cumulative token snapshot rather than summing — each is already a total.
    merged: dict[str, dict[str, Any]] = {}
    for path in sorted(CODEX_ROOT.rglob("*.jsonl"), key=lambda p: p.stat().st_mtime
                       if p.exists() else 0):
        info = scan_codex(path)
        sid = info["session_id"]
        if not sid:
            continue
        age = mtime_age(path)
        info["path"] = path
        info["age_s"] = age
        info["live"] = age is not None and age < LIVE_WINDOW_S
        info["size"] = file_size(path)
        prior = merged.get(sid)
        if prior is None:
            merged[sid] = info
            continue
        prior["size"] += info["size"]
        prior["turns"] += info["turns"]
        prior["timeline"].extend(
            x for x in info["timeline"]
            if not prior["timeline"] or prior["timeline"][-1]["model"] != x["model"])
        if info["usage"]["output"] >= prior["usage"]["output"]:
            prior["usage"] = info["usage"]
        known = {x["at"][:19] for x in prior["compactions"]}
        prior["compactions"].extend(
            x for x in info["compactions"] if x["at"][:19] not in known)
        # This file is newer (sorted), so its state is the current state.
        for field in ("model", "effort", "cwd", "role", "nickname", "parent",
                      "last_ts", "usage_since_compact", "turns_since_compact"):
            if info.get(field):
                prior[field] = info[field]
        if info["age_s"] is not None and (
                prior["age_s"] is None or info["age_s"] < prior["age_s"]):
            prior["age_s"], prior["live"], prior["path"] = (
                info["age_s"], info["live"], info["path"])
    scanned = list(merged.values())

    children: dict[str, list[dict[str, Any]]] = {}
    for info in scanned:
        if info["parent"]:
            children.setdefault(info["parent"], []).append(info)

    sessions = []
    for info in scanned:
        if info["parent"]:
            continue  # rendered under its parent
        subs = []
        for child in children.get(info["session_id"], []):
            subs.append({
                "id": child["session_id"], "label": child["session_id"][:12],
                "model": child["model"], "record_model": "",
                "role": child["role"] or child["nickname"], "status": "",
                "timeline": child["timeline"], "usage": child["usage"],
                "turns": child["turns"], "reported_tokens": child["usage"]["output"] or None,
                "tool_uses": None, "task": child["nickname"],
                "age_s": child["age_s"], "live": child["live"], "size": child["size"],
            })
        subs.sort(key=live_first)
        sessions.append({
            "harness": "codex", "session_id": info["session_id"],
            "label": info["session_id"][:8],
            "project": Path(info["cwd"]).name if info["cwd"] else "unknown",
            "cwd": info["cwd"], "branch": "", "effort": info["effort"],
            "model": info["model"], "timeline": info["timeline"],
            "usage": info["usage"], "turns": info["turns"],
            "last_ts": info["last_ts"], "age_s": info["age_s"], "live": info["live"],
            "compactions": info["compactions"],
            "usage_since_compact": info["usage_since_compact"],
            "turns_since_compact": info["turns_since_compact"],
            "subagents": subs, "path": str(info["path"]),
        })
    return sessions


# ------------------------------------------------------------------ live process

def running_pids() -> dict[str, list[int]]:
    """sessionId -> pids, from the Claude CLI's own process list."""
    try:
        raw = subprocess.run(
            ["claude", "agents", "--json", "--all"],
            capture_output=True, text=True, timeout=20,
        ).stdout
        rows = json.loads(raw) if raw.strip() else []
    except Exception:
        return {}
    out: dict[str, list[int]] = {}
    for row in rows if isinstance(rows, list) else []:
        sid, pid = str(row.get("sessionId") or ""), row.get("pid")
        if sid and isinstance(pid, int):
            out.setdefault(sid, []).append(pid)
    return out


def collect() -> dict[str, Any]:
    pids = running_pids()
    sessions = collect_claude() + collect_codex()
    for session in sessions:
        session["pids"] = pids.get(session["session_id"], [])
        session["switches"] = max(0, len(session["timeline"]) - 1)
    sessions.sort(key=live_first)
    return {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "live_window_s": LIVE_WINDOW_S,
        "tail_lines": TAIL_LINES,
        "sessions": sessions,
    }


def cached() -> dict[str, Any]:
    now = time.time()
    if _cache["data"] is None or now - _cache["at"] > CACHE_TTL_S:
        _cache["data"] = collect()
        _cache["at"] = now
    return _cache["data"]


# -------------------------------------------------------------------------- web

PAGE = r"""<!doctype html>
<html><head><meta charset="utf-8"><title>Sybil's Alcove</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
:root{--bg:#0d1117;--panel:#161b22;--line:#30363d;--fg:#e6edf3;--dim:#8b949e;
--live:#3fb950;--idle:#6e7681;--warn:#d29922;--acc:#58a6ff;--sub:#d2a8ff}
@media (prefers-color-scheme:light){:root{--bg:#fff;--panel:#f6f8fa;--line:#d0d7de;
--fg:#1f2328;--dim:#636c76;--acc:#0969da;--sub:#8250df;--warn:#9a6700}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);
font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace}
header{position:sticky;top:0;background:var(--panel);border-bottom:1px solid var(--line);
padding:10px 14px;display:flex;gap:14px;align-items:center;flex-wrap:wrap;z-index:5}
h1{font-size:14px;margin:0;letter-spacing:1px;font-weight:600}
.muted{color:var(--dim)}
.wrap{padding:14px;max-width:1600px}
.s{background:var(--panel);border:1px solid var(--line);border-radius:6px;
margin-bottom:10px;overflow:hidden}
.shead{padding:9px 12px;display:flex;gap:10px;align-items:baseline;flex-wrap:wrap;
cursor:pointer;user-select:none}
.s.open .shead{border-bottom:1px solid var(--line)}
.s:not(.open) .body{display:none}
.caret{color:var(--dim);width:9px;flex:0 0 auto}
.s.open .caret::before{content:"▾"} .s:not(.open) .caret::before{content:"▸"}
.dot{width:8px;height:8px;border-radius:50%;flex:0 0 auto}
.dot.live{background:var(--live);box-shadow:0 0 6px var(--live)}
.dot.idle{background:var(--idle)}
.sid{color:var(--acc);font-weight:600}
.hz{font-size:10px;letter-spacing:.5px;border:1px solid var(--line);
border-radius:3px;padding:0 4px;color:var(--dim);text-transform:uppercase}
.model{background:color-mix(in srgb,var(--acc) 13%,transparent);
border:1px solid color-mix(in srgb,var(--acc) 40%,transparent);color:var(--acc);
padding:1px 7px;border-radius:10px;white-space:nowrap}
.model.sm{background:color-mix(in srgb,var(--sub) 13%,transparent);
border-color:color-mix(in srgb,var(--sub) 40%,transparent);color:var(--sub)}
.pill{border:1px solid var(--line);padding:1px 7px;border-radius:10px;
color:var(--dim);white-space:nowrap}
.pill.warn{border-color:var(--warn);color:var(--warn)}
.pill.on{border-color:var(--live);color:var(--live)}
.pill.cmp{border-color:var(--sub);color:var(--sub)}
.run{color:var(--live)}
.grow{flex:1 1 auto}
table{width:100%;border-collapse:collapse}
th,td{text-align:left;padding:5px 12px;border-top:1px solid var(--line);
vertical-align:top;white-space:nowrap}
th{color:var(--dim);font-weight:400;font-size:10px;text-transform:uppercase;letter-spacing:.5px}
td.t{white-space:normal;color:var(--dim);max-width:460px;font-size:12px}
.num{text-align:right;font-variant-numeric:tabular-nums}
.sw{color:var(--warn)}
.empty{padding:9px 12px;color:var(--dim)}
code{background:var(--bg);border:1px solid var(--line);border-radius:3px;padding:0 4px}
button,select{background:var(--panel);color:var(--fg);border:1px solid var(--line);
border-radius:4px;padding:3px 9px;cursor:pointer;font:inherit;font-size:12px}
.tl{padding:0 12px 7px;font-size:12px;color:var(--dim)}
</style></head><body>
<header>
  <h1>SYBIL'S ALCOVE</h1>
  <span class="muted" id="stat">loading…</span>
  <span class="grow"></span>
  <select id="filter">
    <option value="live">live only</option>
    <option value="active">active subagents</option>
    <option value="subs">has subagents</option>
    <option value="all" selected>all sessions</option>
  </select>
  <button id="expand">expand all</button>
  <button id="collapse">collapse all</button>
  <label class="muted"><input type="checkbox" id="auto" checked> auto 3s</label>
  <button id="now">refresh</button>
</header>
<div class="wrap" id="out"></div>
<script>
const K = n => n == null ? '—' :
  n >= 1e9 ? (n/1e9).toFixed(2)+'B' : n >= 1e6 ? (n/1e6).toFixed(2)+'M' :
  n >= 1e3 ? (n/1e3).toFixed(1)+'k' : String(n);
const AGE = s => s == null ? '—' : s < 60 ? Math.round(s)+'s' :
  s < 3600 ? Math.round(s/60)+'m' : s < 86400 ? (s/3600).toFixed(1)+'h' :
  (s/86400).toFixed(1)+'d';
const esc = t => (t==null?'':String(t)).replace(/[&<>"]/g,
  c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const HHMM = at => at && at.length > 18 ? at.slice(11,19)+'Z' : '';

// Collapse state lives outside the render so a 3s refresh cannot reopen what
// you closed. Persisted so it survives a reload too.
const LS = 'alcove.collapsed';
let collapsed = new Set(JSON.parse(localStorage.getItem(LS) || '[]'));
const saveCollapsed = () =>
  localStorage.setItem(LS, JSON.stringify([...collapsed]));

function toggle(id, el){
  if(collapsed.has(id)){ collapsed.delete(id); el.classList.add('open'); }
  else { collapsed.add(id); el.classList.remove('open'); }
  saveCollapsed();
}

function timelineHTML(t){
  if(!t || t.length < 2) return '';
  return '<div class="tl">switched: ' + t.map(x =>
    '<span class="sw">'+esc(x.model)+'</span>'+(x.at?' <span class="muted">'+HHMM(x.at)+'</span>':'')
  ).join(' <span class="muted">→</span> ') + '</div>';
}

// Compaction is the one event that invalidates every token total above it, so
// it gets its own line rather than hiding in a tooltip.
function compactHTML(s){
  const c = s.compactions;
  if(!c || !c.length) return '';
  return '<div class="tl">compacted: ' + c.map(x =>
    '<span class="sw">'+HHMM(x.at)+'</span>'
    + (x.trigger?' <span class="muted">'+esc(x.trigger)+'</span>':'')
    + (x.pre_tokens?' <span class="muted">context was '+K(x.pre_tokens)+'</span>':'')
  ).join(' <span class="muted">→</span> ')
  + ' <span class="muted">· totals below span the boundary</span></div>';
}

// `status` is the parent's launch record. It reads `async_launched` for every
// backgrounded subagent and never flips to completed, so it cannot mean "done".
// Only `completed` is terminal; otherwise the child transcript's mtime is the
// only honest signal, and an idle one may be finished or abandoned.
function STATE(s){
  if(s.no_transcript) return '<span class="pill warn">no transcript</span>';
  if(s.live) return '<span class="run">running</span>';
  if(s.status === 'completed') return '<span class="muted">done</span>';
  return '<span class="muted" title="launched in the background with no '
    + 'completion record; transcript has been idle">idle</span>';
}

function subTable(subs){
  if(!subs.length) return '<div class="empty">no subagents</div>';
  let h = '<table><tr><th>subagent</th><th>role</th><th>model</th><th>state</th>'
        + '<th class="num">turns</th><th class="num">out</th><th class="num">in</th>'
        + '<th class="num">cache rd</th><th class="num">age</th><th>task</th></tr>';
  for(const s of subs){
    const mism = s.record_model && s.model && s.record_model !== s.model;
    h += '<tr>'
      + '<td><span class="dot '+(s.live?'live':'idle')+'" '
      +   'style="display:inline-block;margin-right:6px"></span><code>'+esc(s.label)+'</code></td>'
      + '<td class="muted">'+esc(s.role||'—')+'</td>'
      + '<td><span class="model sm">'+esc(s.model||'unknown')+'</span>'
      +   (mism?' <span class="pill warn">rec '+esc(s.record_model)+'</span>':'')
      +   (s.timeline&&s.timeline.length>1?' <span class="pill warn">'+(s.timeline.length-1)+' sw</span>':'')+'</td>'
      + '<td>'+STATE(s)+'</td>'
      + '<td class="num">'+K(s.turns)+'</td>'
      + '<td class="num">'+K(s.usage.output)+'</td>'
      + '<td class="num">'+K(s.usage.input)+'</td>'
      + '<td class="num muted">'+K(s.usage.cache_read)+'</td>'
      + '<td class="num muted">'+AGE(s.age_s)+'</td>'
      + '<td class="t">'+esc(s.task||'')+'</td></tr>';
  }
  return h + '</table>';
}

let last = '';
function render(d){
  const mode = document.getElementById('filter').value;
  let list = d.sessions;
  if(mode === 'live') list = list.filter(s => s.live);
  if(mode === 'active') list = list.filter(s => s.subagents.some(x => x.live));
  if(mode === 'subs') list = list.filter(s => s.subagents.length);

  const liveN = d.sessions.filter(s=>s.live).length;
  const subs = d.sessions.reduce((a,s)=>a+s.subagents.length,0);
  const subsLive = d.sessions.reduce((a,s)=>a+s.subagents.filter(x=>x.live).length,0);
  document.getElementById('stat').textContent =
    d.generated_at+' · '+liveN+' live / '+d.sessions.length+' sessions · '
    +subsLive+' live / '+subs+' subagents';

  let h = '';
  for(const s of list){
    const models = new Set(s.subagents.filter(x=>x.model).map(x=>x.model));
    const mixed = s.model && [...models].some(m => m !== s.model);
    const act = s.subagents.filter(x=>x.live).length;
    const since = s.usage_since_compact;
    const open = collapsed.has(s.session_id) ? '' : ' open';
    h += '<div class="s'+open+'" data-id="'+esc(s.session_id)+'">'
      + '<div class="shead"><span class="caret"></span>'
      + '<span class="dot '+(s.live?'live':'idle')+'"></span>'
      + '<span class="hz">'+esc(s.harness)+'</span>'
      + '<span class="sid">'+esc(s.label)+'</span>'
      + '<span class="model">'+esc(s.model||'unknown')+'</span>'
      + (s.switches?'<span class="pill warn">'+s.switches+' switch'+(s.switches>1?'es':'')+'</span>':'')
      + (s.effort?'<span class="pill">'+esc(s.effort)+'</span>':'')
      + '<span class="pill">'+esc(s.project)+'</span>'
      + (s.branch?'<span class="pill">'+esc(s.branch)+'</span>':'')
      + '<span class="grow"></span>'
      + (s.subagents.length?'<span class="pill'+(act?' on':mixed?' warn':'')+'">'
          +(act?act+' active / ':'')+s.subagents.length+' sub'
          +(models.size>1?' · '+models.size+' models':'')+'</span>':'')
      + (s.compactions&&s.compactions.length?'<span class="pill cmp" title="context '
          +'compacted; token totals here span the boundary">compacted '
          +HHMM(s.compactions[s.compactions.length-1].at)
          +(s.compactions.length>1?' ×'+s.compactions.length:'')+'</span>':'')
      + '<span class="pill"'+(since?' title="since last compaction / tail total"':'')
          +'>out '+(since?K(since.output)+' / ':'')+K(s.usage.output)+'</span>'
      + '<span class="pill"'+(s.turns_since_compact!=null
          ?' title="since last compaction / tail total"':'')+'>'
          +(s.turns_since_compact!=null?s.turns_since_compact+' / ':'')
          +s.turns+' turns</span>'
      + (s.pids.length?'<span class="pill">pid '+s.pids.join(',')+'</span>':'')
      + '<span class="muted">'+AGE(s.age_s)+'</span>'
      + '</div><div class="body">'+timelineHTML(s.timeline)+compactHTML(s)
      + subTable(s.subagents)+'</div></div>';
  }
  h = h || '<p class="muted">no sessions match this filter</p>';
  // Only touch the DOM when something actually changed, so a refresh mid-scroll
  // or mid-click does not yank the page.
  if(h !== last){
    document.getElementById('out').innerHTML = h;
    last = h;
    for(const el of document.querySelectorAll('.s')){
      el.querySelector('.shead').addEventListener('click',
        () => toggle(el.dataset.id, el));
    }
  }
}

let data = null, timer = null;
async function load(){
  try{
    const r = await fetch('/api/sessions', {cache:'no-store'});
    data = await r.json();
    render(data);
  }catch(e){ document.getElementById('stat').textContent = 'error: '+e; }
}
function arm(){
  if(timer) clearInterval(timer);
  if(document.getElementById('auto').checked) timer = setInterval(load, 3000);
}
document.getElementById('auto').addEventListener('change', arm);
document.getElementById('now').addEventListener('click', load);
document.getElementById('filter').addEventListener('change', () => { last=''; render(data); });
document.getElementById('expand').addEventListener('click', () => {
  collapsed.clear(); saveCollapsed(); last=''; render(data); });
document.getElementById('collapse').addEventListener('click', () => {
  for(const s of data.sessions) collapsed.add(s.session_id);
  saveCollapsed(); last=''; render(data); });
load(); arm();
</script></body></html>
"""


LOGIN = """<!doctype html><html><head><meta charset="utf-8"><title>alcove</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{background:#0d1117;color:#e6edf3;font:14px/1.6 ui-monospace,
SFMono-Regular,Menlo,monospace;display:grid;place-items:center;height:100vh;margin:0}
form{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:24px;
min-width:min(380px,90vw)}
h1{font-size:14px;letter-spacing:1px;margin:0 0 4px}
p{color:#8b949e;font-size:12px;margin:0 0 16px}
input{width:100%;background:#0d1117;border:1px solid #30363d;border-radius:5px;
color:#e6edf3;padding:9px;font:inherit;margin-bottom:12px}
input:focus{outline:none;border-color:#58a6ff}
button{width:100%;background:#238636;border:1px solid #2ea043;color:#fff;
border-radius:5px;padding:9px;font:inherit;cursor:pointer}
.err{color:#f85149;font-size:12px;margin:12px 0 0}
code{color:#8b949e}</style></head><body>
<form method="POST" action="/login" autocomplete="off">
<h1>SYBIL'S ALCOVE</h1>
<p>Token required. Value is in <code>/etc/alcove/env</code>.</p>
<input type="password" name="token" placeholder="ALCOVE_TOKEN" autofocus
  autocomplete="current-password" spellcheck="false">
<button type="submit">unlock</button>
__ERR__
</form></body></html>"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "alcove"

    def _send(self, body: bytes, ctype: str, status: int = 200,
              cookie: str | None = None) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        if cookie:
            # HttpOnly so page scripts cannot read it back out; SameSite=Strict
            # so another origin cannot ride the cookie. No Secure flag: the
            # overlay is plain HTTP.
            self.send_header(
                "Set-Cookie",
                f"{COOKIE}={cookie}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800")
        self.end_headers()
        self.wfile.write(body)

    def _supplied_token(self) -> str:
        """Bearer header for scripts, cookie for browsers.

        Deliberately no `?token=` support: a secret in a URL lands in browser
        history, screenshots, referers, and shell history.
        """
        auth = self.headers.get("Authorization", "")
        if auth.startswith("Bearer "):
            return auth[len("Bearer "):].strip()
        for part in self.headers.get("Cookie", "").split(";"):
            name, _, value = part.strip().partition("=")
            if name == COOKIE:
                return value
        return ""

    def _login_page(self, error: str = "", status: int = 200) -> None:
        body = LOGIN.replace(
            "__ERR__", f'<p class="err">{error}</p>' if error else "")
        self._send(body.encode(), "text/html; charset=utf-8", status=status)

    def do_POST(self) -> None:  # noqa: N802
        if self.path.split("?", 1)[0] != "/login":
            self.send_error(404)
            return
        length = min(int(self.headers.get("Content-Length") or 0), 4096)
        raw = self.rfile.read(length).decode("utf-8", errors="replace") if length else ""
        from urllib.parse import parse_qs
        supplied = (parse_qs(raw).get("token") or [""])[0]
        if not token_ok(supplied):
            self._login_page("rejected", status=401)
            return
        # 303 so the browser re-requests with GET and the POST body is not
        # replayed on refresh.
        self.send_response(303)
        self.send_header("Location", "/")
        self.send_header(
            "Set-Cookie",
            f"{COOKIE}={supplied}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self) -> None:  # noqa: N802
        route = self.path.split("?", 1)[0]
        # Loopback is trusted; anything wider requires the shared secret.
        if not is_local_bind() and not token_ok(self._supplied_token()):
            if route == "/api/sessions":
                self._send(b'{"error":"unauthorized"}', "application/json", status=401)
            else:
                self._login_page(status=401)
            return

        if route in ("/", "/index.html", "/login"):
            self._send(PAGE.encode(), "text/html; charset=utf-8")
        elif route == "/api/sessions":
            self._send(json.dumps(cached()).encode(), "application/json")
        else:
            self.send_error(404)

    def log_message(self, *args: Any) -> None:
        return  # a poll every 3s would drown the console


def main() -> int:
    if not CLAUDE_ROOT.is_dir() and not CODEX_ROOT.is_dir():
        print(f"no transcripts found under {CLAUDE_ROOT} or {CODEX_ROOT}")
        return 1
    # Fail closed: this page shows task prompts, so a non-loopback bind without
    # a token is a mistake, not a default worth honouring.
    if not is_local_bind() and not TOKEN:
        print(f"refusing to serve {BIND}:{PORT} without ALCOVE_TOKEN.\n"
              f"  generate one:  python3 -c 'import secrets;print(secrets.token_urlsafe(32))'\n"
              f"  then:          ALCOVE_TOKEN=<token> ALCOVE_BIND={BIND} python3 alcove.py\n"
              f"  or bind loopback: ALCOVE_BIND=127.0.0.1 python3 alcove.py")
        return 2
    print(f"alcove: http://{BIND}:{PORT}")
    print(f"  claude: {CLAUDE_ROOT if CLAUDE_ROOT.is_dir() else '(absent)'}")
    print(f"  codex:  {CODEX_ROOT if CODEX_ROOT.is_dir() else '(absent)'}")
    print(f"  auth:   {'token required' if not is_local_bind() else 'loopback (none)'}")
    ThreadingHTTPServer((BIND, PORT), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        pass
