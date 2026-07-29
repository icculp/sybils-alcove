"""Claude Code transcripts.

    ~/.claude/projects/<project>/<session-id>.jsonl          main thread
    ~/.claude/projects/<project>/<session-id>/subagents/
        agent-<agentId>.jsonl                                one per subagent

Every child entry is `isSidechain: true`, so a parent's own totals must skip
sidechain events or it absorbs its subagents' work.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .. import config
from ..model import (MODEL_ARGS_RE, MODEL_SET_RE, add_anthropic_usage,
                     clean_model_name, event_text, is_real_model, live_first,
                     new_usage, push_model, push_selection)
from ..transcripts import file_size, chronological, mtime_age, tail_events


def scan_claude(path: Path, *, main_thread_only: bool) -> dict[str, Any]:
    timeline: list[dict[str, str]] = []
    turn_rows: list[dict[str, Any]] = []
    selections: list[dict[str, str]] = []
    pending_args = ""
    usage = new_usage()
    ctx_usage = new_usage()
    seen_msgs: set[str] = set()
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
        if event.get("type") == "user":
            # The command and its resolved output are two events sharing a
            # timestamp, so remember the requested alias ("opus[1m]") until the
            # resolved name ("claude-opus-5[1m]") arrives on the next one.
            text = event_text(event)
            if "/model" in text:
                asked = MODEL_ARGS_RE.search(text)
                if asked:
                    pending_args = asked.group(1).strip()
            resolved = MODEL_SET_RE.search(text)
            if resolved:
                name = clean_model_name(resolved.group(1))
                if name:
                    push_selection(selections, name, ts, pending_args)
                pending_args = ""
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
        # One logical turn writes SEVERAL assistant events (one per content
        # block), each repeating the same message.id AND the same usage dict.
        # Counting per event overstated one real session's turns 1757 vs 761
        # and its output tokens 2.18M vs 0.82M — dedupe by message.id. An event
        # without an id (rare) still counts once on its own.
        msg_id = str(message.get("id") or "")
        first = msg_id not in seen_msgs
        if msg_id:
            seen_msgs.add(msg_id)
        if first:
            add_anthropic_usage(usage, message.get("usage"))
            add_anthropic_usage(ctx_usage, message.get("usage"))
        model = message.get("model")
        if not is_real_model(model):
            continue
        if first:
            turns += 1
            ctx_turns += 1
            # One row per real turn, for the store. Keyed by message.id so a
            # re-scan of overlapping windows is a no-op; when an event carries no
            # id, path+timestamp is still stable across re-scans.
            u = message.get("usage") or {}
            turn_rows.append({
                "id": msg_id or f"{path.name}:{ts}",
                "ts": ts, "model": str(model),
                "input": int(u.get("input_tokens") or 0),
                "output": int(u.get("output_tokens") or 0),
                "cache_read": int(u.get("cache_read_input_tokens") or 0),
                "cache_write": int(u.get("cache_creation_input_tokens") or 0),
            })
        push_model(timeline, str(model), ts)
    return {
        "timeline": timeline, "model": timeline[-1]["model"] if timeline else "",
        # Per-turn rows are for the store only and are stripped from the API
        # payload; usage on a `<synthetic>`-model message is counted in the
        # totals above but has no row, since it is not a turn.
        "turn_rows": turn_rows,
        # What the operator chose, vs what actually served a turn. These are
        # different facts and a mismatch is meaningful, so keep both.
        "selections": selections,
        "selected_model": selections[-1]["model"] if selections else "",
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
    root = config.CLAUDE_ROOT
    if not root.is_dir():
        return []
    sessions = []
    for project in sorted(p for p in root.iterdir() if p.is_dir()):
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
                    "age_s": age,
                    "live": age is not None and age < config.LIVE_WINDOW_S,
                    "size": file_size(child),
                    "_turn_rows": child_info["turn_rows"],
                    "_path": str(child),
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
                    "role": record.get("agent_type", ""),
                    "status": record.get("status", ""),
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
                "selections": info["selections"],
                "selected_model": info["selected_model"],
                "timeline": info["timeline"], "usage": info["usage"],
                "turns": info["turns"], "last_ts": info["last_ts"], "age_s": age,
                "live": age is not None and age < config.LIVE_WINDOW_S,
                "compactions": info["compactions"],
                "usage_since_compact": info["usage_since_compact"],
                "turns_since_compact": info["turns_since_compact"],
                "subagents": subs, "path": str(transcript),
                "_turn_rows": info["turn_rows"],
            })
    return sessions
