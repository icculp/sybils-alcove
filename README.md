# Sybil's Alcove

Tooling for watching — and eventually working with — local coding-agent sessions.

## What's here

| | |
| --- | --- |
| [`read-only-viewer/`](read-only-viewer/) | A live web view of local Claude Code and Codex sessions: which model is actually serving each one, what it spawned, and what that cost. Read-only by construction. |

## Why the folder is named that

The viewer never writes a transcript and never calls a model API. That is a
property worth stating in the directory name rather than in a paragraph someone
might not read, because anything that *drives* sessions — interactive chat,
sandboxed execution — has a different threat model and belongs in its own
directory with its own name. A reader you can point at a machine and a
controller you can point at a machine deserve different levels of trust, and the
layout should make which one you are running obvious.

Start with [`read-only-viewer/README.md`](read-only-viewer/README.md).

```bash
python3 read-only-viewer/alcove.py     # http://127.0.0.1:8899
```

No dependencies — Python 3.11+ stdlib only. Configuration is environment
variables; copy [`read-only-viewer/.env.example`](read-only-viewer/.env.example)
and edit. Every value has a working default except `ALCOVE_TOKEN`, which is
required for any non-loopback bind.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
