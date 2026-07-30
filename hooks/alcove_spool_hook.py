#!/usr/bin/env python3
"""Observation-only tool-call hook: append one JSON line per tool event.

Reads a Claude Code or Codex PreToolUse/PostToolUse hook payload on stdin and
appends a single capped line to an append-only spool a separate ingester reads.

The only failure mode this is allowed to have is "a line is missing". It makes
no decisions, blocks nothing, opens no socket, and always exits 0 -- a hook that
exits nonzero or hangs damages every live session on the machine. See
hooks/README.md for the frozen spool contract and the observed payload shapes.

python3 stdlib only.
"""

import sys

SPOOL_DIR = "/root/.local/state/alcove/spool"

SCHEMA_VERSION = 1
MAX_LINE = 2048  # hard cap, bytes
MAX_ARG = 500  # hard cap, chars
MAX_TARGET = 500
MAX_FIELD = 200  # tool / session_id / tool_use_id

# tool_input keys whose value may be spooled as `arg`. A whitelist, not
# "the first key": `content`, `new_string`, `prompt`, and `todos` carry file and
# message bodies, and must never reach the spool.
ARG_KEYS = (
    "command",  # claude Bash, Monitor
    "cmd",  # codex exec_command
    "pattern",  # Glob, Grep
    "query",  # WebSearch, mcp recall/search
    "search_query",  # codex run
    "url",  # WebFetch
    "skill",  # Skill
    "file_path",  # Read, Write, Edit
    "notebook_path",
    "path",  # gbrain read/write, codex view_image
    "filePath",
    "description",  # Agent -- the label, deliberately not `prompt`
)

# tool_input keys that name a filesystem/URL target.
TARGET_KEYS = (
    "file_path",
    "notebook_path",
    "filePath",
    "path",
    "url",
)


def _redact(text):
    """Blunt credential scrub. Over-redaction is safe; under-redaction is not."""
    import re

    secretish = r"(?:passw|secret|token|key|credential|bearer|auth|session)"

    # A quoted run that mentions anything secret-ish goes whole. This is what
    # catches `-H "X-ClickHouse-Key: ..."` and `-H 'Authorization: Bearer ...'`,
    # where the key sits *inside* the quotes so a key=value rule would only eat
    # the first word of the value and leave the rest of the token on the line.
    text = re.sub(
        r"(?i)(['\"])[^'\"]*" + secretish + r"[^'\"]*\1",
        "[redacted]",
        text,
    )
    # Unquoted key=value / key: value where the key looks secret-ish.
    # Deliberately broad: `--sort-key=name` losing its value is a fine price for
    # never spooling a bearer token out of a curl line.
    text = re.sub(
        r"(?i)([\w.-]*" + secretish + r"[\w.-]*)\s*[:=]\s*(\"[^\"]*\"|'[^']*'|\S+)",
        r"\1=[redacted]",
        text,
    )
    # Bare `Bearer <token>` / `Basic <blob>` with no key= in front of it.
    text = re.sub(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{6,}", r"\1 [redacted]", text)
    # URL userinfo: postgresql://user:pw@host, https://tok@host.
    text = re.sub(r"(://[^/\s:@]+):[^/\s@]+@", r"\1:[redacted]@", text)
    # user:pass passed to a --user style flag.
    text = re.sub(
        r"(?i)(--user|--username|--password|-u)([ =])([^\s:]+):\S+", r"\1\2\3:[redacted]", text
    )
    # Known credential prefixes.
    text = re.sub(
        r"(?i)\b(sk|ghp|gho|ghu|ghs|ghr|github_pat|xox[abprs]|hf|pat|glpat|AKIA)"
        r"[-_][A-Za-z0-9_-]{8,}",
        "[redacted]",
        text,
    )
    # Long opaque runs: base64/hex blobs. Paths survive (they contain / and .).
    text = re.sub(r"\b[A-Za-z0-9_-]{40,}\b", "[redacted]", text)
    return text


def _clean_url(value):
    """Keep scheme://host/path; drop query and fragment (they carry tokens)."""
    for sep in ("?", "#"):
        cut = value.find(sep)
        if cut != -1:
            value = value[:cut]
    return value


def _str(value, cap):
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value:
        return None
    # Collapse newlines/tabs: one JSON line per event, and a head is a head.
    value = " ".join(value.split())
    return value[:cap] if len(value) > cap else value


def _apply_patch_header(text):
    """apply_patch's tool_input is the raw patch -- a file body. Take only the
    verb+path header line and never a byte of the payload."""
    for line in text.split("\n", 40)[:40]:
        line = line.strip()
        if line.startswith("*** ") and " File:" in line:
            return line
    return None


def _extract(tool_input):
    """-> (target, arg). Never returns a file/message body."""
    if isinstance(tool_input, dict):
        target = None
        for key in TARGET_KEYS:
            value = _str(tool_input.get(key), MAX_TARGET)
            if value:
                target = _clean_url(value) if key == "url" else value
                break
        arg = None
        for key in ARG_KEYS:
            value = tool_input.get(key)
            if isinstance(value, list):  # codex cmd is occasionally argv
                value = " ".join(str(part) for part in value)
            value = _str(value, MAX_ARG)
            if value:
                arg = _clean_url(value) if key == "url" else value
                break
        return target, arg

    if isinstance(tool_input, str):
        # A bare-string tool_input is a Codex custom_tool_call: `apply_patch`
        # (a patch body) or `exec` (a script body). Only apply_patch has a
        # header safe to spool; everything else yields nulls by design.
        header = _apply_patch_header(tool_input)
        if header:
            path = header.split(" File:", 1)[1].strip() or None
            return _str(path, MAX_TARGET), _str(header, MAX_ARG)
        return None, None

    return None, None


def _determine_ok(payload):
    """post events only. None means "not cheaply determinable" -- which is a
    different answer from False, and must stay different.

    Observed: neither harness emits PostToolUse for a tool that errored or was
    denied, so a structured response with no failure marker is a success. A
    string response is opaque and we refuse to read it, so it stays None.
    """
    if "tool_response" not in payload:
        return None
    response = payload["tool_response"]
    if isinstance(response, bool):
        return response
    if isinstance(response, dict):
        for key in ("is_error", "isError", "error"):
            if response.get(key):  # truthiness only; the value is never spooled
                return False
        if response.get("interrupted") is True:
            return False
        for key in ("success", "ok"):
            if isinstance(response.get(key), bool):
                return response[key]
        for key in ("exit_code", "exitCode"):
            if isinstance(response.get(key), int):
                return response[key] == 0
        return True
    return None


def _harness(payload):
    for i, arg in enumerate(sys.argv):
        if arg == "--harness" and i + 1 < len(sys.argv):
            name = sys.argv[i + 1]
            if name in ("claude", "codex"):
                return name
    # Fallback sniff: turn_id is required in Codex's schema and absent from
    # Claude Code's payload (which sends prompt_id instead).
    return "codex" if "turn_id" in payload else "claude"


def _timestamp():
    import datetime

    now = datetime.datetime.now(datetime.timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + "%03dZ" % (now.microsecond // 1000), now


def main():
    import json
    import os

    payload = json.loads(sys.stdin.read())
    if not isinstance(payload, dict):
        return

    event_name = payload.get("hook_event_name")
    event = {"PreToolUse": "pre", "PostToolUse": "post"}.get(event_name)
    if event is None:
        return  # not ours; say nothing

    harness = _harness(payload)
    ts, now = _timestamp()
    target, arg = _extract(payload.get("tool_input"))
    if arg:
        arg = _redact(arg)[:MAX_ARG]
    if target:
        target = _redact(target)[:MAX_TARGET]

    record = {
        "v": SCHEMA_VERSION,
        "ts": ts,
        "harness": harness,
        "event": event,
        "session_id": _str(payload.get("session_id"), MAX_FIELD) or "",
        "tool": _str(payload.get("tool_name"), MAX_FIELD) or "",
        "cwd": _str(payload.get("cwd"), MAX_TARGET),
        "target": target,
        "arg": arg,
        "ok": _determine_ok(payload) if event == "post" else None,
        "tool_use_id": _str(payload.get("tool_use_id"), MAX_FIELD),
    }

    def encode(rec):
        return json.dumps(rec, ensure_ascii=True, separators=(",", ":")).encode() + b"\n"

    line = encode(record)
    if len(line) > MAX_LINE:
        # The cap is a cap. Shed the free-form fields, longest first, rather
        # than write an over-long line or drop the event.
        for field in ("arg", "target", "cwd"):
            budget = MAX_LINE - (len(line) - len(record[field] or ""))
            if record[field] and budget > 16:
                record[field] = record[field][: budget - 16]
            else:
                record[field] = None
            line = encode(record)
            if len(line) <= MAX_LINE:
                break
    if len(line) > MAX_LINE:
        return  # pathological; a missing line is the allowed failure

    os.makedirs(SPOOL_DIR, exist_ok=True)
    path = os.path.join(SPOOL_DIR, "%s-%s.jsonl" % (harness, now.strftime("%Y%m%d")))
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, line)  # one O_APPEND write, <= 2KB
    finally:
        os.close(fd)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        pass
    sys.exit(0)
