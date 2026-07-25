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

Per session — harness, live/idle, current model, **model-switch timeline with
timestamps**, reasoning effort, project, branch, token totals, turn count, pid.

Per subagent — id, role/type, model, running/done, turns, output/input/cache
tokens, age, and the task it was given. Sorted live-first. A session whose
subagents run a different model than the parent is flagged.

Subagents appear **as soon as they write their first event**, not when they
finish.

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

**On binding:** this page displays task prompts and has no authentication.
Default is localhost. Widening it puts your prompts on the network — a private
overlay is not authentication.

## Known limits

- Reads the **tail** of each transcript (they reach 100MB+), so token totals on
  very long sessions are recent-window, not lifetime. Identity is read from the
  head, so sessions are never missed.
- **"Live" means the transcript was written recently.** An agent in a long tool
  call writes nothing and reads idle. Tune `ALCOVE_LIVE_WINDOW_S`.
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
