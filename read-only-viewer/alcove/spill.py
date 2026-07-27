"""Spillout: the recent event stream of one session, normalised across harnesses.

This is the "what is it actually doing right now" view — assistant messages, the
tool calls with their arguments, and what came back.

What it deliberately does NOT show is reasoning. Both harnesses persist a
reasoning record with the text stripped: Claude writes a `thinking` block
carrying only a `signature`, Codex a `reasoning` item carrying only
`encrypted_content`. Measured across this corpus, 22,669 Claude thinking blocks
and 22,428 Codex reasoning items contained text in exactly zero cases. So the
stream emits a `reasoning` marker with no body — the model thought here, and the
content is not on disk. Rendering nothing at all would imply it never thought;
inventing a summary would be a lie.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .collect import cached
from .transcripts import chronological, tail_events

# Per-event text cap. Tool results run to megabytes; the browser wants a peek,
# not the payload. Truncation is always flagged so a cut never reads as the end.
MAX_TEXT = 4000
MAX_ARG = 600
DEFAULT_LIMIT = 300


def _clip(text: str, limit: int = MAX_TEXT) -> tuple[str, bool]:
    text = text.replace("\r\n", "\n")
    if len(text) <= limit:
        return text, False
    return text[:limit], True


def _shrink(value: Any, depth: int = 0) -> Any:
    """Truncate long strings inside tool arguments but keep the structure.

    A Write call's `content` is the whole file; flattening the dict to a clipped
    JSON string would hide which parameters were even passed. Keys survive,
    values get cut.
    """
    if isinstance(value, str):
        return value[:MAX_ARG] + ("…" if len(value) > MAX_ARG else "")
    if isinstance(value, dict) and depth < 4:
        return {k: _shrink(v, depth + 1) for k, v in list(value.items())[:40]}
    if isinstance(value, list) and depth < 4:
        return [_shrink(v, depth + 1) for v in value[:20]]
    return value


def _ts_epoch(ts: str) -> float | None:
    if not ts:
        return None
    try:
        cleaned = ts.replace("Z", "+00:00")
        parsed = datetime.fromisoformat(cleaned)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.timestamp()
    except ValueError:
        return None


def _blocks_text(content: Any) -> str:
    """Flatten a content list to text, dropping images.

    A transcript image block is an inline base64 PNG — hundreds of kilobytes
    that would dwarf every other event in the payload.
    """
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for block in content:
        if not isinstance(block, dict):
            parts.append(str(block))
            continue
        kind = block.get("type")
        if kind == "image":
            parts.append("[image]")
        elif kind == "tool_reference":
            parts.append(f"[tool: {block.get('tool_name', '')}]")
        elif block.get("text") is not None:
            parts.append(str(block.get("text")))
    return "\n".join(p for p in parts if p)


def _event(kind: str, ts: str, **rest: Any) -> dict[str, Any]:
    out = {"kind": kind, "ts": ts}
    out.update(rest)
    return out


def _spill_claude(path: Path) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for event in chronological(tail_events(path)):
        etype = event.get("type")
        ts = str(event.get("timestamp") or "")
        if etype == "system" and event.get("subtype") == "compact_boundary":
            out.append(_event("compact", ts))
            continue
        message = event.get("message")
        if not isinstance(message, dict) or etype not in ("user", "assistant"):
            continue
        model = str(message.get("model") or "")
        content = message.get("content")
        blocks = content if isinstance(content, list) else [
            {"type": "text", "text": content}]
        for block in blocks:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype == "thinking":
                # Signature only; see the module docstring.
                out.append(_event("reasoning", ts, model=model))
            elif btype == "text":
                text, cut = _clip(str(block.get("text") or ""))
                if text.strip():
                    out.append(_event(
                        "assistant" if etype == "assistant" else "user",
                        ts, text=text, truncated=cut, model=model))
            elif btype == "tool_use":
                out.append(_event("tool_use", ts, name=str(block.get("name") or ""),
                                  tool_id=str(block.get("id") or ""),
                                  args=_shrink(block.get("input")), model=model))
            elif btype == "tool_result":
                text, cut = _clip(_blocks_text(block.get("content")))
                out.append(_event("tool_result", ts, text=text, truncated=cut,
                                  tool_id=str(block.get("tool_use_id") or ""),
                                  error=bool(block.get("is_error"))))
    return out


def _spill_codex(path: Path) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for event in chronological(tail_events(path)):
        kind = event.get("type")
        payload = event.get("payload")
        ts = str(event.get("timestamp") or "")
        if not isinstance(payload, dict):
            continue
        ptype = payload.get("type")
        if kind == "compacted" or ptype == "context_compacted":
            # One compaction is written twice, milliseconds apart, as a
            # `compacted` record and an `event_msg`. Compare at second
            # granularity: two real compactions in one second is not a thing.
            if not (out and out[-1]["kind"] == "compact"
                    and out[-1]["ts"][:19] == ts[:19]):
                out.append(_event("compact", ts))
        elif kind != "response_item":
            continue
        elif ptype == "reasoning":
            out.append(_event("reasoning", ts))
        elif ptype == "message":
            role = str(payload.get("role") or "")
            # `developer` is the injected system preamble, re-sent every turn.
            # It is not commentary and would swamp the stream.
            if role not in ("assistant", "user"):
                continue
            text, cut = _clip(_blocks_text(payload.get("content")))
            if text.strip():
                out.append(_event(role, ts, text=text, truncated=cut))
        elif ptype in ("function_call", "custom_tool_call", "local_shell_call"):
            # Codex serialises arguments as a JSON *string*; parse it so the
            # viewer can show fields, and fall back to the raw text if it is not
            # the JSON it claims to be.
            raw = payload.get("arguments")
            args: Any
            try:
                args = json.loads(raw) if isinstance(raw, str) else raw
            except (json.JSONDecodeError, ValueError):
                args = {"arguments": raw}
            out.append(_event("tool_use", ts, name=str(payload.get("name") or ptype),
                              tool_id=str(payload.get("call_id") or ""),
                              args=_shrink(args)))
        elif ptype in ("function_call_output", "custom_tool_call_output"):
            body = payload.get("output")
            if isinstance(body, dict):
                body = body.get("content") or json.dumps(body)
            text, cut = _clip(str(body or ""))
            out.append(_event("tool_result", ts, text=text, truncated=cut,
                              tool_id=str(payload.get("call_id") or ""),
                              error=False))
        elif ptype == "tool_search_call":
            out.append(_event("tool_use", ts, name="tool_search",
                              tool_id="", args=_shrink(payload.get("queries")
                                                       or payload.get("query"))))
    return out


def _index() -> dict[tuple[str, str], dict[str, Any]]:
    """Session/agent id -> transcript path, from the collected snapshot.

    The client sends ids, never paths. Resolving through the snapshot means an
    unknown id is simply absent rather than a filesystem read, so no request can
    reach a file the collector did not already choose to open.
    """
    out: dict[tuple[str, str], dict[str, Any]] = {}
    for session in cached()["sessions"]:
        sid = session["session_id"]
        out[(sid, "")] = {
            "path": session.get("path"), "harness": session["harness"],
            "label": session.get("label", ""), "model": session.get("model", ""),
            "cwd": session.get("cwd", ""), "project": session.get("project", ""),
            "state": session.get("state", ""),
        }
        for sub in session.get("subagents") or []:
            if not sub.get("_path"):
                continue
            out[(sid, sub["id"])] = {
                "path": sub["_path"], "harness": session["harness"],
                "label": sub.get("label", ""), "model": sub.get("model", ""),
                "cwd": session.get("cwd", ""), "project": session.get("project", ""),
                "state": "running" if sub.get("live") else "",
                "role": sub.get("role", ""), "task": sub.get("task", ""),
            }
    return out


def spill(session_id: str, agent_id: str = "", minutes: int = 0,
          limit: int = DEFAULT_LIMIT) -> dict[str, Any]:
    target = _index().get((session_id, agent_id))
    if not target or not target.get("path"):
        return {"error": "unknown session", "events": []}
    path = Path(str(target["path"]))
    reader = _spill_claude if target["harness"] == "claude" else _spill_codex
    events = reader(path)
    window = None
    if minutes > 0:
        cutoff = datetime.now(timezone.utc).timestamp() - minutes * 60
        window = minutes
        # An event with an unparseable timestamp is kept: dropping it would
        # silently hide activity, and a missing timestamp is not evidence of age.
        events = [e for e in events
                  if (_ts_epoch(e.get("ts", "")) or 1e18) >= cutoff]
    total = len(events)
    events = events[-limit:]
    return {
        "session_id": session_id, "agent_id": agent_id,
        "harness": target["harness"], "label": target.get("label", ""),
        "model": target.get("model", ""), "cwd": target.get("cwd", ""),
        "project": target.get("project", ""), "state": target.get("state", ""),
        "role": target.get("role", ""), "task": target.get("task", ""),
        "events": events, "shown": len(events), "matched": total,
        "window_minutes": window,
        # The tail window bounds this view exactly as it bounds the live one:
        # these are the last events in the file, not the whole session.
        "tail_bounded": True,
    }
