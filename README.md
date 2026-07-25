# Sybil's Alcove

A live, read-only web view of local coding-agent sessions: which model is
actually serving each one, what it spawned, and what that cost.

Supports **Claude Code** and **Codex** side by side.

```bash
python3 alcove.py          # http://127.0.0.1:8899
```

No dependencies — Python 3.11+ stdlib only.

## Why

`claude agents --json` reports pid, cwd, and session id. Not the model, not
token usage, not subagents. Codex has no equivalent at all.

The gap that matters: **a session's serving model can change mid-thread without
the switch appearing in the conversation.** If you dispatch a fan-out, the
subagents may run on a different model than the thread that spawned them, and
nothing surfaces that. Transcripts on disk are the only honest record.

## What it shows

Per session — harness, state, current model, **model-switch timeline with
timestamps**, reasoning effort, project, branch, token totals, turn count, pid.

**A transcript on disk is not a session.** State is four distinct values, never
collapsed into one "live" flag:

| state | meaning |
| --- | --- |
| `running` | a live process owns this session id — authoritative |
| `writing` | no owning process found, but the transcript moved recently |
| `ended` | no process, no recent write; the transcript is all that is left |
| `unknown` | the pid lookup itself failed, so absence proves nothing |

This matters in both directions. A session in a long tool call writes nothing for
an hour and looks dead by file age alone, while a finished one-shot run leaves a
transcript that looks like a session forever. Only the process answers it.

Per subagent — id, role/type, model, running/idle, turns, output/input/cache
tokens, age, and the task it was given. A session whose subagents run a
different model than the parent is flagged.

Counts are **active / total**, not lifetime only: a session with 2 running out
of 28 spawned reads `2 active / 28 sub`, and the `active subagents` filter shows
only sessions doing work right now. Sessions and the subagent drilldown use the
same ordering — running first, then freshest.

Subagents appear **as soon as they write their first event**, not when they
finish.

**Compaction is shown**, because it is the one event that invalidates every
token total above it. A compacted session gets a `compacted HH:MM:SS` pill, the
pre-compaction context size where the harness records it, and token/turn figures
as `since last compaction / tail total`. Without this, compacting a session
changes nothing on screen and the totals silently describe a context that no
longer exists.

## Where the data comes from

```
Claude Code
  ~/.claude/projects/<project>/<session-id>.jsonl          main thread
  ~/.claude/projects/<project>/<session-id>/subagents/
      agent-<agentId>.jsonl                                one per subagent

Codex
  ~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<id>.jsonl    one per session;
      spawned agents are sibling files linked by parent_thread_id
```

Model authority differs by harness: Claude records `message.model` per assistant
event; Codex records `model` + `effort` in `turn_context`, and cumulative token
totals in `token_count`.

## Config

| env | default | |
| --- | --- | --- |
| `ALCOVE_PORT` | `8899` | |
| `ALCOVE_BIND` | `127.0.0.1` | See below before changing. |
| `ALCOVE_LIVE_WINDOW_S` | `300` | Idle threshold. |
| `ALCOVE_TAIL_LINES` | `4000` | Lines read per transcript. |
| `ALCOVE_CLAUDE_ROOT` | `~/.claude/projects` | |
| `ALCOVE_CODEX_ROOT` | `~/.codex/sessions` | |
| `ALCOVE_CLAUDE_BIN` | auto | Path to `claude`. Auto-resolves via PATH, then nvm. |
| `ALCOVE_TOKEN` | — | Required for any non-loopback bind. |

**On binding:** this page displays task prompts. Default is localhost, which
needs no token. Any wider bind **requires `ALCOVE_TOKEN` and refuses to start
without one** — a private overlay is not authentication.

Browsers get a login form that POSTs the token and stores an HttpOnly,
SameSite=Strict cookie. There is deliberately no `?token=` URL parameter: a
secret in a URL leaks into browser history, screenshots, referers, and shell
history. Scripts use `Authorization: Bearer <token>`.

The wire is plain HTTP, so the token is only as private as the network carrying
it. ZeroTier encrypts peer-to-peer; do not put this on an untrusted network.

## Known limits

- Reads the **tail** of each transcript (they reach 100MB+), so token totals on
  very long sessions are recent-window, not lifetime. Identity is read from the
  head, so sessions are never missed.
- **Codex state is inferred, and says so** (shown as `writing?`). Codex puts no
  thread id in its argv and holds no transcript fd open, so there is no honest
  way to attribute a process to a session — only a total count of `codex`
  processes. Claude state is authoritative because `claude agents --json --all`
  maps session id to pid.
- **`writing` still leans on file age** for the no-pid case, so
  `ALCOVE_LIVE_WINDOW_S` tunes that fallback. `running` does not depend on it.
- **The pid lookup is a subprocess and can fail.** When it does, the header says
  so and every Claude session reads `unknown` rather than `ended` — a broken
  lookup must not render as "everything stopped". This was a real bug: under
  systemd, PATH has no nvm directory, `claude` did not resolve, and the failure
  was swallowed, so every session showed no pid and liveness silently degraded
  to file age.
- **`idle` is not the same as `done`.** The parent's launch record says
  `async_launched` for every backgrounded subagent and never flips to
  `completed`, so only the 12% that report `completed` can be called finished.
  For the rest, an idle transcript may mean finished or abandoned — the harness
  does not say which, so neither does this.
- Claude subagent `type` is often blank because the parent records
  `agentType: null` for most spawns. Missing at the source, not dropped here.
- Codex turn counts are under-reported (only the tail is scanned for
  `turn_context`).

## Two traps worth knowing if you read transcripts yourself

Both of these produce confidently wrong output rather than an error:

1. **File order is not chronological.** Compaction rewrites transcripts with
   repeated, out-of-order blocks. Scanning raw order reports model switches that
   run backwards in time. Dedupe by `uuid`, sort by timestamp.
2. **Identity is in the first line, activity is in the last.** Codex writes
   `session_meta` (thread id, `parent_thread_id`) as line one. A tail-only read
   loses identity on any large file, and the session silently vanishes from the
   listing instead of erroring.

Also: `<synthetic>` appears where a model name goes. It marks harness-injected
messages, not a served model — counting it invents phantom switches.

## Scope

Read-only, deliberately. It opens transcript files, never writes them, and never
calls a model API. Driving sessions from here is a different tool with a
different risk profile.
