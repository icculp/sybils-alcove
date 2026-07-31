#!/usr/bin/env python3
"""Emit the canonical snapshot from the PYTHON implementation.

The other half of the equivalence gate. Everything volatile — wall clock, file
ages, liveness, pids, process state — is excluded, because those differ between
two runs of the same implementation and would drown the signal. What remains is
the parsing facts, which is what a port can get wrong.

Codex's private sqlite is now read by both implementations, so the gate points
them at the frozen copy inside the fixture.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

# ALCOVE_CODEX_HOME is honoured from the environment: the gate points BOTH
# implementations at the frozen copy in the fixture, so the Codex sqlite
# enrichment is compared rather than switched off.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))  # reference/

from alcove.sources.claude import collect_claude  # noqa: E402
from alcove.sources.codex import collect_codex  # noqa: E402


def usage(u: dict) -> dict:
    return {k: int(u.get(k) or 0)
            for k in ("input", "output", "cache_read", "cache_write", "reasoning")}


def sub(a: dict) -> dict:
    return {
        "id": a.get("id", ""), "label": a.get("label", ""),
        "model": a.get("model", ""), "role": a.get("role", ""),
        "status": a.get("status", ""), "turns": int(a.get("turns") or 0),
        "usage": usage(a.get("usage") or {}), "task": a.get("task", ""),
    }


def session(s: dict) -> dict:
    subs = sorted((sub(a) for a in s.get("subagents") or []), key=lambda x: x["id"])
    return {
        "harness": s["harness"], "session_id": s["session_id"],
        "label": s.get("label", ""), "project": s.get("project", ""),
        "cwd": s.get("cwd", ""), "branch": s.get("branch", ""),
        # `effort` is DELIBERATELY not compared. The frozen reference reads it
        # as {"level": ...}, a shape that occurs zero times in 19,783 real
        # events, so it reports "" for every session; Rust reads the bare string
        # the harness actually writes. Leaving it in turns a documented, one-way
        # divergence into a permanent gate failure that trains people to ignore
        # the gate. The split is asserted instead — see store_equivalence.py's
        # `check_columns`, which fails if the reference ever starts reporting it.
        "model": s.get("model", ""),
        "selected_model": s.get("selected_model", ""),
        "turns": int(s.get("turns") or 0), "last_ts": s.get("last_ts", ""),
        "usage": usage(s.get("usage") or {}),
        "timeline": [{"model": t["model"], "at": t["at"]} for t in s.get("timeline") or []],
        "selections": [{"model": x["model"], "at": x["at"],
                        "requested": x.get("requested", "")}
                       for x in s.get("selections") or []],
        "compactions": [{"at": c.get("at", ""), "trigger": c.get("trigger", ""),
                         "pre_tokens": c.get("pre_tokens")}
                        for c in s.get("compactions") or []],
        "subagents": subs,
    }


def main() -> int:
    rows = [session(s) for s in collect_claude() + collect_codex()]
    rows.sort(key=lambda s: (s["harness"], s["session_id"]))
    print(json.dumps({"sessions": rows}, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
