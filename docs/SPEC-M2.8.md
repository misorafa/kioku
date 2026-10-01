# kioku — SPEC-M2.8: per-turn cost, retention and operational hygiene

Status: spec, 2026-10-01. Amends SPEC-M1 §7 (finalize), SPEC-M2 (service, doctor),
SPEC-M2.6 §2 (backup). Read CLAUDE.md, SPEC-M1.md §7, SPEC-M2.6.md and SPEC-M2.7.md
first. Source: the v0.8.0 audit (kioku page "kioku v0.8.0 監査と M3 ロードマップ").
Depends on M2.7 being merged (schema version, data-dir lock).

Goal: the cost of one agent turn is O(what changed in that turn), storage has a ceiling,
and an unattended server keeps itself healthy. No change to what agents see.

## 1. Incremental finalize (audit CORE-中-1)

Today `finalize` re-reads and re-parses every observation of the session, and
`write_state` re-digests the last 10 sessions, on every Stop — O(n²) per session, inside
the write lock.

- `sessions` gains `digest_json TEXT NULL` and `digest_seq INTEGER NULL`: the
  `SessionDigest` as of `digest_seq`. `finalize` loads it and folds in only observations
  with `seq > digest_seq` (a pure `SessionDigest::extend(&[Observation])`), then stores
  the new digest. A session without a cached digest is digested from scratch once.
- `write_state` reads cached digests for finalized sessions; only the current session is
  recomputed (it is, by the step above).
- `agent_handoff`, counts and timestamps must be identical to a from-scratch digest.
  Test: for a 300-observation session, finalize after every 10 observations and assert
  the cached digest equals `digest_for` from scratch at each step (Japanese prompts and
  file paths in the fixture). Benchmark-style test: finalize of a 2,000-observation
  session with a warm cache parses ≤ 10 observations (count via a test hook / spy).

## 2. One commit per turn, none when nothing changed (audit CORE-中-5)

- `finalize` writes the session page and STATE.md, then makes **one** git commit for
  both and **one** tantivy commit (`upsert_many`). `put_page` keeps its own commit for
  agent `write_page`.
- STATE.md is written only when its content, excluding the `updated:` line, changed.
  Otherwise it is not touched, not committed, not reindexed.
- Session pages likewise: unchanged rendered body (excluding `updated:`) → no write.
- Tests: two consecutive finalizes with no new observations produce no new commit
  (`git rev-list --count`), one with new observations produces exactly one commit
  containing both files.

## 3. Retention: `[retention]`, `kioku prune`, `kioku forget` (audit CORE-中-6, PROD-6)

```toml
[retention]
raw_days = 90            # raw/<project>/<session>.jsonl older than this are gzipped, then deleted at 2×
observations_days = 180  # payloads of finalized sessions older than this are reduced to a stub
backups_keep = 10        # M2.7 §12 setting moves here
hook_dump_days = 7
```

- `kioku prune [--dry-run]` (server machine or `POST /api/v1/prune`, bearer): applies
  the policy and prints per-category counts and bytes freed. The server runs it daily
  (same scheduler as the update check, 10 min after the first update check), skipped
  when `[retention] auto = false`.
- "Reduced to a stub": the observation row keeps `kind, ts, seq, event_id` and a
  `payload` of `{"tool_name", "path"|"command" (first line), "is_error"}` — enough for
  the digest; `text` is cleared. The cached digest (§1) makes this safe for sessions that
  already have one; a session without a cached digest is digested before reduction.
- `kioku forget --session <id> [--purge-history]` (server machine): deletes the
  session's observations, receipts, raw log, session page (git commit "kioku: forget
  session <id>"), handoffs it authored (accepted or not), reindexes, rewrites STATE.md.
  `--purge-history` additionally runs `git filter-repo`-free history rewriting is **out
  of scope**: print the exact commands for the user instead. `kioku forget --project
  <id>` removes everything of a project the same way (confirmation prompt unless
  `--yes`).
- `kioku status` and `GET /status` report sizes: db bytes, raw bytes, wiki bytes,
  backups bytes, index bytes, oldest raw log date, last prune.
- Tests: prune on a fixture with old/new sessions (dry-run changes nothing; real run
  gzips/reduces exactly the old ones; the digest of a reduced session equals its cached
  digest); forget removes everything and search no longer finds the page (Japanese
  query); sizes in status.

## 4. Backup without a long write lock (audit CORE-中-4)

- The wiki's `.git` is captured with `git bundle create <stage>/wiki.bundle --all` run
  **outside** the write lock; only the `.md` working tree is copied under the lock
  (small). The manifest records the bundle's HEAD commit id.
- Restore: `git clone <bundle>` into `wiki/` then overlay the `.md` files; verify HEAD
  matches the manifest.
- A backup must still be consistent: the bundle may be a few commits *ahead* of the copied
  `.md` files (commits made after the copy) — document this and verify in restore that
  every copied `.md` exists in the bundle's HEAD tree or later.
- Tests: backup while another thread finalizes repeatedly: write-lock hold time of the
  backup < 1 s (measure with a test timer) and restore succeeds; existing backup tests
  still pass.

## 5. Self-maintaining server (audit CORE-低-7, CORE-低-1/2, CLI-M4)

- Index schema outdated at start → reindex in `spawn_blocking` after listening (search
  serves the old index until the swap); log start/end. `doctor` no longer tells the user
  to run `kioku reindex` for this case.
- Startup sweep (under the write lock, before listening): delete `wiki/**/.*.tmp` left
  by interrupted `write_atomic`; run the cheap consistency check (`pages.hash` vs file
  hash only) and upsert the differing pages into DB and index (self-heal); log counts.
- `write_atomic` fsyncs the parent directory after rename on Unix.
- Headless Mac: `kioku service install` and `doctor` detect that `launchctl print
  gui/<uid>` fails and say (ja/en): "この Mac にログインしているユーザーセッションがありません。
  自動ログインを有効にするか、`kioku service install --daemon` を使ってください".
  `--daemon` writes `/Library/LaunchDaemons/dev.kioku.serve.plist` content to stdout
  and prints the two `sudo` commands to install it (never runs sudo itself). The daemon
  plist runs as the current user (`UserName`), `KIOKU_SERVICE=1`, same paths.
- Tests: startup sweep removes `.tmp` and heals a page whose row hash differs; outdated
  index triggers reindex (observable via index version after start); daemon plist
  content golden test.

## 6. Digest quality fixes (audit CORE-低-3/4)

- `is_error_response`: an error only when `is_error == true`, or `exit_code`/`code` is a
  non-zero integer, or `stderr`/`error` **starts** with `error`, `Error`, `fatal:`,
  `panicked`, `Traceback`, `FAILED`. Body substring matches are removed.
- Prompt titles strip `<command-name>…</command-name>`, `<command-message>…`,
  `<system-reminder>…</system-reminder>`, `<pasted_content …>` blocks before truncation;
  multi-line prompts use the first non-empty line.
- `git_commits` deduplicated (keep first); `commands` keep the first 10 and the last 20
  instead of the last 30.
- Tests: reading `error.rs` is not an error; `cargo test` output with `test result: ok`
  is not; a Bash response with `exit_code: 1` is; title of a prompt starting with a
  `<command-name>` block.

## 7. hook-dump hygiene (audit CLI-L7)

- `KIOKU_HOOK_DUMP=1` records `state/hook-dump-enabled-at`; dumping stops by itself 24 h
  later (doctor shows "hook dump expired; re-enable to capture"). `kioku hook-dump
  extract` writes to `~/.kioku/captures/<date>/` by default, never into the cwd.
- Tests: expiry; default output dir.

## 8. Deliverables

Branch `m2.8-hygiene`, draft PR against `main`, CI green. README / README.ja: retention
settings, `kioku prune`, `kioku forget`, headless Mac note, backup bundle note. Record
deviations in §9. Do not merge, tag or change secrets.
