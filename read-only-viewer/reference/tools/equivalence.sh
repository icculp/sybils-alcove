#!/usr/bin/env bash
# The equivalence gate: the Python and Rust normalisers must produce byte-identical
# canonical snapshots over the same corpus.
#
# It MUST run against a frozen fixture, never the live transcript roots. Agents
# append to their transcripts continuously, so two runs seconds apart legitimately
# differ in last_ts and token totals — which reads exactly like a port bug and
# cost real debugging time before this was enforced here.
#
#   tools/equivalence.sh /path/to/fixture      # fixture/claude + fixture/codex
#   tools/equivalence.sh --freeze /path/to/out # snapshot the live corpus first
set -euo pipefail

# reference/tools -> read-only-viewer
cd "$(dirname "$0")/../.."

if [ "${1:-}" = "--freeze" ]; then
  out="${2:?usage: $0 --freeze <dir>}"
  rm -rf "$out"; mkdir -p "$out"
  cp -a "${ALCOVE_CLAUDE_ROOT:-$HOME/.claude/projects}" "$out/claude"
  cp -a "${ALCOVE_CODEX_ROOT:-$HOME/.codex/sessions}" "$out/codex"
  # Parent-link sidecars are a documented Rust-only feature; the Python
  # reference is frozen. Keep this gate scoped to the shared transcript parser.
  find "$out/codex" -name '*.alcove-parent.json' -delete
  # Codex's own sqlite is enrichment for BOTH implementations now, so it has to
  # be frozen too — reading the live one would let Codex change it mid-gate.
  mkdir -p "$out/codex-home"
  cp -a "${ALCOVE_CODEX_HOME:-$HOME/.codex}"/state_*.sqlite "$out/codex-home/" 2>/dev/null || true
  echo "frozen $(find "$out" -name '*.jsonl' | wc -l) files into $out"
  exit 0
fi

fixture="${1:?usage: $0 <fixture-dir>   (or --freeze <dir> to make one)}"
[ -d "$fixture/claude" ] || { echo "missing $fixture/claude"; exit 2; }
[ -d "$fixture/codex" ]  || { echo "missing $fixture/codex";  exit 2; }

export ALCOVE_CLAUDE_ROOT="$fixture/claude"
export ALCOVE_CODEX_ROOT="$fixture/codex"
export ALCOVE_CODEX_HOME="$fixture/codex-home"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

python3 reference/tools/canonical.py > "$tmp/py.json"
./rs/target/release/alcove --snapshot > "$tmp/rs.json"

if diff -q "$tmp/py.json" "$tmp/rs.json" > /dev/null; then
  n=$(python3 -c "import json,sys;print(len(json.load(open('$tmp/py.json'))['sessions']))")
  echo "PASS — byte-identical over $n sessions"

  # Second property: the incremental (warm) path must agree with a cold full
  # rescan. A stale cache entry would otherwise hide events silently, and the
  # cross-implementation diff above cannot see it — both --snapshot runs are cold.
  a="$(./rs/target/release/alcove --snapshot)"
  b="$(./rs/target/release/alcove --snapshot)"
  [ "$a" = "$b" ] || { echo "FAIL — snapshot not deterministic"; exit 1; }
  echo "PASS — snapshot deterministic across runs"

  # Third property: the two stores must agree. The snapshot gate cannot see
  # this — snapshots carry no per-turn rows, so both sides can agree on every
  # aggregate and still write different rows.
  ALCOVE_DB="$tmp/py.db" python3 reference/alcove.py --ingest-only > /dev/null
  ALCOVE_DB="$tmp/rs.db" ./rs/target/release/alcove --ingest-only > /dev/null
  python3 reference/tools/store_equivalence.py "$tmp/py.db" "$tmp/rs.db" || exit 1
  exit 0
fi

echo "FAIL — implementations diverge:"
diff "$tmp/py.json" "$tmp/rs.json" | head -60
exit 1
