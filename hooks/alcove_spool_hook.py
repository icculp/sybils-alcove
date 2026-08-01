#!/usr/bin/env python3
"""Observation-only session hook: append one JSON line per tool or stop event.

Reads a Claude Code or Codex PreToolUse / PostToolUse / Stop / SubagentStop hook
payload on stdin and appends a single capped line to an append-only spool a
separate ingester reads.

The only failure mode this is allowed to have is "a line is missing". It makes
no decisions, blocks nothing, opens no socket, and always exits 0 -- a hook that
exits nonzero or hangs damages every live session on the machine. See
hooks/README.md for the frozen spool contract and the observed payload shapes.

python3 stdlib only.
"""

import sys

SPOOL_DIR = "/root/.local/state/alcove/spool"

# An ADDITIVE nullable field does not bump this. A reader that does not know the
# field ignores it; a reader that does treats absent as null, which is exactly
# what an old line means. Bumping v would make every current reader skip every
# new line -- counted as malformed -- to gain nothing.
SCHEMA_VERSION = 1

EVENT_MAP = {
    "PreToolUse": "pre",
    "PostToolUse": "post",
    # A stop is a state transition, not a tombstone: a later event for the same
    # session or child means it resumed. This records; the viewer interprets.
    "Stop": "stop",
    "SubagentStop": "subagent_stop",
}

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


def _command_launchers(command, depth=0):
    """Classify agent executables, including commands nested under a shell."""
    import os
    import re
    import shlex

    if depth > 4:
        return []

    wrappers = {"env", "sudo", "command", "nohup", "setsid", "timeout", "!"}
    segments = []
    current = []
    quote = None
    escaped = False
    for char in command:
        if escaped:
            current.append(char)
            escaped = False
        elif char == "\\":
            current.append(char)
            escaped = True
        elif quote:
            current.append(char)
            if char == quote:
                quote = None
        elif char in "'\"`":
            current.append(char)
            quote = char
        elif char in ";&|\n":
            if "".join(current).strip():
                segments.append("".join(current))
            current = []
        else:
            current.append(char)
    if "".join(current).strip():
        segments.append("".join(current))

    out = []
    for segment in segments:
        try:
            tokens = shlex.split(segment)
        except ValueError:
            tokens = segment.split()
        for i, raw in enumerate(tokens):
            token = raw.strip("'\"`(){}")
            if (
                not token
                or token in wrappers
                or token.startswith("-")
                or "=" in token
                or re.fullmatch(r"[0-9.]+[smh]?", token)
            ):
                continue
            base = os.path.basename(token).lower()
            rest = [part.strip("'\"`(){}") for part in tokens[i + 1 :]]
            if base in ("bash", "sh", "zsh", "dash"):
                for index, part in enumerate(rest):
                    if part.startswith("-") and "c" in part and index + 1 < len(rest):
                        for launcher in _command_launchers(rest[index + 1], depth + 1):
                            if launcher not in out:
                                out.append(launcher)
                        break
                break
            launcher = None
            if "codex-spark-triage" in base:
                launcher = "spark"
            elif base == "codex" or base.startswith("codex-") or base.endswith("-codex"):
                if base != "codex" or (rest and rest[0] in ("exec", "review")):
                    launcher = "codex"
            elif base == "claude" or "claude-agent" in base:
                if not any(part in ("--version", "agents", "mcp") for part in rest):
                    launcher = "claude"
            elif base == "hermes" or "hermes-agent" in base:
                launcher = "hermes"
            elif base in ("opencode", "aider") or "luna-agent" in base:
                launcher = base
            if launcher and launcher not in out:
                out.append(launcher)
            break
    return out


def _agent_launchers(tool, tool_input):
    """Return launcher names without retaining a command body or agent prompt."""
    import json
    import re

    if tool in ("Agent", "Task") or "spawn_agent" in tool.lower():
        if isinstance(tool_input, dict):
            role = tool_input.get("subagent_type") or tool_input.get("agent_type")
            if isinstance(role, str) and role.strip():
                return [role.strip()[:MAX_FIELD]]
        return ["native agent"]

    if tool not in ("exec", "functions.exec", "exec_command", "Bash", "bash", "shell"):
        return []

    candidates = []
    if isinstance(tool_input, dict):
        for key in ("cmd", "command"):
            value = tool_input.get(key)
            if isinstance(value, list):
                value = " ".join(str(part) for part in value)
            if isinstance(value, str):
                candidates.append(value)
    elif isinstance(tool_input, str):
        # Custom Codex exec is JavaScript. Mask string literals, then consider
        # only values passed to `cmd:`/`command:`. This excludes command examples
        # inside patch bodies and other data strings.
        matches = list(
            re.finditer(
                r'''(?s)"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`''',
                tool_input,
            )
        )
        literals = {}
        masked = []
        end = 0
        for index, match in enumerate(matches):
            masked.append(tool_input[end : match.start()])
            marker = "__ALCOVE_STR_%d__" % index
            masked.append(marker)
            raw = match.group(0)
            if raw.startswith('"'):
                try:
                    literals[marker] = json.loads(raw)
                except Exception:
                    literals[marker] = ""
            else:
                literals[marker] = raw[1:-1].replace("\\n", "\n")
            end = match.end()
        masked.append(tool_input[end:])
        masked = "".join(masked)
        variables = {}
        for match in re.finditer(
            r"\b(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(__ALCOVE_STR_[0-9]+__)",
            masked,
        ):
            variables[match.group(1)] = literals.get(match.group(2), "")
        for match in re.finditer(
            r"\b(?:cmd|command)\s*:\s*([A-Za-z_$][A-Za-z0-9_$]*|__ALCOVE_STR_[0-9]+__)",
            masked,
        ):
            token = match.group(1)
            value = literals.get(token, variables.get(token))
            if value:
                candidates.append(value)

    out = []
    for command in candidates:
        for launcher in _command_launchers(command):
            if launcher not in out:
                out.append(launcher)
            if len(out) == 8:
                return out
    return out


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
    event = EVENT_MAP.get(event_name)
    if event is None:
        return  # not ours; say nothing

    harness = _harness(payload)
    ts, now = _timestamp()

    if event in ("stop", "subagent_stop"):
        # A turn or a child ended. `tool` stays "" rather than null: the merged
        # ingester types it `String`, and a null there fails deserialization and
        # drops the line -- which would lose the very event this exists for.
        tool = ""
        tool_use_id = None
        ok = None
        if event == "subagent_stop":
            # `session_id` is the PARENT's and `transcript_path` is the PARENT's
            # transcript. The child is named by `agent_id`, and its own
            # transcript is a SEPARATE field, `agent_transcript_path`.
            target = _str(payload.get("agent_id"), MAX_FIELD)
            arg = _str(payload.get("agent_transcript_path"), MAX_ARG)
        else:
            target = None
            arg = None
        # No redaction here: both are harness-generated ids and paths, never
        # user text, and a mangled child id would defeat the point.
    else:
        tool = _str(payload.get("tool_name"), MAX_FIELD) or ""
        tool_use_id = _str(payload.get("tool_use_id"), MAX_FIELD)
        ok = _determine_ok(payload) if event == "post" else None
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
        "tool": tool,
        "cwd": _str(payload.get("cwd"), MAX_TARGET),
        "target": target,
        "arg": arg,
        "ok": ok,
        "tool_use_id": tool_use_id,
        # Which agent acted. `session_id` on a child's tool call is the PARENT's
        # -- verified by capture -- so without this a child's activity cannot be
        # told from its parent's, and "this subagent is still working" has no
        # authoritative source. Both harnesses put `agent_id`/`agent_type` at the
        # top level of a child's tool payload and omit them for the parent's own
        # calls (including the `Agent`/`Task` call that spawned the child), so
        # null here means "the session itself", not "unknown".
        "agent_id": _str(payload.get("agent_id"), MAX_FIELD),
        "agent_type": _str(payload.get("agent_type"), MAX_FIELD),
        # This is the durable fact the child transcript cannot provide when a
        # launcher is wrapped, fails, or uses ephemeral storage. Names only: the
        # command and prompt remain subject to the stricter arg whitelist above.
        "agent_launchers": _agent_launchers(tool, payload.get("tool_input")),
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
