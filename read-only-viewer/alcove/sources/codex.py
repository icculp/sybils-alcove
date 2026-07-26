"""Codex rollout transcripts.

    ~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<id>.jsonl

A Codex subagent writes a full sibling transcript with its own thread id; the
link back is `parent_thread_id` in its `session_meta`.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .. import config
from ..model import is_real_model, live_first, new_usage, push_model
from ..transcripts import (chronological, file_size, head_events, mtime_age,
                           tail_events)


def scan_codex(path: Path) -> dict[str, Any]:
    """One Codex rollout file.

    Model/effort come from `turn_context`. Token totals come from the last
    `token_count`, whose `total_token_usage` is already cumulative for the
    session — summing them would multiply-count.
    """
    timeline: list[dict[str, str]] = []
    turn_rows: list[dict[str, Any]] = []
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
            # `turn_context` is written once per SESSION (and again on a model or
            # effort change) — not once per turn. Counting it as a turn reported
            # every Codex session and subagent as having taken exactly one.
            model = payload.get("model")
            if payload.get("effort"):
                effort = str(payload["effort"])
            if is_real_model(model):
                if ts and not last_ts:
                    last_ts = ts
                push_model(timeline, str(model), ts)
        elif (kind == "response_item" and payload.get("type") == "message"
                and payload.get("role") == "assistant"):
            # The real per-turn signal, and it agrees with the count of
            # `event_msg`/`agent_message` events on the same transcript.
            turns += 1
            ctx_turns += 1
            if ts:
                last_ts = ts
            # Codex token totals are cumulative session snapshots, so there is
            # no per-turn attribution to record — the columns stay NULL rather
            # than being filled with a number that would not mean what it says.
            turn_rows.append({
                "id": str(payload.get("id") or "") or f"{path.name}:{ts}",
                "ts": ts,
                "model": timeline[-1]["model"] if timeline else "",
                "input": None, "output": None,
                "cache_read": None, "cache_write": None,
            })
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
        "turn_rows": turn_rows,
        "usage": usage, "turns": turns, "last_ts": last_ts, "cwd": cwd,
        "effort": effort, "context_window": context_window,
        "compactions": compactions,
        "usage_since_compact": (
            {k: max(0, usage[k] - usage_at_compact.get(k, 0)) for k in usage}
            if usage_at_compact is not None else None),
        "turns_since_compact": ctx_turns if compactions else None,
    }


def collect_codex() -> list[dict[str, Any]]:
    """Codex sessions, with spawned agents nested under their parent."""
    root = config.CODEX_ROOT
    if not root.is_dir():
        return []
    # A Codex thread can span several rollout files (resume, rollback). Merge by
    # thread id, newest file wins for current model/effort, and keep the largest
    # cumulative token snapshot rather than summing — each is already a total.
    # NOTE: no thread in the corpus this was written against actually spanned
    # multiple files, so this merge path is effectively untested.
    merged: dict[str, dict[str, Any]] = {}
    for path in sorted(root.rglob("*.jsonl"),
                       key=lambda p: p.stat().st_mtime if p.exists() else 0):
        info = scan_codex(path)
        sid = info["session_id"]
        if not sid:
            continue
        age = mtime_age(path)
        info["path"] = path
        info["age_s"] = age
        info["live"] = age is not None and age < config.LIVE_WINDOW_S
        info["size"] = file_size(path)
        prior = merged.get(sid)
        if prior is None:
            merged[sid] = info
            continue
        prior["size"] += info["size"]
        prior["turns"] += info["turns"]
        prior["turn_rows"].extend(info["turn_rows"])
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
                "turns": child["turns"],
                "reported_tokens": child["usage"]["output"] or None,
                "tool_uses": None, "task": child["nickname"],
                "age_s": child["age_s"], "live": child["live"],
                "size": child["size"],
                "_turn_rows": child["turn_rows"],
            })
        subs.sort(key=live_first)
        sessions.append({
            "harness": "codex", "session_id": info["session_id"],
            # Codex thread ids are time-ordered, so two sessions started in the
            # same window share an 8-char prefix and read as one duplicated row.
            "label": info["session_id"][:13],
            "project": Path(info["cwd"]).name if info["cwd"] else "unknown",
            "cwd": info["cwd"], "branch": "", "effort": info["effort"],
            "model": info["model"], "timeline": info["timeline"],
            # Codex has no slash-command record; a model change emits its own
            # `turn_context`, so its served timeline already captures switches.
            "selections": [], "selected_model": "",
            "usage": info["usage"], "turns": info["turns"],
            "last_ts": info["last_ts"], "age_s": info["age_s"],
            "live": info["live"],
            "compactions": info["compactions"],
            "usage_since_compact": info["usage_since_compact"],
            "turns_since_compact": info["turns_since_compact"],
            "subagents": subs, "path": str(info["path"]),
            "_turn_rows": info["turn_rows"],
        })
    return sessions
