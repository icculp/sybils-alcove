"""One adapter per data origin.

Each yields normalized facts in the `model` vocabulary and knows nothing about
the server. Adding a source (hooks spool, OTEL, Codex's SQLite log) means adding
a module here, not touching the ones that exist.
"""
