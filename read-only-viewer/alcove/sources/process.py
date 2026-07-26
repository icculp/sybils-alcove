"""Live process state — the only authoritative liveness signal.

A file timestamp says a transcript was written recently. Only a process says a
session is alive. These are different facts and the UI keeps them separate.
"""

from __future__ import annotations

import glob
import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

from .. import config


def pid_alive(pid: int) -> bool:
    """Portable liveness probe. /proc exists only on Linux — checking it on
    macOS silently drops every pid and `running` never appears, with the lookup
    still reporting ok. Signal 0 works everywhere; EPERM means the process
    exists but belongs to someone else, which is still alive.
    """
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False


def claude_bin() -> str:
    """Absolute path to the `claude` CLI.

    A bare "claude" resolves fine in a login shell and not at all under systemd,
    whose PATH has no nvm directory. That failure was swallowed by a bare except
    for the entire life of the pid column: every session reported no process, so
    liveness silently degraded to "was this file written recently", which reports
    a busy session as idle and a dead one as present.
    """
    if config.CLAUDE_BIN:
        return config.CLAUDE_BIN
    found = shutil.which("claude")
    if found:
        return found
    for pattern in (str(Path.home() / ".nvm/versions/node/*/bin/claude"),
                    str(Path.home() / ".local/bin/claude"),
                    "/usr/local/bin/claude", "/usr/bin/claude"):
        for hit in sorted(glob.glob(pattern), reverse=True):
            if os.access(hit, os.X_OK):
                return hit
    return ""


def running_pids() -> tuple[dict[str, dict[str, Any]], str]:
    """(sessionId -> {pids, name, kind}, status).

    Status is reported to the page rather than discarded: "no process" and "I
    could not ask" must not look the same, or a broken lookup reads as every
    session having ended.
    """
    exe = claude_bin()
    if not exe:
        return {}, "unavailable: claude CLI not found"
    try:
        proc = subprocess.run([exe, "agents", "--json", "--all"],
                              capture_output=True, text=True, timeout=25)
    except Exception as err:  # noqa: BLE001 - reported, not swallowed
        return {}, f"unavailable: {type(err).__name__}"
    if proc.returncode != 0:
        return {}, f"unavailable: exit {proc.returncode}"
    try:
        rows = json.loads(proc.stdout) if proc.stdout.strip() else []
    except ValueError:
        return {}, "unavailable: unparseable output"
    out: dict[str, dict[str, Any]] = {}
    for row in rows if isinstance(rows, list) else []:
        sid, pid = str(row.get("sessionId") or ""), row.get("pid")
        # The CLI can list an entry whose process is already gone.
        if not (sid and isinstance(pid, int) and pid_alive(pid)):
            continue
        entry = out.setdefault(sid, {"pids": [], "name": "", "kind": ""})
        entry["pids"].append(pid)
        # The CLI's own label for the window ("root-4c"), so a row can be matched
        # to the terminal the operator is actually typing in.
        entry["name"] = entry["name"] or str(row.get("name") or "")
        entry["kind"] = entry["kind"] or str(row.get("kind") or "")
    return out, "ok"


def codex_process_count() -> int | None:
    """How many `codex` processes are running, or None if /proc is unreadable.

    Deliberately a count and not a mapping: Codex puts no thread id in its argv
    and holds no transcript fd open, so there is no honest way to attribute a
    process to a session. Counting argv[0] basenames avoids double-counting the
    `node` wrapper that fronts each one. Linux-only; returns None elsewhere.
    """
    try:
        entries = [p for p in Path("/proc").iterdir() if p.name.isdigit()]
    except OSError:
        return None
    total = 0
    for entry in entries:
        try:
            argv = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if argv and argv[0] and os.path.basename(
                argv[0].decode("utf-8", errors="replace")) == "codex":
            total += 1
    return total
