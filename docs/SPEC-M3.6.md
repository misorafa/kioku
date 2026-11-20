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

## 5. Implementation notes (2026-11-20, branch `m3.6-fresh-memory`)

### A resumed session's own handoff (§1)

- `db::newest_own_agent_handoff` (session, project, lane, `source = 'agent'`, any status,
  the usual handoff order) and `store::resumed_handoff`, used by both `route_for_start`
  (resumed branch) and `Store::session_context`, so the two always agree. The newer
  `created_at` wins (parsed, not compared as text); on a tie the accepted one wins.
  `route_for_start` now returns the flag next to the `PendingHandoff`; the `PendingHandoff`
  type of `GET /handoffs/pending` is unchanged.
- `SessionStartResponse.own_handoff` is `#[serde(default, skip_serializing_if = "is_false")]`.
  The client renders the new heading in the SPEC-M3.0 layout **and** in the legacy layout
  (an older server never sends the flag, so this only matters for a hand-built response).
- With neither candidate, `route_for_start` keeps the resumed reference (lane's pending,
  `reference_reason = resumed`) and `session_context` keeps the main line's reference on
  a branch lane, as before.
- **Two existing expectations changed, as the amendment intends:**
  `store::handoff_tests::a_session_never_accepts_its_own_handoff` restarted the same
  session id with `startup` after it wrote a handoff and expected nothing back; a known id
  is a resume (SPEC-M3.1 §1 rule 1), so it now gets that handoff back with `own_handoff`,
  still unaccepted (the rest of the test — never accepted, the next session receives it —
  is unchanged). `kioku-server` `tests/http_api.rs::session_context_is_read_only` expected
  `pending_handoff: null` for a session that accepted nothing but had written an agent
  handoff; it now expects that handoff with `own_handoff: true`, and a new session that
  wrote nothing (started by an offline replay) checks the `null` case.

### `resolved` and the age limit (§2)

- `HandoffInput.resolved` (serde default, skipped when empty), redacted in `write_handoff`
  like the other lists, rendered as `### 解決済み` / `### Resolved` **directly after
  決定事項** (before 確認済みの事実 / 落とし穴), only when non-empty, so a handoff without it
  renders byte-for-byte as before. `carry::handoff_items` reads it back
  (`HandoffItems.resolved`).
- `carry::resolves(r, item)` implements the three match forms on normalised text.
  The closers of handoff *i* are the `resolved` items of `shown` and of the handoffs newer
  than *i*; a handoff's own `resolved` does not close its own items. As with the existing
  decision rule, when the newest occurrence of an item is closed, older duplicates of it
  are not carried either.
- `carry` takes `now`; `SourceHandoff` gained `created_at` (RFC 3339; unparsable counts
  as new). The store passes `util::now()` both for the block and for STATE.md, so
  STATE.md drops a 30-day-old question the next time it is rewritten, not at the moment
  it turns 30 days old.
- Hint line: the section is rendered with `OPEN_QUESTIONS_CAP` minus the hint's length,
  then the hint is appended, so it counts against the cap and comes after a
  `…(N more)` line. Not shown when the section is empty; STATE.md (`render::state_body`)
  does not get it.
- MCP: `resolved` is a field of `HandoffWriteParams` (ja/en description as specified),
  shared by the server's MCP and the `kioku mcp` stdio bridge (which forwards it in the
  `POST /handoffs` body); the tool description mentions it too.

### Search (§3)

- `is_noise_token` also matches a single ASCII letter or digit. The `code` field drops
  one-character identifier parts when the query has a meaningful `ja` token or a longer
  identifier token; `X` alone still searches `x` in both fields.
- `term_queries` builds one *token group* per meaningful token: its title (×2) / body
  clauses, plus the `code` clauses of an identifier run whose forms include that token;
  a run with no matching `ja` token is its own group (whole + parts = one token). With
  ≥ `COVERAGE_MIN_TOKENS` (3) groups the first query is
  `Must(BooleanQuery::with_minimum_required_clauses(groups, MIN_COVERAGE = 2))` plus the
  tag clauses as `Should` (they score, they do not count); the second is today's flat OR
  query. `search_with` tries them in order and uses the bigram fallback only when both
  return nothing; snippets are generated from the query that produced the hits.
- **Clarification found by a test:** a user-dictionary synonym is emitted at the same
  position as its word, so 「記憶装置」 with the synonym `kioku` produced three tokens and
  coverage dropped pages that only had the synonym
  (`store::search_tests::user_dictionary_is_used_and_doctor_sees_it_change`). Tokens at
  the same position now form one group: *n* counts words, not their synonyms.
- Note: *n* counts analyzer tokens, so 脆弱性 is two (脆弱 + 性) and 「投稿 脆弱性」 is
  already a three-token query. That is what the spec says; it is mentioned because it is
  easy to misread as "words".
- Index schema stays at 3, DB schema unchanged; nothing is reindexed.

### Evaluation (§3 rule 3)

Seven queries of five to seven words added (11 pages: the 競合比較 page, `x86_64` and 脆弱性
distractors, Docker multi-arch, PowerShell signing, a C extension next to a "C drive"
page for the one-letter case, SQLite WAL, MCP timeout, an English rate-limit query next
to a page that shares only `limit`), all `top: true`.

| corpus | code | Recall@3 | MRR@3 |
|--------|------|----------|-------|
| 53 pages / 53 queries (before) | before M3.6 | 1.000 | 0.991 |
| 64 pages / 60 queries | before M3.6 | 1.000 | 0.992 |
| 64 pages / 60 queries | M3.6 (rules 1 and 2) | 1.000 | 0.992 |

No regression and every `top: true` query stays first, so rule 2 is kept. The old code
already put the expected pages first: the observed noise was in hits 2–8, which Recall@3
/ MRR@3 cannot see; `index::tests::observed_query_needs_two_tokens` checks it with
`limit = 8` (only the 競合比較 page is returned).

### Tests

`carry::tests::{resolved_items_close_questions_and_gotchas, resolved_match_forms,
old_open_questions_expire_but_decisions_do_not}` and the `resolved` round trip in
`items_round_trip_through_markdown_in_both_languages`;
`store::handoff_tests::{resumed_session_gets_its_own_newer_handoff,
resumed_session_without_own_agent_handoff_gets_the_accepted_one}` (both lanes; own rules
handoff newer than A, own agent handoff on another lane; `session_context`);
`store::context_tests::resolved_items_stop_being_carried` (block and STATE.md, no hint in
STATE.md); the redaction test in `store.rs` covers `resolved`;
`kioku-cli` `context::m36_tests::{own_handoff_heading_golden_ja_and_en,
response_without_own_handoff_uses_the_usual_heading, resolve_hint_only_with_items}` and
the updated M3.0 goldens (hint line); `kioku-server` `tests/mcp.rs` (`resolved` in the
schema with ja/en description, stored through the tool) and `tests/http_api.rs`
(`resolved` in the body, rendered after the decisions; a body without it renders as
before); `index::tests::{one_letter_tokens_are_noise_unless_alone,
observed_query_needs_two_tokens, long_query_with_one_shared_token_falls_back_to_any_one}`
and the evaluation.

### Other (not part of this spec, same branch)

- `test(install)`: `scripts/test-install.sh` asserted `lacks "iex"` on output that echoes
  the random mktemp directory; on macos-latest it was `tmp.NIwN3iexGu` and the check
  failed. The Git Bash check now asserts `lacks "ExecutionPolicy"` and `lacks "| iex"`
  (what it means: no `-ExecutionPolicy Bypass`, no `irm … | iex`), and the source-build
  check `lacks "downloading kioku-"` instead of `lacks "downloading"`. The other
  `lacks` / `has` needles in `test-install.sh` and `test-scripts.sh` contain spaces,
  brackets, `@`, `/` or non-ASCII and cannot occur in a mktemp name.
- `fix(cli)`: `update::tests::replace_verifies_the_checksum_before_touching_the_binary`
  failed on ubuntu-latest with `Text file busy (os error 26)`: the just-written binary is
  run for its `--version` probe while another test thread's fork still holds its write fd
  (ETXTBSY). A real `kioku update` can hit the same race, so the fix is in the product:
  `kioku_core::util::retry_text_busy` retries a spawn that fails with ETXTBSY (unix only,
  up to 10 tries 50 ms apart; every other error and Windows unchanged). It wraps the update
  probe and the spawn in `util::output_with_deadline`, which `update::binary_version` uses.
  `binary_version` probes `<exe>.prev`, written by an earlier update (no race in
  production), but its tests and the auto-update tests probe scripts written a moment
  before, so the helper lives in `kioku-core` next to `output_with_deadline` rather than in
  `update.rs`. Test: `util::tests::retry_text_busy_retries_only_etxtbsy_up_to_the_limit`.
- `docs`: the example project name in README / README.ja, SPEC-M1 §4, SPEC-M2 and the
  tests of `project.rs`, `install/block.rs` and `install/agents/tests.rs` is now the
  neutral `sample-app` (`sample-app-ace9dc4a`, `Sample_App` for the case variant); example
  strings only, no behaviour change.
