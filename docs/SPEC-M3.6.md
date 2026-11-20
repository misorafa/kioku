# kioku — SPEC-M3.6: fresh memory at session start, less noise in search

Status: spec, 2026-11-20. Amends SPEC-M3.1 §1 rule 1 (what a resumed session gets back),
SPEC-M3.0 §1 section 4 and §2 (carried open questions, handoff schema), SPEC-M3.1 §2
(search query). Read CLAUDE.md, docs/INDEX.md, those sections and
`crates/kioku-core/src/{store.rs,db.rs,carry.rs,handoff.rs,render.rs,index.rs,index/analysis.rs,index/evaluation.rs}`
and `crates/kioku-cli/src/context.rs`, `crates/kioku-server/src/mcp.rs` first. Source: the
feature review of 2026-11-20, three observations in one long-lived session of the author
(session started 2026-09-27, compacted on 2026-11-20):

- After the compact, the block's 「前回からの引き継ぎ」 was the handoff this session had
  accepted on **2026-10-02** (seven weeks old, about a problem long fixed), although the
  session itself had written an agent handoff minutes earlier. SPEC-M3.1 §1 rule 1 gives a
  resumed session back what it accepted; for a long-lived session that is the oldest thing
  it knows.
- The carried 「未解決」 still listed questions of 2026-10-07 that were settled weeks ago
  ("when to tag v1.0.0", "the release date"). A carried question only disappears when a
  later decision repeats its first 12 characters, or after 20 newer agent handoffs.
- `kioku_query` with 「競合 比較 claude-mem 優位性 宣伝 X 投稿」 returned 8 hits of which 4 were
  unrelated, the first one included: the single letter `X` matched `x86_64`, and 「性」
  (from 優位性) matched 脆弱性. Each hit matched **one** incidental token of a seven-word
  query.

The server API, the config format and the on-disk formats stay compatible within 1.x: one
optional request field, one optional response field, no DB or index schema change.

## 1. A resumed session gets back the newer of "accepted" and "its own" (SPEC-M3.1 §1 rule 1 amended)

For a resumed session (`source ∈ {compact, resume}` or a known session id), and in
`session_context` (the side-effect-free read of the same block):

- candidates: (a) the handoff the session accepted last on its project and lane (today's
  rule), (b) the session's **own newest agent handoff** (`source = 'agent'`,
  `session_id = this session`, same project and lane, any status — a superseded or
  accepted one counts, it is still what this session last said);
- the one with the newer `created_at` is returned as `pending_handoff`; on a tie (a) wins.
  Nothing is accepted, as before;
- when (b) is returned, `SessionStartResponse` carries `own_handoff: true`
  (`#[serde(default)]`, skipped when false), and the block renders section 2 under
  「このセッションの直近の引き継ぎ（再開）」 / "This session's latest handoff (resumed)" instead
  of the usual heading. An older client ignores the field and shows the usual heading; an
  older server omits it;
- with neither (a) nor (b), the lane's pending handoff is shown as a reference, as today.

A session's own **rules** handoff is not a candidate: it restates what the compact summary
and section 7 (last reply) already carry. SPEC-M3.1 §1 rule 2 (a session never *accepts*
its own handoff) is unchanged: this is a re-presentation, `accepted_by` is not touched.
Section 7 keeps its existing rule (hidden when section 2 came from the same session's
agent handoff). The carried sections exclude the items of whichever handoff is shown, as
today.

## 2. Carried open questions close (SPEC-M3.0 §1 section 4 and §2 amended)

1. **`resolved`**: `HandoffInput` gains optional `resolved: Vec<String>`（解決済み /
   Resolved）: carried open questions or gotchas that no longer hold, as their text or its
   leading part. Rendered in the handoff Markdown as its own list after 決定事項 (omitted
   when empty), parsed back by `carry::items` (ja and en titles), redacted like the other
   lists. In `carry`, an open question or a gotcha of handoff *H* is dropped when a handoff
   newer than *H* — or the shown one — has a `resolved` item *r* such that, on normalised
   text, `r == item`, or `r` starts with the item's first `RESOLVE_PREFIX` chars, or the
   item starts with `r` and `r` has at least `RESOLVE_PREFIX` chars. Resolved items are
   not themselves carried or shown in sections 3–4. `kioku_handoff_write` gets the
   parameter (MCP description ja/en: 「前回までの『未解決』『落とし穴』のうち、もう当てはまらない
   ものを書くと次回から表示されなくなる」); the HTTP `POST /handoffs` body accepts it; an older
   server ignores it (serde default).
2. **Age**: an open question whose handoff is older than `OPEN_QUESTION_DAYS = 30` days is
   not carried (constant in `carry.rs`; `carry` takes `now`). It stays in the handoff and
   in `kioku_handoff_pending(history)`; decisions, verified facts and gotchas have no age
   limit (a decision does not expire; a gotcha is closed with `resolved`).
3. The block's section 4, when it lists at least one item, ends with one hint line:
   「（解決したものは kioku_handoff_write の resolved に書くと消える）」 / "(list what is settled
   in `resolved` of kioku_handoff_write to clear it)". It counts against the section cap.
   STATE.md does not get the hint line.

## 3. Search: one-letter tokens and one-token matches (SPEC-M3.1 §2 amended)

Query side only; the index schema stays at version 3 and nothing is reindexed.

1. **One-letter tokens are noise**: `is_noise_token` also returns true for a token that is
   a single ASCII letter or digit (`X`, `a`, `1`). As for single hiragana today, noise
   tokens are dropped only when a meaningful token remains, so the query `X` alone still
   searches `x`. The same rule applies to the `code` field: a one-character part of
   `identifier_runs(query)` is not queried unless it is all the query has.
2. **Coverage**: let *n* be the number of distinct meaningful tokens of the query (after
   rule 1 and the identifier-part filter; a whole identifier of the `code` field counts as
   one token together with its parts). When *n* ≥ 3, a hit must match at least **2**
   of them, in any of title / body / `code` (tags do not count). Implement with one
   `BooleanQuery` of `Should` clauses per token, combined by
   `BooleanQuery::with_minimum_required_clauses(.., 2)`; the tag clauses and scoring stay
   as today. When that returns nothing, retry with the present any-one-token query, then
   the `ja_bigram` fallback as today (hits of the any-one retry are **not** flagged
   partial). Queries using tantivy syntax are unaffected.
3. **Evaluation**: add to `search-eval.json` at least 6 queries of five or more words
   whose expected page shares several words with the query while distractor pages share
   exactly one incidental token (include the observed query with a page about 競合比較 and
   distractors containing `x86_64` and 脆弱性; mixed ja/en; one with a one-letter token).
   Thresholds unchanged (Recall@3 ≥ 0.90, MRR@3 ≥ 0.80, every `top: true` first). Record
   Recall@3 / MRR@3 before and after in §5. **If rule 2 lowers either number on the
   existing corpus below its threshold, or makes a `top: true` query lose first place,
   keep rule 1, drop rule 2, and say so in §5** rather than loosening the corpus.

## 4. Deliverables

Branch `m3.6-fresh-memory`, commits on that branch only (the reviewer pushes it and opens
the PR). `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and all tests green. Do
not merge, tag, push or change secrets; do not run anything against a real server or the
real `~/.kioku`. Update: `docs/INDEX.md` (handoff routing, handoff contents, the `<kioku>`
block, search query and MCP tools rows point here too), one-line "Amended by SPEC-M3.6 §N"
notes at SPEC-M3.1 §1 and §2 and SPEC-M3.0 §1 and §2, `CHANGELOG.md` (Unreleased), README
and README.ja (the MCP tools table: `resolved`; the handoff consumption paragraph; the
Search list: coverage and one-letter tokens; the sample block if it shows section 4).
Tests (next to the code, `tempfile`, Japanese throughout):

- §1: a session accepts handoff A, writes agent handoff B, resumes with `compact` → gets B
  with `own_handoff`, nothing newly accepted, A still `accepted_by` that session; a session
  that wrote nothing gets A back as today; own rules handoff newer than A → still A; B on
  another lane is not a candidate; `session_context` agrees with the resume; block golden
  test for the new heading (ja and en) and for a response without the field.
- §2: `resolved` round trip (render → parse, ja and en); a resolved question and a resolved
  gotcha disappear from `carry`, an older `resolved` does not close a newer question; the
  three match forms; a 31-day-old question is not carried while a 31-day-old decision is;
  the hint line appears only with items; MCP and HTTP accept the field; a request without
  it behaves as before.
- §3: the observed query on a fixture — the 競合比較 page first, the `x86_64` and 脆弱性
  distractors absent; `X` alone still finds a page containing `x`; a five-word query whose
  only matching page shares one token is still found through the any-one retry and is not
  flagged partial; the evaluation thresholds.

Record deviations and causes found in §5.

## 5. Implementation notes

(to be filled by the implementation)
