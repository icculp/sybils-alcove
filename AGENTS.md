# Sybil's Alcove — agent guide

Tooling for observing local coding-agent sessions. `read-only-viewer/` is a web
view over transcript files on disk. Start with its README before changing it.

## The invariant: `read-only-viewer/` never writes

It opens transcripts, reads the process list, and serves a page. It does not
write transcripts, call a model API, or drive a session. Do not add a write path,
a "just this one" POST that mutates state, or a model call — a tool that can
drive sessions has a different threat model and belongs in its own top-level
directory with its own name. The only POST is `/login`, and it sets a cookie.

## Claims about what is running need evidence

The entire value of this tool is being right about which model served a turn and
whether a session is alive. Guessing defeats the purpose, so:

- **Verify against a real transcript**, not a plausible reading of the schema.
  Print the events and count them before believing a field means what it looks
  like. Every bug found here so far was a field that read correctly and meant
  something else: `turn_context` counted as a turn (it is once per session),
  `status: async_launched` read as terminal (it never flips), a model selected but
  never served (no assistant event exists to carry it).
- **A process is authoritative; a file timestamp is an inference.** Say which one
  a number came from, and label inference as inference in the UI.
- **Never let a failed lookup render as a negative result.** "I could not ask"
  and "the answer is no" must look different on screen. A silently swallowed
  error once made every session report no pid, which read as "nothing running".
- Run it and look at the page. `python3 read-only-viewer/alcove.py` needs no
  arguments and no dependencies.

## Workflow

- **A worktree per concern**, and one concern per PR. A restructure and a bug fix
  in one diff cannot be reviewed or reverted independently.
- **Commit subject-only by default.** Add a body for non-obvious *why*: what was
  measured, what the wrong reading was, what a future reader would otherwise
  re-derive. Do not paraphrase the diff.
- **State limits in the README** when you cannot fix them. A documented floor is
  a feature; an undocumented one is a bug report waiting to happen.
- Python 3.11+ stdlib only. A dependency needs a reason that survives "this is a
  single-file read-only tool".

## Config and secrets

Environment variables only — see `read-only-viewer/.env.example`. `ALCOVE_TOKEN`
is mandatory for any non-loopback bind and the server refuses to start without
one, because the page shows task prompts and a private network is not
authentication. Never commit a `.env`, never put a token in a URL, and never
print one in output or a commit message.
