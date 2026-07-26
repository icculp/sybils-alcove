"""Sybil's Alcove — read-only view of local coding-agent sessions.

Layout, in dependency order:

    config       environment settings, resolved once
    transcripts  reading JSONL off disk (tail/head/chronology)
    model        the vocabulary both harnesses map into
    sources/     one adapter per data origin — claude, codex, process
    collect      one snapshot across every source; decides session state
    web          auth, static assets, JSON API

`sources/` and `model` are pure functions over bytes, so they are testable
without a server. That matters here: every bug this project has had was a field
that parsed cleanly and meant something else.
"""

__all__ = ["config", "transcripts", "model", "collect", "web"]
