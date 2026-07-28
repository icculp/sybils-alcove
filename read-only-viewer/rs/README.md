# alcove — Rust core

The normaliser: the part that turns two undocumented, shifting transcript
formats into one vocabulary. This is a **port in progress**, not a replacement.

## Why port at all

Not speed. The Python is 2,000 lines of stdlib and reads 146 MB in 1.5s, which
is fine. The reasons are:

- **The formats are Rust's already.** Codex ships `sqlx` and `opentelemetry_sdk`,
  and its rollouts are serde's internally-tagged enum representation —
  `{"type": "response_item", "payload": {"type": "message", …}}`. Modelling that
  with serde is re-deriving the producer's own types rather than guessing at a
  dynamic blob.
- **Loud failure at a known point.** The Python reads content blocks with
  `.get()` chains that return `""` when a shape surprises them, which is exactly
  how several real bugs hid. Naming both shapes (see `Content` in `model.rs`)
  turns an unhandled variant into a failure with a location.
- **A single 2.4 MB binary** with no runtime, versus "needs python3".
- **Cores.** Scanning transcripts is pure CPU over independent files — what the
  GIL serialises. 6.6x on 8 cores, and it is not the last word: parsing still
  goes through `serde_json::Value` rather than typed structs.

Honest accounting: of ~10 shape bugs found while writing the Python, roughly two
were the kind a type system catches outright. `turn_context` being per-session,
`message.id` repeating across content blocks, and `tokens_used` being a
quadratic lie are semantic, and Rust would have shipped them too.

## The equivalence gate

The port is only safe because both implementations must agree byte for byte:

    tools/equivalence.sh --freeze /tmp/fixture   # snapshot the live corpus
    tools/equivalence.sh /tmp/fixture            # diff Python vs Rust

**It must run against a frozen fixture.** Agents append to transcripts
continuously, so two runs seconds apart legitimately differ in `last_ts` and
token totals — which reads exactly like a port bug and burned real time before
the gate enforced it.

The canonical snapshot excludes everything volatile (wall clock, file ages,
liveness, pids, process state) because those differ between two runs of the
*same* implementation. What remains is the parsing facts, which is what a port
can get wrong.

## Status

Ported: transcript reading, the shared vocabulary, both harness scanners,
subagent nesting, the canonical snapshot.

Not ported, still Python and still the shipping implementation: the HTTP server,
the sqlite store, spillout, the activity charts, process/pid liveness, and the
Codex `state_<N>.sqlite` enrichment. The gate runs with that enrichment disabled
on the Python side so the two are compared like for like.
