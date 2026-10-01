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

## 9. Implementation notes (2026-10-01, branch `m2.8-hygiene`)

What differs from §1–§7, or what the text left open, and why.

1. **Digest cache (§1).** `SessionDigest` carries a `tally` (edited / read files with counts
   in first-seen order, every distinct command, the root the paths were made relative to,
   the last folded seq); its lists are derived from it, so `extend` is exactly a continued
   fold and the step test compares the whole struct (tally included) with `digest_for`.
   `sessions.digest_json` (via `ADDED_COLUMNS`, `SCHEMA_VERSION` 4) also caches the
   addendum digest (observations after the agent handoff) keyed by the handoff id and its
   `seq_at`; a handoff without `seq_at` (pre-M2.4 rows) is always recomputed. A cache whose
   root differs from the session's current root (the project-root fallback moved, a merge)
   is rebuilt from scratch. The agent handoff is never cached (read fresh). The cached
   digest of a finalized session that only gained `stop` observations is extended
   (`cached_digest`); prompts/tool uses reopen the session anyway.
2. **One commit (§2).** STATE.md's recent-session list takes the title of the session being
   finalized from the page being written (its row does not exist yet in the same turn).
   When only STATE.md changed the commit message is `kioku: state <project>/STATE.md`;
   merge and forget rewrite STATE.md with the same "unchanged → no write" rule.
3. **Stubs (§3).** The literal `{"tool_name","path"|"command","is_error"}` is not enough for
   the digest (multi-file `apply_patch` edits, heredoc commit messages, prompts), so a stub
   is exactly what the fold reads: `{"stub": true, "tool_name", "path", "paths" (only when
   one call edited several files), "command" (first line, ≤ 160 chars), "git_commit",
   "is_error"}`; a prompt keeps its cleaned, truncated `prompt` (≤ 300 chars); `stop` /
   `compact` / `note` keep `{"stub": true}`. `text` is cleared. The digest reads stubs and
   full payloads alike, so a reduced session's digest equals its cached digest (tested).
   A session is reduced when it is finalized and its newest observation is older than
   `observations_days`. The bytes reported are payload/text bytes removed from rows; the
   SQLite file shrinks only after a `VACUUM`, which prune does not run (it would block
   recording on a large database). `0` days turns a category off.
4. **Raw logs (§3).** Age is the file's modification time (the last append). A gzipped log
   keeps that time, so it is deleted at 2 × `raw_days` after its last append; a `.jsonl`
   already older than 2 × is deleted without gzipping. gzip uses **`flate2`** (pure Rust,
   `miniz_oxide`), added as a direct dependency of kioku-core: it was already compiled into
   kioku through `lindera-dictionary`, pulls in no openssl, and gzip has no std
   implementation. A session resumed after its log was gzipped starts a new `.jsonl`.
5. **`prune` / `forget` transport (§3).** The running server holds the data directory lock
   (SPEC-M2.7 §6), so both commands are HTTP clients like `kioku backup`:
   `POST /api/v1/prune {dry_run?}` and `POST /api/v1/forget {session | project, dry_run?}`
   (bearer). `forget` also deletes the session row(s); `--project` asks for confirmation
   on a terminal and refuses without `--yes` otherwise (a dry run is shown first).
   `--purge-history` prints `git filter-repo` (and a `git filter-branch` fallback) plus the
   `reflog expire` / `gc` and `kioku service stop/start` lines for the server's wiki
   directory. The daily run starts in every `kioku serve` (also one started by hand), 10
   minutes after the first update check (`FIRST_PRUNE` = 20 min), then every
   `next_interval()`; the update check itself still runs only under the service manager.
   Hook dumps are pruned in the server's data directory only; a client-only machine's
   capture is bounded by §7's window and the 5 MiB rotation.
6. **Backup (§4).** Snapshot format 2: `wiki/` holds every working-tree file except `.git`
   and `.*.tmp` (not only `.md`), `wiki.bundle` the history, `wiki_head` the bundle's HEAD;
   no bundle when git is missing or the wiki has no commit yet. `VACUUM INTO` still runs
   under the write lock (it is the database snapshot); the lock-hold test measures the
   whole locked section. The pages are copied under the lock without fsync and synced
   after it is released (per-file fsync made the Windows CI exceed 1 s). Found in CI: the
   startup sweep opened the tantivy writer even with nothing to write, so its lock files
   were held from the first start; an empty, non-clearing `upsert_many` now does nothing. Restore clones the bundle, removes checked-out files that are not
   among the copied pages (committed after the copy; the database snapshot does not know
   them), overlays the copied pages, and commits the difference as `kioku: restore backup`
   so the restored wiki is clean. A copied page whose content is in no commit of the bundle
   (an uncommitted hand edit) is restored with a warning rather than failing the restore.
   Without git on the restoring machine the pages are restored without history. Format 1
   snapshots still restore; an older kioku refuses format 2 ("unsupported backup format").
7. **Self-maintenance (§5).** The startup sweep runs inside `Store::open` (so also for
   `restore` and tests), after the index is opened and before the session-page migration;
   rows whose file is gone are removed (with their index entries) besides healing differing
   pages. The deferred rebuild is `Store::reindex_if_outdated`, spawned by
   `kioku_server::serve_with` right after binding; it holds the write lock while it runs
   (finalize waits, search and recording do not). Found while testing the sweep: tantivy's
   `delete_all_documents` on a reopened index could drop documents re-added in the same
   commit after an earlier `delete_term`; a clearing rebuild now uses
   `delete_query(AllQuery)` (regression test `delete_then_rebuild_keeps_the_rebuilt_document`).
8. **Headless Mac (§5).** `service install` checks `launchctl print gui/<uid>` only after
   `launchctl bootstrap` failed (the normal path runs the same commands as before), and
   then reports the ja/en message instead of launchctl's. `doctor` checks it when the
   LaunchAgent is not installed or not active, and reports OK when
   `/Library/LaunchDaemons/dev.kioku.serve.plist` exists and the server answers. The daemon
   plist is the LaunchAgent plist plus `UserName` and `HOME` (launchd does not promise a
   `HOME` for `UserName` jobs); the plist goes to stdout and the explanation and the two
   `sudo` commands to stderr, so `kioku service install --daemon > file` works. `service
   start/stop` keep managing the LaunchAgent only; a daemon restarts itself after an
   automatic update through `KeepAlive` (exit 75).
9. **Digest quality (§6).** Besides the listed rules: `exitCode` (camel case) counts like
   `exit_code`; a structured `error` (non-empty object / array, or `true`) still counts —
   Cursor / Gemini failures are normalized to that shape — while a string `error` and a
   plain string response follow the prefix rule. The cleaning is applied to every prompt the
   digest keeps (so a cut-off `<system-reminder>` never reaches a page), `<command-args>`
   blocks are stripped too, and a prompt that was only a slash command becomes
   `/command args` instead of an empty title.
10. **Hook dump (§7).** The 24-hour window starts with the first dumped invocation (the
    marker is written then), for `KIOKU_HOOK_DUMP=1` and `[client] hook_dump = true`
    alike. A hook with dumping off reads nothing. Since an environment variable cannot be
    "re-set" observably, re-enabling is `kioku hook-dump enable` (new subcommand; rewrites
    the marker); doctor names it in its fix line.
11. **Rollback skip (addition from the M2.7 review).** After an automatic rollback the
    failing release is recorded as `skip_tag` (`v<version>`) in `state/auto-update.json`;
    the server's update task returns `Skipped` for that tag (logged once, no download) and a
    newer tag clears the mark and installs as usual. Manual `kioku update` ignores it.
12. **Not verified on real machines:** the headless-Mac message and the LaunchDaemon on a
    Mac without a GUI login (tested with a recording runner and a golden plist only), the
    daily prune timing in a long-running service, restore of a format-2 snapshot on
    Windows with a real (large) wiki history, and the lock-hold time with a large database
    (`VACUUM INTO` stays under the lock).
