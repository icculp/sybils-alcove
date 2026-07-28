# Rust port — status

**Goal:** one static binary plus the JS. Not a hybrid — Python is deleted at the
end, and this document is deleted with it.

The Python is the shipping implementation until step 3 below lands. Nothing has
switched over.

## Layers

| layer | Python | Rust | ships today |
|---|---|---|---|
| transcript read / normalise | ✅ | ✅ | Python |
| incremental scan | ⬜ | ✅ | — (Rust only, by decision) |
| HTTP server + `/api/sessions` | ✅ | ✅ | Python |
| pid liveness | ✅ | ✅ | Python |
| store (sqlite) | ✅ | ⬜ | Python |
| spillout | ✅ | ⬜ | Python |
| activity | ✅ | ⬜ | Python |
| Codex `state_<N>.sqlite` | ✅ | ⬜ | Python |
| browser UI (JS/CSS) | — | — | stays as-is, not ported |

The Rust binary serves the session list today. It is not the shipping server yet
because `/spill` and `/activity` are still Python — those routes return 501 with
a message saying so, rather than a 404 that looks like a typo.

## Order of work

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
3. **store** — rusqlite, bundled. Needed for activity, and where the schema
   decision gets made.
4. spillout, activity, Codex sqlite — mechanical once 1–3 land.
5. delete the Python, move `rs/*` up a level, delete the gate and this file.

## The equivalence gate

    tools/equivalence.sh --freeze /tmp/fixture   # snapshot the live corpus
    tools/equivalence.sh /tmp/fixture            # diff Python vs Rust

It exists to make replacement safe, and it is temporary. Two properties:

- **cross-implementation** — Python and Rust produce byte-identical canonical
  snapshots. Passing over 31 sessions.
- **incremental vs cold** (once step 2 lands) — an incremental scan must equal a
  cold full rescan of the same corpus. This is the property that keeps a stale
  offset from silently hiding events.

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
- **Codex's private sqlite is disabled on the Python side during the gate**, so
  the two are compared like for like until it is ported.

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
