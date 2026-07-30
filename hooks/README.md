# `hooks/` — the tool-call spool

`alcove_spool_hook.py` is an observation-only hook. A harness runs it before and
after every tool call; it appends one line describing the call to a spool file
and exits. A separate ingester consumes the spool. The hook makes no decisions,
blocks nothing, opens no socket, and reads no tool output body.

**Its only permitted failure mode is "a line is missing."** The entire body is
wrapped in `try/except BaseException` and it always `exit(0)` — a tool hook that
exits nonzero or hangs degrades every live session on the machine, and a dropped
observation is strictly cheaper than that.

## Spool contract (frozen)

Directory `/root/.local/state/alcove/spool/` (`mkdir -p` on every invocation),
file `<harness>-<YYYYMMDD>.jsonl` on the **UTC** date, one JSON object per line,
written with a single `O_APPEND` `write()` of at most **2048 bytes**.

| field | type | meaning |
|---|---|---|
| `v` | int | schema version, currently `1` |
| `ts` | str | ISO8601 UTC with milliseconds, `2026-07-30T13:30:57.243Z` |
| `harness` | `"claude"` \| `"codex"` | which agent harness emitted the event |
| `event` | `"pre"` \| `"post"` | PreToolUse or PostToolUse |
| `session_id` | str | harness session id (`""` if the payload omitted it) |
| `tool` | str | tool name (`""` if the payload omitted it) |
| `cwd` | str \| null | harness-reported working directory |
| `target` | str \| null | `file_path`, or the path-like primary argument, if the tool has one |
| `arg` | str \| null | head of the primary argument, ≤500 chars; for a shell tool this is the command head |
| `ok` | bool \| null | post events only; `null` when not cheaply determinable |
| `tool_use_id` | str \| null | the harness's own id for the call |

Caps are hard, not advisory: `arg` and `target` ≤500 chars, `tool` /
`session_id` / `tool_use_id` ≤200, and the assembled line ≤2048 bytes. If a line
still exceeds 2048 the hook sheds `arg`, then `target`, then `cwd`, and only if
that is still not enough does it drop the line.

Duplicate-safety and ordering are the ingester's problem. The hook writes what it
sees, when it sees it; concurrent sessions interleave in one file.

### What never reaches the spool

No `tool_response` body, no environment variables, no file contents, no message
or prompt bodies.

- `arg` is taken from a **whitelist** of `tool_input` keys, not "the first key":
  `command`, `cmd`, `pattern`, `query`, `search_query`, `url`, `skill`,
  `file_path`, `notebook_path`, `path`, `filePath`, `description`. So `Write`
  spools its `file_path` and never its `content`; `Edit` never spools
  `old_string`/`new_string`; `Agent` spools its `description` and never its
  `prompt`; `mcp__hindsight__retain` never spools its `content`.
- `tool_response` is inspected for failure *markers* only (`is_error`,
  `interrupted`, `exit_code`, …). Its values are never copied out — a truthy
  `error` sets `ok: false` and the message is discarded.
- URLs are truncated to `scheme://host/path`; query strings and fragments are
  dropped because they carry tokens.
- `arg` and `target` pass through a blunt credential scrub: any quoted run
  mentioning `passw|secret|token|key|credential|bearer|auth|session` is replaced
  wholesale, unquoted `secretish=value` loses its value, `Bearer <blob>` and URL
  userinfo (`postgres://u:pw@host`) are replaced, known prefixes (`sk-`, `ghp_`,
  `xoxb-`, `AKIA`, …) are replaced, and any opaque run of ≥40 word characters is
  replaced.

  **Over-redaction is deliberate.** `echo "=== nats secret ==="` spools as
  `echo [redacted]` and `--sort-key=name` loses `name`. Losing a shell comment is
  an acceptable price for never spooling a bearer token out of a `curl` line;
  the reverse trade is not available.

### Known floors

- **A `pre` with no matching `post` is the failure signal.** Neither harness
  emits PostToolUse for a tool that errored or was denied — verified live: a
  `Read` of a nonexistent file and a permission-blocked `Bash` each produced a
  `pre` line and no `post` line. The hook cannot mark those, and deliberately
  does not guess; the ingester pairs on `tool_use_id`.
- **`ok: true` means "the harness returned a tool result", not "the command
  exited 0".** `ls /nonexistent` returns exit 2 inside a successful `Bash` tool
  call, and Claude Code's `tool_response` carries no exit code — so that line is
  `ok: true`. Reading the shell's status would mean parsing the response body,
  which this hook will not do.
- **`ok: null` and `ok: false` are different answers and must stay different.**
  `null` is "I could not tell"; `false` is "it failed". A string `tool_response`
  is opaque without reading it, so it yields `null`.
- **Codex `custom_tool_call` payloads carry a bare string, not an object.** For
  `apply_patch` the hook lifts only the `*** Add File: <path>` header line — never
  a byte of the patch body. For `exec` (a raw script body) both `target` and
  `arg` are `null` by design.
- Latency is dominated by CPython process startup, not by the hook. On this host
  `python3 -c pass` alone is ~11 ms median / ~26 ms p95; the full hook is
  **~24.5 ms median, ~42.5 ms p95, 54.6 ms max over 100 runs** on a real
  PreToolUse payload — roughly 2–3 ms of actual work. There is no meaningful
  headroom to win without leaving Python.

## Payload shapes actually observed

Captured by wiring a hook that dumped stdin verbatim and running real turns —
not read off a schema.

**Claude Code 2.1.208**, PreToolUse:

```json
{
  "session_id": "65db234b-…", "transcript_path": "/root/.claude/projects/…/….jsonl",
  "cwd": "/root/proj/sybils-alcove", "prompt_id": "743a3b72-…",
  "permission_mode": "default", "effort": {"level": "xhigh"},
  "hook_event_name": "PreToolUse", "tool_name": "Bash",
  "tool_input": {"command": "echo hooktest", "description": "Echo hooktest"},
  "tool_use_id": "toolu_01BdT79P3PDJPGu6RwGiN2HU"
}
```

PostToolUse adds `tool_response` and `duration_ms` and repeats the same
`tool_use_id`. For `Bash` the response is
`{"stdout":…,"stderr":…,"interrupted":false,"isImage":false,"noOutputExpected":false}`;
for `Read` it is `{"type":"text","file":{"filePath":…,"content":…,"numLines":…}}`.
`tool_use_id` is a **top-level** field on both events, not nested in the response.

**Codex CLI 0.146.0** sends the same field names. Its embedded
`pre-tool-use.command.input` / `post-tool-use.command.input` JSON schemas require
`cwd`, `hook_event_name`, `model`, `permission_mode`, `session_id`, `tool_input`,
`tool_name`, `tool_use_id`, `transcript_path`, `turn_id` (plus `tool_response` on
post), with optional `agent_id` / `agent_type`. `tool_input` and `tool_response`
are typed `true` — any JSON, which is why the hook handles a bare string.

Codex tool names differ from Claude's: `exec_command` (`{"cmd":…,"workdir":…}`),
`apply_patch` and `exec` (bare-string `custom_tool_call`), `view_image`
(`{"path":…}`), `wait`, `write_stdin`, `spawn_agent`, `update_plan`.

**Harness detection**: both wirings pass `--harness claude` / `--harness codex`
explicitly. Absent the flag the hook sniffs `turn_id`, which is required in
Codex's schema and absent from Claude Code's payload.

## Wiring

### Claude Code — `~/.claude/settings.json`

Additive; leave the existing `Stop` / `SubagentStop` / `SessionEnd` entries
alone, and validate the file with `python3 -c 'import json;json.load(open(...))'`
before and after.

```json
"PreToolUse": [
  {
    "matcher": "*",
    "hooks": [
      {
        "type": "command",
        "command": "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness claude",
        "timeout": 5
      }
    ]
  }
],
"PostToolUse": [ … same … ]
```

Synchronous on purpose. `"async": true` would remove the ~25 ms from the session's
critical path, but async hook processes race the harness's teardown — in `claude -p`
runs the async Hindsight `Stop` hook is killed before it can log — and an
observation hook that silently loses its tail is worse than one that costs 25 ms.

### Codex — `~/.codex/config.toml`

Codex **does** support per-tool hook events; there is no asymmetry to document.
Its `[hooks]` table accepts the same PascalCase event names, and the binary
carries `PreToolUse`/`PostToolUse` handling end to end (`"Before a tool
executes"` / `"After a tool executes"` in its hook-management UI, plus
`PreToolUseHookSpecificOutputWire` and `"Command blocked by PreToolUse hook"`).

```toml
[hooks]
PreToolUse = [{ hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
PostToolUse = [{ hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
```

**A newly added Codex hook does not run until it is acknowledged in the TUI.**
Codex keys trust on a `trusted_hash` under
`[hooks.state."<config path>:<event>:<i>:<j>"]` and shows an unacknowledged entry
as *"New hook — review required"* / *"1 hook needs review before it can run."*
The hash is Codex's to compute: launch `codex`, open the hooks review, and trust
the two new entries. Do not hand-write a `trusted_hash` — a guessed hash either
fails closed or defeats the mechanism.

## Verifying

```sh
tail -f /root/.local/state/alcove/spool/claude-$(date -u +%Y%m%d).jsonl
```

Then run a turn that uses a shell command and a file read. Every line should
carry all eleven fields, `pre`/`post` should pair on `tool_use_id`, and no line
should exceed 2048 bytes:

```sh
python3 - <<'PY'
import glob, json
for f in glob.glob("/root/.local/state/alcove/spool/*.jsonl"):
    for i, line in enumerate(open(f), 1):
        assert len(line.encode()) <= 2048, (f, i)
        d = json.loads(line)
        assert set(d) == {"v","ts","harness","event","session_id","tool",
                          "cwd","target","arg","ok","tool_use_id"}, (f, i)
        assert d["event"] == "post" or d["ok"] is None, (f, i)
print("ok")
PY
```
