# kioku — SPEC-M3.1: handoff consumption semantics and cheaper, better search

Status: spec, 2026-10-01. Amends SPEC-M1 §6 (search) and §7.1 (handoff acceptance),
SPEC-M2.4 §1.4 (lane routing), SPEC-M2.6 §6. Read CLAUDE.md, SPEC-M1.md §6/§7,
SPEC-M2.4.md, SPEC-M3.0.md first. Source: the v0.8.0 audit. Depends on M3.0 (sections
3–7 of the block, `assistant` observations, `sessions.machine`).

## 1. Who consumes a handoff (SPEC-M2.4 §1.4 extended)

Today every SessionStart except `offline-replay` routes and accepts the newest pending
handoff of its lane. Three cases are wrong:

1. **compact / resume of a known session** (`source ∈ {compact, resume}`
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

## 5. Implementation notes (2026-10-02, branch `m3.1-handoffs-search`)

Where the spec was impossible, silent or had to be made concrete.

### Handoffs (§1)

1. **Resume detection.** Rule 1 applies when `source ∈ {compact, resume}` *or* the
   session id is already in `sessions` (checked before the upsert): a `startup` with a
   known id resumes too. Claude Code's `/clear` starts a new session id, so it is a new
   session and accepts the lane's pending handoff (review of PR #11; `clear` with a known
   id still resumes through the known-id check).
   A resumed session gets `db::newest_handoff_accepted_by` (same project and lane).
2. **Own handoffs** (rule 2) are left out of every pending lookup that names a session —
   SessionStart, `kioku_handoff_pending(session=…)` peeks and explicit accepts — and an
   accept never supersedes the acceptor's own pending handoffs (they wait for the next
   session).
3. **Busy lane** (rule 3): another session of the same project and lane with
   `status = open` and an observation whose `ts` is within the last 30 minutes
   (`ACTIVE_SESSION_MINUTES`). Observation `ts` comes from the client's clock (normalized
   by the server); there is no better timestamp. Claude Code finalizes a session on every
   Stop, so a Claude Code session idle between turns does not count as active; one in the
   middle of a turn does (and Codex / Gemini sessions until their Stop). Explicit
   `accept=true` ignores the busy lane, as specified.
4. **Why a handoff is a reference** travels as a new optional `reference_reason`
   (`main_line` / `concurrent` / `resumed`) on the SessionStart response and on
   `PendingHandoff`; the block renders 「## 未受領の引き継ぎ（参考）」 with the spec's sentence
   (or a resumed-session sentence), the main-line heading otherwise. A client older than
   this ignores the field and labels a same-lane reference as the main line's (cosmetic
   only; nothing is accepted either way). An unknown future reason falls back to the
   main-line heading.
5. **Superseding** writes `accepted_by = "superseded"` (with `accepted_at`); rows
   superseded before this release keep the session id they were given.
6. **History** is `PendingHandoff.history` (serde default, skipped when empty);
   `GET /handoffs/pending?history=N` returns full rows, the MCP text cuts each handoff at
   1,500 chars and shows `pending` / `accepted by <session>` / `superseded`.

### Search (§2)

7. **Index switch.** tantivy cannot open an index whose schema differs, so the old index
   cannot be "rebuilt in place while it keeps serving" (SPEC-M2.8 §5). Each schema version
   ≥ 3 lives in its own directory, `index/tantivy-v3/`; the schema-2 index in
   `index/tantivy/` is opened with its own schema (fields looked up by name; `code`,
   `ja_bigram` and `machine` absent) and serves searches and writes until the rebuild the
   server runs after listening fills the new directory and switches to it (one
   `RwLock<Arc<…>>` swap). The old directory is then deleted — best effort, since Windows
   keeps a directory busy while a search in flight still maps it; the next start retries.
   `index_outdated()` is also true while an old index is served.
8. **Re-ranking** as specified; `Hit.score` is now the re-ranked score. Recency uses the
   page's `updated`, against an injectable "now" (the evaluation pins it so it stays
   deterministic). `since` is a `YYYY-MM-DD` day in UTC, clamped below 2200 (tantivy keeps
   dates as i64 nanoseconds); `kinds` accepts a list (MCP) or a comma list (HTTP,
   `kioku search --kind`, repeatable). Bad values are a 400 / tool error.
9. **`code` field.** Identifier runs `[A-Za-z0-9_]+` joined by `::`, `/`, `.`, `-` (after
   NFKC) yield the whole run, its `/` and `::` segments, its words and their `snake_case` /
   `camelCase` parts. Two adjustments were needed for "found by their parts *and whole*"
   to rank right (without them a page that merely repeats `write` and `lock` outranked
   the one naming `write_lock`, see the evaluation's `handoff-words` / `lock-words`
   distractors): the field scores by presence (no term frequencies, no length
   normalization), the whole identifier is weighted 0.7 and the parts of a compound
   0.35, and the `ja` clauses of those parts are left out (the whole of a one-word
   identifier such as `WireGuard` stays a `ja` term).
10. **User dictionary format.** "lindera CSV: surface, cost, part of speech, reading" is
    not a format lindera reads: it takes the 3-column simple form (`surface,pos,reading`,
    fixed cost) or the full 13-column IPADIC row. kioku reads its own
    `surface,cost,pos,reading[,synonym_of]` (an empty cost = lindera's default -10000;
    lindera's 3-column form is accepted too), skips `#` comments and blank lines (lindera's
    CSV reader has neither), and writes the full IPADIC rows to
    `index/user-dict.lindera.csv` for `load_user_dictionary_from_csv`. Broken lines are
    logged and skipped; a broken file falls back to IPADIC only.
11. **User dictionary semantics.** With `Segmenter::new(.., Some(user_dict))` alone a
    dictionary word hides its parts: 引き継ぎ書 in the starter dictionary would stop
    `引き継ぎ` from finding every page that says 引き継ぎ書. The `ja` analyzer therefore
    keeps the IPADIC tokens and adds the user-dictionary segmentation's new tokens at the
    position of the first IPADIC token they overlap. "Synonyms via the user dictionary"
    needed a mechanism lindera does not have: the optional fifth column (a kioku extension)
    also indexes and searches the word as that word, at the same position. The starter
    file (kioku's terms plus 引継 / 引継ぎ / 引継書 as synonyms) is written by `kioku init`
    (and `kioku setup`, which runs it) only when missing; an existing install gets one by
    running `kioku init`. Staleness is `dict/user.csv` mtime > `index/schema-version`
    mtime, reported as `StatusReport.user_dict_stale`; `kioku doctor` warns with
    `kioku reindex` as the fix, and `reindex` re-reads the dictionary.
12. **Bigram fallback.** Runs only when the word query has no hit after the filters;
    needs ⌈75%⌉ of the query's distinct bigrams (a typo or a truncated word still
    matches); bigrams never cross spaces or punctuation. To resolve 「引継」 the field also
    holds the bigrams of each kanji "skeleton" (kanji joined across ≤ 2 hiragana of
    okurigana: 引き継ぎ書 → 引継書 → 引継, 継書). A one-character query has no bigram and no
    fallback. The snippet marks the first query bigram found verbatim, else shows the
    beginning of the body.
13. **Machine in result lines**: a stored `machine` field (from the session page's
    frontmatter) feeds `Hit.machine`; lines read `(session, 2026-10-01, @mini)`.
14. **Evaluation corpus**: 53 pages, 53 queries. Recall@3 is `|relevant ∩ top 3| /
    min(|relevant|, 3)` (one query has five relevant pages). Result on this branch:
    **Recall@3 = 1.000, MRR@3 = 0.991**, every `top` query first, three `partial`
    queries flagged partial, slowest query ≈ 3 ms on an M-series Mac (the test asserts
    < 500 ms). With the `code` field, recency or the pinned boost switched off, five
    `top` queries fail (write_lock, index.rs, both old/new duplicates, the pinned page),
    so each feature is load-bearing.

### Sessions of a path (§3)

15. `kioku_query.query` became optional; `path_prefix` alone lists sessions, with a query
    both are returned (sessions first). Only sessions with a cached digest (finalized at
    least once) are seen. A file matches when its project-relative path — or any part of an
    absolute one after a `/` — starts with the prefix (`\` folded to `/`, a leading `./`
    dropped), so a directory, a file or a name prefix all work. SQL pre-filters on the
    prefix's last segment (`instr` on `digest_json`), scanning at most the newest 2,000
    sessions; each lists ≤ 5 matching files (most edited first) and the summary of its
    newest handoff (the agent's, else the rule-based one). `GET /search` takes
    `path_prefix` without `q` and adds `sessions`; an older server ignores the parameter,
    so the `kioku mcp` bridge and `kioku search` report that the server predates it
    instead of an empty list. Without `path_prefix`, a missing `q` is still a 400 and an
    empty `q` still answers `{hits: []}`.

### Other

16. **M3.0 review item:** carried items de-duplicated by NFKC text keep the newest
    occurrence's place, date and handoff id but the **earliest** occurrence's wording.
17. **Index writer.** A rebuild keeps the existing `IndexWriter` (the new analyzer is
    registered on the index, so new segments use it) instead of dropping and reopening
    it: a writer's lock file handle can be held for a moment by a `git` child forked
    meanwhile, which made the reopen fail with `LockBusy` in a parallel test run. Opening a
    writer also retries `LockBusy` for up to 2 s.
18. **Windows "Access is denied" on commit** (fixed after v0.9.3, branch
    `fix-windows-index-flake`). Every tantivy commit writes `meta.json` to a temp file and
    renames it over the old one (`MoveFileExW(MOVEFILE_REPLACE_EXISTING)`), which Windows
    refuses with os error 5 while any handle has the target open. The reader was opened with
    `ReloadPolicy::OnCommitWithDelay`, whose watcher thread opens `meta.json` every 500 ms
    (immediately at reader creation, and for a moment after the index is dropped), and the
    reload after a commit ran outside the writer lock, so another thread's commit could
    land on it. The reader is now `Manual` (kioku is the only writer and reloads after
    each of its commits, under the writer lock), and a writer being retired (index switch,
    drop) waits for its merge threads, so nothing of the process keeps files of a directory
    that is removed or reopened. The old index directory is now removed on Windows too
    once no search maps it; the next-start retry stays for searches in flight.
19. **Not verified on real machines:** the upgrade of a real v0.9 data directory
    (schema-2 index → `tantivy-v3`, removal of the old directory, especially on Windows
    with searches in flight), the busy-lane rule with two real agents on one branch
    (Claude Code's per-turn finalize makes it apply only mid-turn), `/clear` and
    `/compact` in Claude Code and Codex against the new rule 1, and index build time and
    size with the extra fields on a large wiki.
