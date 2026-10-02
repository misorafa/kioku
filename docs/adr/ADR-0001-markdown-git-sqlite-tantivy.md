# ADR-0001: Markdown in git as the source of truth, SQLite for state, tantivy for search; no LLM in the server

Status: accepted (M1, 2026-09; recorded 2026-10-02 in SPEC-M3.2 §4)

## Context

kioku is one person's memory for every coding agent on every machine. It must stay
readable and repairable without kioku itself, survive years of use on a small home
server, search Japanese properly, and cost nothing per agent turn. Graph databases,
Postgres and vector stores were considered and rejected for that scale and audience.

## Decision

- **Page content** (session pages, `STATE.md`, pages written with `kioku_write_page`) is
  plain Markdown with YAML frontmatter in `wiki/`, a git repository; every write is a
  commit (one per turn, SPEC-M2.8 §2). It is the source of truth for content: any editor
  can read and fix it, `kioku reindex` rebuilds the rest from it.
- **State** (projects, sessions, observations, handoffs, receipts, aliases) lives in one
  SQLite file (`rusqlite`, bundled, WAL), versioned with `PRAGMA user_version`
  (SPEC-M2.7 §5).
- **Search** is a tantivy index with the lindera (IPADIC) Japanese analyzer, NFKC
  normalisation, an identifier field and a character-bigram fallback (SPEC-M3.1 §2). It is
  derived data: rebuildable from `wiki/` at any time.
- **No LLM calls in the server.** Session digests, session pages, `STATE.md` and the
  automatic handoff are built by rules (SPEC-M1 §7.2); judgement (summaries, decisions,
  open questions) comes from the agent through `kioku_handoff_write`.
- Git is shelled out to (no `git2`); a missing `git` only disables history.

## Consequences

- A user can `cat`, `grep`, edit and `git log` their memory; backups are a directory copy
  plus a git bundle (SPEC-M2.8 §4).
- Sessions and handoffs are *not* recoverable from Markdown alone — backups must include
  SQLite (`kioku backup`).
- Retrieval is lexical (BM25 + re-rank); semantic search (embeddings) is a later, optional
  layer and must not replace the Markdown/SQLite truth.
- Summaries are only as good as rules plus what the agent writes; the Stop nudge
  (SPEC-M3.0 §4) asks the agent for the parts rules cannot know.
- Running costs are zero per turn and the server works offline.
