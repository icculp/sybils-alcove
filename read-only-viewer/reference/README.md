# Reference implementation (Python)

**Frozen.** This is not the shipping viewer — `rs/` is. It is kept because it is
the most readable description of *how these transcript formats actually work*:
2,000 lines of standard-library Python, no dependencies, runnable with
`python3 reference/alcove.py`.

If you want to write your own tool against Claude Code or Codex transcripts,
read this rather than the Rust. Every non-obvious rule is commented where it is
enforced, and each one cost a measurement to learn:

- Claude writes one assistant event **per content block**, all repeating the same
  `message.id` *and* the same usage dict. Counting per event overstated one real
  session 1,757 turns vs 761 and its output 2.18M vs 0.82M.
- Codex `turn_context` is written **once per session**, not per turn. Counting it
  reported every Codex session as having taken exactly one turn.
- Codex token totals are **cumulative snapshots**. Summing them multiply-counts;
  the post-compaction figure is a subtraction, not a reset.
- Codex marks one compaction **twice**, milliseconds apart, so dedupe at second
  granularity.
- A subagent transcript is `isSidechain: true`; a parent that does not skip those
  absorbs its children's work.
- Codex `payload.id` is the thread's **own** id; `payload.session_id` on a spawned
  agent is its **parent's**. Reading `session_id` first collapses children into
  parents.
- Reasoning text is **not on disk** in either harness. 22,669 Claude `thinking`
  blocks carry only a signature; 22,428 Codex `reasoning` items carry only
  `encrypted_content`. Text present in exactly zero.
- `threads.tokens_used` in Codex's sqlite is a cumulative counter re-added per
  turn — one thread reports 848,292,502. Do not use it.

## Equivalence with the shipping implementation

Proven byte-for-byte at the freeze commit, over a 304-file / 983 MB corpus:

    reference/tools/equivalence.sh --freeze /tmp/fixture
    reference/tools/equivalence.sh /tmp/fixture

    PASS — byte-identical over 31 sessions
    PASS — snapshot deterministic across runs
    store equivalence: PASS

## Deliberate divergences after the freeze

The Rust implementation continues; this one does not. Differences introduced on
purpose are listed here, so a reader is never misled about which is correct:

- **`turn.id` collision (Rust fixed, reference not).** A spawned Codex agent
  inherits the parent's replayed history, so the same assistant message id
  appears in the parent and in every child, all stored under the parent's
  `session_id`. Measured: 1,967 rows generated, 1,961 distinct, **6 silently
  dropped (0.3%)**, worst id appearing 7 times across 7 threads. The Rust store
  adds a `thread_id` column and keys on `(id, thread_id)`. The reference keeps
  the original single-column key and therefore still drops those rows.

The browser UI in `../static/` is **shared**, not part of this reference — both
implementations serve the same files.
