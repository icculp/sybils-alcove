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

TABLES = {
    "turn": "select id,session_id,harness,ts,model,input,output,cache_read,"
            "cache_write,is_subagent from turn order by id",
    "selection": "select session_id,ts,model,requested from selection"
                 " order by session_id,ts,model",
    "compaction": "select session_id,ts,trigger,pre_tokens from compaction"
                  " order by session_id,ts",
    "session": "select id,harness,project,cwd,branch from session order by id",
    "subagent": "select id,session_id,harness,model,role from subagent order by id",
}


def main(py_db: str, rs_db: str) -> int:
    py, rs = sqlite3.connect(py_db), sqlite3.connect(rs_db)
    ok = True
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
