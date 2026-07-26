#!/usr/bin/env python3
"""Sybil's Alcove — a live view of local coding-agent sessions and their subagents.

Answers the question the built-in tooling does not: *which model is actually
serving this session right now, what did it spawn, and what is that costing?*

`claude agents --json` reports pid/cwd/sessionId and nothing else — no model, no
tokens, no subagents. A serving model can also change mid-session without the
switch appearing in the conversation, so the transcript is the only honest
record of what ran.

Read-only by design: it opens transcripts, never writes them, and never calls a
model API. Binds 127.0.0.1 unless told otherwise — a private overlay is not
authentication, and this exposes prompts.

This file is the entrypoint only; the code lives in `alcove/`. Run it directly:

    python3 alcove.py

No dependencies — Python 3.11+ standard library only.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Support being run as a plain script from anywhere, not only as a module.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from alcove.web import serve  # noqa: E402

if __name__ == "__main__":
    try:
        raise SystemExit(serve())
    except KeyboardInterrupt:
        raise SystemExit(130) from None
