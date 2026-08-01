# `hooks/` — the tool-call spool

`alcove_spool_hook.py` is an observation-only hook. A harness runs it before and
after every tool call, and again when a turn or a subagent ends; it appends one
line describing the event to a spool file and exits. A separate ingester consumes
the spool. The hook makes no decisions, blocks nothing, opens no socket, and reads
no tool output or assistant message body.

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
| `event` | `"pre"` \| `"post"` \| `"stop"` \| `"subagent_stop"` | which hook fired |
| `session_id` | str | harness session id (`""` if the payload omitted it) |
| `tool` | str | tool name; `""` on stop-family events |
| `cwd` | str \| null | harness-reported working directory |
| `target` | str \| null | tool events: `file_path` or the path-like primary argument. `subagent_stop`: the child's `agent_id` |
| `arg` | str \| null | tool events: head of the primary argument, ≤500 chars (for a shell tool, the command head). `subagent_stop`: the child's transcript path |
| `ok` | bool \| null | `post` only; `null` when not cheaply determinable |
| `tool_use_id` | str \| null | the harness's own id for the call; `null` on stop-family events |
| `agent_id` | str \| null | WHICH agent acted: a child's id, or `null` for the session's own turn |
| `agent_type` | str \| null | that child's kind (`Explore`, `general-purpose`, …) |
| `agent_launchers` | list[str] | agent executables/native roles classified without retaining their command or prompt |
| `params` | object | **spawn calls only, absent otherwise**: the whitelisted parameters of an agent launch — `model` above all. Never the prompt. Serialized form capped at 300 chars |

`agent_id` / `agent_type` were **added without bumping `v`**, and that is the
point: they are nullable and additive, an old line means exactly what a `null`
means, and a version bump would have made every deployed reader skip every new
line to gain nothing. `v` moves when an existing field changes meaning.

Caps are hard, not advisory: `arg` and `target` ≤500 chars, `tool` /
`session_id` / `tool_use_id` ≤200, and the assembled line ≤2048 bytes. If a line
still exceeds 2048 the hook sheds `arg`, then `target`, then `cwd`, and only if
that is still not enough does it drop the line.

Duplicate-safety and ordering are the ingester's problem. The hook writes what it
sees, when it sees it; concurrent sessions interleave in one file.

### `stop` and `subagent_stop`

These exist because liveness inferred from transcript mtime is wrong for minutes
at a time: a finished subagent keeps rendering as active until its file ages out
of the window. A stop event is the authoritative "done, now" signal.

**A stop is a state transition, not a tombstone.** A later event for the same
`session_id` — or the same `subagent_stop` `target` — means it resumed. The hook
records transitions; the viewer decides what the latest one means. Nothing here
should be read as "this session is gone forever."

`tool` is `""` on these lines, **not `null`**, and that is deliberate: the merged
ingester types the field `String`, so a `null` fails deserialization and the line
is dropped — losing exactly the event this was added for. An empty string
round-trips under both that type and a later `Option<String>`.

#### Identifying the child (verified, and not what it looks like)

The `subagent_stop` payload names the child in `agent_id` and gives the child's
own transcript in **`agent_transcript_path`**. Its `session_id` and
`transcript_path` are the **parent's** — `transcript_path` does *not* point at
`agent-<id>.jsonl`. Reading it as the child's transcript is the exact class of
mistake this repo keeps paying for, so, from a real capture:

```json
{
  "hook_event_name": "SubagentStop",
  "session_id": "8fcd8b00-…",                                     // PARENT
  "transcript_path": "/root/.claude/projects/-root-proj-sybils-alcove/8fcd8b00-….jsonl",   // PARENT
  "agent_id": "a409d7dba26431ed6",                                // the child
  "agent_type": "Explore",
  "agent_transcript_path": "/root/.claude/projects/-root-proj-sybils-alcove/8fcd8b00-…/subagents/agent-a409d7dba26431ed6.jsonl",
  "cwd": "/root/proj/sybils-alcove", "stop_hook_active": false,
  "last_assistant_message": "…", "background_tasks": [], "session_crons": []
}
```

So `target` = `agent_id`, `arg` = `agent_transcript_path`. The child transcript
path was confirmed to exist on disk, and its basename contains `target`. `Stop`
carries no agent fields at all, so its `target` and `arg` are `null`; the payload
does have the session's own `transcript_path`, which the contract does not spool.

Codex's `subagent-stop.command.input` schema **requires the same three field
names** (`agent_id`, `agent_transcript_path`, `agent_type`), so one mapping serves
both harnesses.

`last_assistant_message` is a message body and is never read.

#### A child's own tool calls carry the PARENT's `session_id` (verified)

Measured, not assumed, because the state fold depends on it. A `claude -p` turn
was run with a hook that dumped every payload verbatim, and it spawned an
`Explore` subagent. What came back, in order:

| event | `session_id` | `agent_id` | `agent_type` | tool |
|---|---|---|---|---|
| PreToolUse | `0311f4f7…` | *absent* | *absent* | `Agent` ← the parent spawning the child |
| PreToolUse | `0311f4f7…` | `ac76b5442617a9edf` | `Explore` | `Bash` ← **the child's own call** |
| PostToolUse | `0311f4f7…` | `ac76b5442617a9edf` | `Explore` | `Glob` |
| SubagentStop | `0311f4f7…` | `ac76b5442617a9edf` | `Explore` | — |
| PostToolUse | `0311f4f7…` | *absent* | *absent* | `Agent` |
| Stop | `0311f4f7…` | *absent* | *absent* | — |

Three facts that are easy to get backwards:

1. **A child's tool calls DO reach the spool.** They are not missing, and they are
   not filed under the child's own id — they carry the parent session's id, and
   the child is named only by the top-level `agent_id`.
2. **`agent_id` is absent for the parent's own calls**, including the `Agent` call
   that spawns the child. So `null` means "the session itself", not "unknown", and
   a child's work must never be counted as its parent's.
3. **A background child outlives its parent's turn.** `Stop` says the harness
   finished answering; it does not say the children are done. Observed live: a
   `stop` for `b3d712dd…` at 14:39:25 while a child spawned at 14:38 kept working
   and spooling.

Without `agent_id` on tool events there is no authoritative "this child is still
working" signal at all — only `subagent_stop`, which can say a child finished but
never that one resumed. That is why the field was added.

`agent_type` (`Explore`, `general-purpose`, `spark-triage`, …) rides along for a
smaller reason: a child whose parent spawn record says `agentType: null` — most of
them — can still be labelled by kind.

#### `params`: what a spawn was actually asked for (verified per harness)

`arg` on a spawn line carries the description and nothing else, so until this
existed the spool recorded *that* an agent was launched and never *what it was
given* — and **which model a subagent ran is the single most governance-relevant
parameter of a launch**. `params` carries a whitelist of it, on spawn lines only:

| key | harness | |
|---|---|---|
| `model` | both | the child's model. **Absent when the caller did not name one** |
| `subagent_type` | claude | `Explore`, `general-purpose`, … |
| `agent_type` | codex | `default`, `explorer`, `worker` |
| `effort` / `reasoning_effort` | claude / codex | the same fact, two spellings |
| `run_in_background` | claude | a background child outlives its parent's turn |
| `isolation` | claude | `worktree` / `remote` |
| `fork_context` | codex | whether the child inherited the parent's history |

Spawn tools are `Agent` and `Task` on Claude and anything containing
`spawn_agent` on Codex (the tool namespace is sometimes prefixed, as in
`multi_agent_v1spawn_agent`). The other `multi_agent_v1*` tools act on an agent
that already exists and carry no spawn parameters, so they get no `params`.

Captured payloads, prompt bodies elided — read off real runs, not a schema:

```jsonc
// Claude Code 2.1.208, PreToolUse, tool_name "Agent", model NAMED by the caller
"tool_input": {"description":"List files in hooks/ directory","prompt":"…",
               "subagent_type":"Explore","model":"haiku","run_in_background":false}
// the same harness, same tool, model NOT named — there is no `model` key at all
"tool_input": {"description":"List files in /tmp","prompt":"…",
               "subagent_type":"Explore","run_in_background":false}
// Codex 0.146.0 spawn_agent arguments
{"agent_type":"default","fork_context":false,"model":"gpt-5.5",
 "reasoning_effort":"xhigh","service_tier":"priority","message":"…"}
```

Three consequences worth stating rather than rediscovering:

1. **An absent `model` means the caller did not choose one**, not that the child
   ran the default. The harness resolves the default after the hook has seen the
   payload, so the spool cannot know what it resolved to and does not guess.
2. **`params` is absent, never `{}`**, on a non-spawn line, on a spawn that named
   none of these, and on every line written before the field existed. Additive
   and nullable, so `v` stays `1` for the same reason `agent_id` did.
3. The serialized object is capped at **300 chars**, and the cap is applied by
   DROPPING whole keys in the table's order rather than clipping the JSON — a
   truncated object is unparseable, and half a parameter set beats none. Every
   value is an enum, a bool or a model name, so this cap has never bitten;
   it exists so a future harness passing a long free-form value cannot push the
   line toward the 2048-byte limit.

`prompt` (Claude) and `message` (Codex) are the task body and are **never**
spooled — the keys are a whitelist, so a parameter added by a future release
cannot leak a body in by being unrecognised. The credential scrub is deliberately
NOT applied to these values: they are harness enums and model names rather than
user text, and the scrub would eat any of them containing `session`, `key` or
`auth`. Same reasoning as the stop-family ids.

### What never reaches the spool

No `tool_response` body, no environment variables, no file contents, no message
or prompt bodies.

- Only `tool_input` is ever mined for content, so `last_assistant_message` on the
  stop-family events has no path to the spool at all.
- `arg` is taken from a **whitelist** of `tool_input` keys, not "the first key":
  `command`, `cmd`, `pattern`, `query`, `search_query`, `url`, `skill`,
  `file_path`, `notebook_path`, `path`, `filePath`, `description`. So `Write`
  spools its `file_path` and never its `content`; `Edit` never spools
  `old_string`/`new_string`; `Agent` spools its `description` and never its
  `prompt`; `mcp__hindsight__retain` never spools its `content`. `params` is a
  second, separate whitelist over the same `tool_input` — see above.
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

  Stop-family `target`/`arg` skip the scrub: they are harness-generated ids and
  paths, never user text, and a mangled child id would defeat the point.

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
  `arg` are `null` by design. The hook separately classifies only values passed
  to `cmd:`/`command:` into `agent_launchers`; this preserves a launch fact
  without spooling the script or agent prompt.
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

`Stop` and `SubagentStop` already carry a Hindsight hook. **Multiple hooks per
event are supported — append a second group, do not replace theirs:**

```json
"Stop": [
  { "hooks": [ { …existing hindsight-retain… } ] },
  { "hooks": [ { "type": "command",
                 "command": "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness claude",
                 "timeout": 5 } ] }
],
"SubagentStop": [ … same two groups … ]
```

Synchronous on purpose. `"async": true` would remove the ~25 ms from the session's
critical path, but async hook processes race the harness's teardown — the async
Hindsight `Stop` hook is sometimes killed before it can log in short `claude -p`
runs — and an observation hook that silently loses its tail is worse than one that
costs 25 ms.

### Codex — `~/.codex/config.toml`

Codex **does** support per-tool hook events; there is no asymmetry to document.
Its `[hooks]` table accepts the same PascalCase event names, and the binary
carries `PreToolUse`/`PostToolUse` handling end to end (`"Before a tool
executes"` / `"After a tool executes"` in its hook-management UI, plus
`PreToolUseHookSpecificOutputWire` and `"Command blocked by PreToolUse hook"`).

All four events go in `[hooks]`, and they must appear **before** the
`[hooks.state]` table header — once that header opens, bare keys belong to it.
`Stop` and `SubagentStop` already hold a Hindsight hook; append a second group
rather than replacing it, which also keeps the existing hook at index `0` so its
`trusted_hash` stays valid.

```toml
[hooks]
Stop = [{ hooks = [{ …existing hindsight-retain… }] }, { hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
SubagentStop = [{ hooks = [{ …existing hindsight-retain… }] }, { hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
PreToolUse = [{ hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
PostToolUse = [{ hooks = [{ type = "command", command = "python3 /root/proj/sybils-alcove/hooks/alcove_spool_hook.py --harness codex", timeout = 5 }] }]
```

**A newly added Codex hook does not run until it is acknowledged in the TUI.**
Codex keys trust on a `trusted_hash` under
`[hooks.state."<config path>:<event>:<i>:<j>"]` — note the **snake_case** event in
the state key (`stop:0:0`) against the PascalCase config key — and shows an
unacknowledged entry as *"New hook — review required"* / *"1 hook needs review
before it can run."* The hash is Codex's to compute: launch `codex`, open the hooks
review, and trust the new entries. Do not hand-write a `trusted_hash` — a guessed
hash either fails closed or defeats the mechanism.

Measured, not assumed: a `codex exec` turn after wiring ran the pre-existing
trusted Stop hook (it prints `hook: Stop`) and wrote **no** spool line, and no new
`[hooks.state]` entry appeared. So the Codex side is **wired and unacknowledged**;
its `PreToolUse`, `PostToolUse`, `Stop[1]` and `SubagentStop[1]` entries stay inert
until someone trusts them in the TUI. The mapping itself was exercised by replaying
payloads built from Codex's own required-field schemas.

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
        # The additive fields are absent on lines written before they existed,
        # which is why this checks a floor and a ceiling rather than equality.
        base = {"v","ts","harness","event","session_id","tool",
                "cwd","target","arg","ok","tool_use_id"}
        added = {"agent_id","agent_type","agent_launchers","params"}
        assert base <= set(d) <= base | added, (f, i)
        assert d["event"] in {"pre","post","stop","subagent_stop"}, (f, i)
        assert d["event"] == "post" or d["ok"] is None, (f, i)
        if d["event"] in {"stop","subagent_stop"}:
            assert d["tool"] == "" and d["tool_use_id"] is None, (f, i)
        # params is a spawn-only field, is never empty when present, and never
        # carries a body.
        if "params" in d:
            assert d["tool"] in ("Agent","Task") or "spawn_agent" in d["tool"], (f, i)
            assert d["params"] and len(json.dumps(d["params"])) <= 300, (f, i)
            assert not ({"prompt","message","description"} & set(d["params"])), (f, i)
print("ok")
PY
```

For the stop-family events, run a turn that spawns a subagent
(`claude -p "use the Agent tool with subagent_type Explore to …"`) and confirm a
subagent actually ran before trusting the capture. A `subagent_stop` line should
name a child whose transcript exists:

```sh
python3 - <<'PY'
import glob, json, os
for f in glob.glob("/root/.local/state/alcove/spool/*.jsonl"):
    for line in open(f):
        d = json.loads(line)
        if d["event"] == "subagent_stop":
            print(d["target"], os.path.exists(d["arg"] or ""), d["arg"])
PY
```
