# Rust port — status

**Goal:** one static binary plus the JS. The Python is **kept, frozen**, as a
reference implementation — see `reference/README.md`.

Every layer is ported. What remains is the cutover: pointing the systemd unit at
the Rust binary.

## Layers

| layer | Python | Rust | ships today |
|---|---|---|---|
| transcript read / normalise | ✅ | ✅ | Python |
| incremental scan | ⬜ | ✅ | — (Rust only, by decision) |
| HTTP server + `/api/sessions` | ✅ | ✅ | Python |
| pid liveness | ✅ | ✅ | Python |
| store (sqlite) + `/api/activity` | ✅ | ✅ | Python |
| spillout | ✅ | ✅ | Python |
| activity | ✅ | ✅ | Python |
| Codex `state_<N>.sqlite` | ✅ | ✅ | Python |
| browser UI (JS/CSS) | — | — | stays as-is, not ported |

**Every route is now ported.** The Rust binary serves the whole viewer; the
Python is still what the systemd unit runs, pending the cutover.

## Order of work

0. ~~normaliser~~, ~~server + pid liveness~~, ~~incremental~~, ~~store~~,
   ~~spillout~~, ~~Codex sqlite~~, ~~freeze~~ — all done. Remaining: cut the
   systemd unit over to the Rust binary.

1. **HTTP server + pid liveness** — incremental caching is worthless in a
   one-shot CLI, so the long-lived process has to exist first. At this point the
   Rust binary serves the page and Python stops shipping.
2. **incremental scan** — cache each file's scan keyed on `(size, mtime)` and
   re-read only files that moved. Written ONLY in Rust: biggest win in the
   project (146 MB re-read every 3 s to find ~2 KB of new events, 99.9984%
   waste) and a Python version would be deleted within days.

   Note this is *cache-unchanged-files*, not *parse-only-appended-bytes*. The
   scanners compute aggregates over the whole tail window (turns, usage, model
   timeline), so resuming mid-file would mean making every one of them
   resumable — a much larger change with real correctness risk. Stat-caching
   gets 146 MB down to ~1 MB per poll and rests on one provable claim: a file
   whose size and mtime are unchanged produces the same scan. True byte-offset
   incrementality stays available if that is ever not enough.
3. ~~**store**~~ — done. rusqlite bundled; `--ingest-only` and `/api/activity`.
4. ~~spillout, Codex sqlite~~ — done.
5. **freeze** the Python as a reference implementation, cut the unit over to the
   Rust binary, then fix the `turn.id` collision (a deliberate divergence, so it
   has to come after the freeze).

## The equivalence gate

    tools/equivalence.sh --freeze /tmp/fixture   # snapshot the live corpus
    tools/equivalence.sh /tmp/fixture            # diff Python vs Rust

It exists to make replacement safe, and it is temporary. Two properties:

- **cross-implementation** — Python and Rust produce byte-identical canonical
  snapshots. Passing over 31 sessions.
- **snapshot determinism** — repeated runs agree, so the parallel scan cannot
  reorder anything.
- **store equivalence** — both implementations ingest identical rows into
  `turn`, `selection`, `compaction`, `session`, `subagent`. This one earns its
  keep: snapshots carry no per-turn rows, so both sides agreed on every
  aggregate while writing DIFFERENT rows to the store, and only this check saw
  it.
- **incremental vs cold** — verified when incremental landed: appending one
  event produced exactly one cache miss, and the warm snapshot equalled a cold
  full rescan.

**Always run it against a frozen fixture.** Against the live transcript roots it
fails with `last_ts` minutes apart and token totals grown — agents appending to
their own transcripts mid-measurement, which reads exactly like a port bug.

The canonical snapshot excludes wall clock, file ages, liveness, pids and
process state, because those differ between two runs of the *same*
implementation.

## Decisions

- **Incremental is written in Rust only.** No Python version; it would be
  deleted within days.
- **The UI is not ported.** It is already the right language.
- **`rs/` stays inside `read-only-viewer/`.** A parallel top-level tree
  reproduces the "which one ships?" ambiguity this document exists to remove.
- **The Codex sqlite is now compared, not disabled.** `--freeze` copies
  `state_*.sqlite` into the fixture and both implementations are pointed at it,
  so neither reads the live file mid-gate.

## Fixed: Codex turn ids collide across threads

`turn.id` uses the natural key (`payload.id`), and the store's whole premise is
that this makes ingestion idempotent. For Codex that premise is 99.7% true and
not 100%: a spawned agent inherits the parent's replayed history, so the same
assistant message id can appear in the parent AND in every child thread. All of
them are stored under the parent's `session_id`, so they collide, and
`INSERT OR IGNORE` keeps whichever is written first.

Measured on the fixture: 1,967 Codex turn rows generated, 1,961 distinct ids,
**6 rows silently dropped (0.3%)**, worst id appearing 7 times across 7 threads.

That also made the two implementations disagree until Rust sorted subagents the
way Python does — the surviving row depended on iteration order, which is not a
property anything should depend on.

**Fixed in Rust after the freeze.** `turn` is keyed on `(id, thread_id)`, where
`thread_id` is the thread that actually produced the turn — the session for a
main thread, the subagent's own id for a child. Result: 8,787 -> 8,793 rows,
exactly the 6 that were being dropped, still idempotent (0 new on a second
ingest), and the worst id is now stored 7 times instead of 1.

The reference keeps the single-column key, so the gate's store check asserts the
SHAPE of the divergence rather than equality: every reference row must still
exist in the Rust store, and the only extra rows may be ids the reference
collapsed.

## Known complications

- **pid liveness shells out** to `claude agents --json --all` in any language.
  It is 562 ms of a 2.4 s cold collect, so once parsing is fast it dominates —
  the port is not a route around it. Wants its own cache with a longer TTL.
- **Static assets** must be embedded in the binary (`include_str!`) or the
  "single file" property is lost.
- **Codex's `state_<N>.sqlite` is WAL**, so a read-only open needs directory
  write access. The Python copies the file and opens the copy immutable; the
  Rust port has to do the same, not "just open it read-only".

## Measurements that justify any of this

| | |
|---|---|
| corpus | 304 files, 983 MB |
| python normalise, 1 core | 1.46 s |
| rust normalise, 8 cores | 0.22 s (6.6×) |
| rust normalise, 1 core | 0.65 s (2.1× — what the language alone buys) |
| redundant work per poll | **99.9984%** |
| binary | 2.4 MB, no runtime |
| rust cold collect (server) | 658 ms |
| rust warm collect (incremental) | **139 ms**, 264 cache hits / 0 misses |
| python full rescan, same moment | 1.4–4.7 s |
