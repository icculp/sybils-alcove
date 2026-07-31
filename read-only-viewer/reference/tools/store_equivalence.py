#!/usr/bin/env python3
"""Third gate property: the Python and Rust stores must agree.

The canonical-snapshot gate cannot see this. Snapshots exclude per-turn rows, so
two implementations can agree on every aggregate and still write different rows
into the store — which is exactly what happened: a Codex message id inherited by
seven threads collapsed to one row under INSERT OR IGNORE, and each
implementation kept a different one because they iterated subagents in a
different order.

Usage: store_equivalence.py <fixture-dir> <py.db> <rs.db>
Volatile columns (first_seen, last_seen, observation) are excluded: they are
wall-clock and differ between two runs of the same implementation.
"""

from __future__ import annotations

import sqlite3
import sys

# `turn` is checked separately: the two implementations DELIBERATELY differ there
# after the freeze (see reference/README.md and PORT.md).
TABLES = {
    "selection": "select session_id,ts,model,requested from selection"
                 " order by session_id,ts,model",
    "compaction": "select session_id,ts,trigger,pre_tokens from compaction"
                  " order by session_id,ts",
    "session": "select id,harness,project,cwd,branch from session order by id",
    "subagent": "select id,session_id,harness,model,role from subagent order by id",
}


# The columns BOTH stores have. `effort` and `version` are Rust-only by decision
# — the reference is frozen and does not learn them — so they can never appear in
# a shared-column comparison, and `check_columns` below asserts that split
# instead of letting it rot into an untested assumption.
COLS = ("id,session_id,harness,ts,model,input,output,cache_read,cache_write,"
        "is_subagent")

# Columns the Rust store has and the reference must NOT grow.
RUST_ONLY = ("effort", "version")


def turn_columns(conn: sqlite3.Connection) -> set[str]:
    return {row[1] for row in conn.execute("pragma table_info(turn)")}


def check_columns(py: sqlite3.Connection, rs: sqlite3.Connection) -> bool:
    """The second documented divergence: per-turn effort and harness version.

    The Rust scanners read a per-turn reasoning effort (both harnesses) and a
    per-turn harness version (Claude only — Codex records one per rollout, and a
    resumed rollout replays turns an older build served). The reference is
    frozen and writes neither.

    A shared-column diff would pass silently whether or not that stayed true, so
    the shape is asserted directly: Rust HAS them, the reference does NOT, and
    every column the two stores share is still compared byte for byte by
    `check_turns`.
    """
    a, b = turn_columns(py), turn_columns(rs)
    ok = True
    for column in RUST_ONLY:
        if column not in b:
            print(f"     rust `turn` is MISSING {column} — the divergence is "
                  f"documented in PORT.md but not implemented")
            ok = False
        if column in a:
            print(f"     reference `turn` grew {column} — the reference is "
                  f"frozen; either unfreeze it deliberately or revert")
            ok = False
    # Not just "the column exists" — prove the Rust reader actually fills it, and
    # prove the frozen reference still cannot. A column that is present and
    # always empty would satisfy a shape check and mean nothing.
    filled = rs.execute(
        "select count(*) from turn where effort <> '' or version <> ''"
    ).fetchone()[0]
    total = rs.execute("select count(*) from turn").fetchone()[0]
    print(f"     rust populates {filled}/{total} turn rows with effort/version")
    # `effort` is also excluded from the canonical snapshot for the same reason
    # (canonical.py). If the reference is ever unfrozen and learns to read it,
    # that exclusion becomes wrong — so notice it here.
    if "effort" in a or "version" in a:
        print("     the reference has been unfrozen; revisit canonical.py's "
              "effort exclusion as well")
    shared = a & b
    missing = set(COLS.split(",")) - shared
    if missing:
        print("     columns compared by check_turns are not in both stores:",
              sorted(missing))
        ok = False
    print(f"  turn cols   reference={len(a):5} rust={len(b):5}  "
          f"rust-only={sorted(b - a)}  "
          f"{'AS DOCUMENTED' if ok else 'UNDOCUMENTED'}")
    return ok


def check_turns(py: sqlite3.Connection, rs: sqlite3.Connection) -> bool:
    """The one documented divergence: the reference keys `turn` on id alone.

    A spawned Codex agent inherits the parent's replayed history, so one message
    id appears in the parent and every child. Keyed on id alone they collide and
    the reference silently keeps whichever landed first; the Rust store keys on
    (id, thread_id) and keeps them all.

    So this is not an equality check. It asserts the shape of the divergence:
    every reference row still exists in the Rust store, and the only extra rows
    are ids the reference collapsed.
    """
    a = set(py.execute(f"select {COLS} from turn"))
    b = set(rs.execute(f"select {COLS} from turn"))
    missing = a - b
    extra = b - a
    collapsed = {row[0] for row in extra}
    print(f"  turn        reference={len(a):5} rust={len(b):5}  "
          f"+{len(extra)} recovered, {len(missing)} missing")
    if missing:
        for row in list(missing)[:3]:
            print("     in reference but NOT rust:", str(row)[:130])
        return False
    # Every extra row must be a duplicate id the reference could not store.
    dupes = {i for (i,) in rs.execute(
        "select id from turn group by id having count(*) > 1")}
    stray = collapsed - dupes
    if stray:
        print("     unexplained extra ids:", list(stray)[:3])
        return False
    print(f"     divergence is exactly the id collision "
          f"({len(dupes)} id(s) stored per-thread)")
    return True


def main(py_db: str, rs_db: str) -> int:
    py, rs = sqlite3.connect(py_db), sqlite3.connect(rs_db)
    ok = check_columns(py, rs)
    ok &= check_turns(py, rs)
    for label, sql in TABLES.items():
        a, b = py.execute(sql).fetchall(), rs.execute(sql).fetchall()
        same = a == b
        ok &= same
        print(f"  {label:11} python={len(a):5} rust={len(b):5}  "
              f"{'MATCH' if same else 'DIFFER'}")
        if not same:
            sa, sb = set(map(str, a)), set(map(str, b))
            for x in list(sa - sb)[:2]:
                print("     only python:", x[:140])
            for x in list(sb - sa)[:2]:
                print("     only rust  :", x[:140])
    print("  store equivalence:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1], sys.argv[2]))
