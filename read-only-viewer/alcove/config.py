"""Environment configuration. Every value has a working default except
ALCOVE_TOKEN, which is mandatory for any non-loopback bind (see web.serve)."""

from __future__ import annotations

import os
from pathlib import Path

# expanduser(): these are routinely set from an env file as "~/.claude/…", and a
# literal "~" path fails is_dir() — the server would exit "no transcripts".
CLAUDE_ROOT = Path(
    os.environ.get("ALCOVE_CLAUDE_ROOT", Path.home() / ".claude" / "projects")
).expanduser()
CODEX_ROOT = Path(
    os.environ.get("ALCOVE_CODEX_ROOT", Path.home() / ".codex" / "sessions")
).expanduser()

PORT = int(os.environ.get("ALCOVE_PORT", "8899"))
# Localhost by default. Set ALCOVE_BIND=0.0.0.0 deliberately, knowing this page
# shows task prompts and there is no auth in front of it.
BIND = os.environ.get("ALCOVE_BIND", "127.0.0.1")

# No transcript write in this long => treated as idle. Generous on purpose: a
# long tool call or a thinking turn writes nothing.
LIVE_WINDOW_S = float(os.environ.get("ALCOVE_LIVE_WINDOW_S", "300"))

# Transcripts reach 100MB+; only the tail is read, so totals are recent-window.
TAIL_LINES = int(os.environ.get("ALCOVE_TAIL_LINES", "4000"))
TAIL_BYTES = int(os.environ.get("ALCOVE_TAIL_BYTES", str(1 << 20)))

# The `claude` CLI supplies the authoritative session-id -> pid mapping. Set this
# when it is not on PATH, which is the normal case under a service manager.
CLAUDE_BIN = os.environ.get("ALCOVE_CLAUDE_BIN", "")

CACHE_TTL_S = 2.0

# Shared secret required when not bound to loopback. Empty + non-local bind is
# refused at startup rather than served open.
TOKEN = os.environ.get("ALCOVE_TOKEN", "")
COOKIE = "alcove_token"
LOCAL_BINDS = {"127.0.0.1", "localhost"}

STATIC_DIR = Path(__file__).resolve().parent / "static"


def is_local_bind() -> bool:
    return BIND in LOCAL_BINDS
