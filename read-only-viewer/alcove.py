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


def ingest_once() -> int:
    """Scan every transcript and write derived facts to the store, then exit.

    Idempotent: the same scan run twice adds nothing the second time, because
    every fact is keyed by a natural id. Safe to run from cron.
    """
    from alcove import store
    from alcove.collect import collect

    conn = store.connect()
    counts = store.ingest(conn, collect())
    total = store.totals(conn)
    print(f"store: {store.db_path()}")
    print("  changed:  " + ", ".join(f"{k}={v}" for k, v in counts.items() if v))
    print(f"  lifetime: {total['turns']} turns, {total['output']} output tokens,"
          f" {total['sessions']} sessions")
    print(f"  span:     {total['first_ts']} .. {total['last_ts']}")
    return 0


if __name__ == "__main__":
    try:
        if "--ingest-only" in sys.argv:
            raise SystemExit(ingest_once())
        raise SystemExit(serve())
    except KeyboardInterrupt:
        raise SystemExit(130) from None
