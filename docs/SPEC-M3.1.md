# kioku — SPEC-M3.1: handoff consumption semantics and cheaper, better search

Status: spec, 2026-10-01. Amends SPEC-M1 §6 (search) and §7.1 (handoff acceptance),
SPEC-M2.4 §1.4 (lane routing), SPEC-M2.6 §6. Read CLAUDE.md, SPEC-M1.md §6/§7,
SPEC-M2.4.md, SPEC-M3.0.md first. Source: the v0.8.0 audit. Depends on M3.0 (sections
3–7 of the block, `assistant` observations, `sessions.machine`).

## 1. Who consumes a handoff (SPEC-M2.4 §1.4 extended)

Today every SessionStart except `offline-replay` routes and accepts the newest pending
handoff of its lane. Three cases are wrong:

1. **compact / resume / clear of a known session** (`source ∈ {compact, resume, clear}`
   or the session id already exists): the session gets back **the handoff it accepted
   last** (same project and lane, `accepted_by = this session`) as `pending_handoff`,
   rendered under the usual heading, **and does not accept anything new**. If it never
   accepted one, it sees the newest pending handoff of its lane as a *reference* (not
   accepted). A genuinely new session id is unaffected.
2. **A session's own rules handoff**: `accept_pending_handoffs` skips handoffs whose
   `session_id` is the starting session itself (the Stop of the previous turn wrote it).
3. **Several open sessions on one lane** (another session of this project+lane has an
   observation within the last 30 minutes and is not finalized): the newest pending
   handoff is shown as a reference and **not accepted**; the block says 「同じブランチで
   別のセッションが作業中のため、引き継ぎは消費していません」/ "another session is active on
   this lane; the handoff was left pending". `accepted_by` stays empty until a session
   starts on an idle lane or `kioku_handoff_pending(accept=true)` is called explicitly.

Superseding: accepting marks older pending handoffs of the lane as `superseded` (new
`accepted_by = "superseded"` value) rather than "accepted by this session", so history is
honest. `kioku_handoff_pending` gains `history: N` (≤ 20) returning the last N handoffs of
the lane with their status; the MCP description tells agents to use it when the block's
carried decisions are not enough.

Tests: the matrix (new / compact / resume / second concurrent session / own rules
handoff / explicit accept) on two lanes; `history`; Japanese text throughout.

## 2. Search: re-rank, show time, index what matters (SPEC-M1 §6 extended)

Index schema version 3 (reindex automatically at start, M2.8 §5).

- **Re-rank**: fetch `3 × limit` BM25 hits, then order by
  `score × recency × kind_weight` where `recency = 0.5 ^ (age_days / 30)` floored at
  0.25, `kind_weight = page 1.0 / state 0.8 / session 0.6`, and a `pinned` tag ×1.5.
  Ties keep BM25 order. `kioku_query` gets an optional `since: "YYYY-MM-DD"` filter and
  `kinds: ["page", "session"]`.
- **Result lines** (MCP `format_hits`, `kioku search`): `N. <path> — <title> (<kind>,
  <YYYY-MM-DD>[, @machine for sessions]) <snippet>`; M3.0 §5 added date and kind — keep.
- **Identifiers**: a second analyzer field `code` on title+body that tokenizes on
  `[^A-Za-z0-9_]`, splits `snake_case` / `camelCase` / `::` / `/`, lowercases, and keeps
  the whole identifier as well; queried with OR alongside the Japanese fields (weight
  0.7). Tests: `kioku_handoff_write`, `Store::open`, `src/index.rs`, `write_lock` are
  found by their parts and whole.
- **User dictionary**: `<data_dir>/dict/user.csv` (lindera CSV: surface, cost, part of
  speech, reading) loaded into `Segmenter::new(.., Some(user_dict))` when present; a
  starter file is written by `kioku init` with kioku's own terms (引き継ぎ書, レーン,
  セッション, 観測, 索引, プロジェクト別名) and a comment explaining the format. Changing
  the file requires `kioku reindex` (doctor warns when the file is newer than the index).
- **Zero-hit fallback**: when the query returns nothing, retry against a character
  bigram field `ja_bigram` (title+body, NFKC) and mark the output `（部分一致）/ (partial
  match)`. This resolves SPEC-M1 §6.4's open "引継" case.
- **Evaluation corpus**: grow `search-eval.json` to ≥ 50 pages / ≥ 50 queries covering:
  identifiers, mixed ja/en, half-width katakana, synonyms via the user dictionary, old
  vs new duplicates (recency must win), pinned pages, zero-hit fallback. Thresholds:
  Recall@3 ≥ 0.90, MRR@3 ≥ 0.80, and every `top: true` query first.

## 3. `kioku_query` for sessions of a file

`kioku_query` accepts `path_prefix` (e.g. `crates/kioku-core/src/store.rs`): returns
the sessions (from the digest cache, M2.8 §1) that edited files under it, newest first,
with their titles and handoff summaries. Cheap "who touched this and why" lookup without
embeddings. Test with Japanese titles.

## 4. Deliverables

Branch `m3.1-handoffs-search`, draft PR against `main`, CI green. README / README.ja:
handoff consumption rules (replace the "single-use" paragraph), search features (`since`,
`kinds`, `path_prefix`, user dictionary, partial match), the dictionary file. Update
SPEC-M1 §6.4 and §7.1 to point here. Record deviations in §5. Do not merge, tag or change
secrets.
