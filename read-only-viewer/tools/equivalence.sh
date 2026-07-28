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

cd "$(dirname "$0")/.."

if [ "${1:-}" = "--freeze" ]; then
  out="${2:?usage: $0 --freeze <dir>}"
  rm -rf "$out"; mkdir -p "$out"
  cp -a "${ALCOVE_CLAUDE_ROOT:-$HOME/.claude/projects}" "$out/claude"
  cp -a "${ALCOVE_CODEX_ROOT:-$HOME/.codex/sessions}" "$out/codex"
  echo "frozen $(find "$out" -name '*.jsonl' | wc -l) files into $out"
  exit 0
fi

fixture="${1:?usage: $0 <fixture-dir>   (or --freeze <dir> to make one)}"
[ -d "$fixture/claude" ] || { echo "missing $fixture/claude"; exit 2; }
[ -d "$fixture/codex" ]  || { echo "missing $fixture/codex";  exit 2; }

export ALCOVE_CLAUDE_ROOT="$fixture/claude"
export ALCOVE_CODEX_ROOT="$fixture/codex"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

python3 tools/canonical.py > "$tmp/py.json"
./rs/target/release/alcove   > "$tmp/rs.json"

if diff -q "$tmp/py.json" "$tmp/rs.json" > /dev/null; then
  n=$(python3 -c "import json,sys;print(len(json.load(open('$tmp/py.json'))['sessions']))")
  echo "PASS — byte-identical over $n sessions"
  exit 0
fi

echo "FAIL — implementations diverge:"
diff "$tmp/py.json" "$tmp/rs.json" | head -60
exit 1
