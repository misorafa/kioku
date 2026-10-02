# kioku — SPEC-M2.6: reliable memory and recovery

Status: spec, 2026-10-01. Amends SPEC-M1 §§2, 5–7 and SPEC-M2 diagnostics. Read CLAUDE.md,
SPEC-M1.md, SPEC-M2.md and SPEC-M2.4.md first.

> **Amended by SPEC-M2.7 (v0.9):** the database carries `PRAGMA user_version` = 3 (M1 = 1,
> M2.4 = 2, M2.6 = 3) and a binary refuses a newer one (§5); one process per data directory
> (`kioku.lock`, §6); a page write whose index update fails sets
> `reliability_meta.needs_reindex`, healed at the next start (§6); `/api/v1/backup` keeps
> `[server] backup_keep` (10) snapshots and refuses a second backup within 60 s (§12).

Origin: a proposal implemented in commit ca8afe5 (not merged). It was reviewed in three
parts; this spec keeps what held up, fixes what did not, and drops the rest (§6). No new
dependency, service, model or database.

## 1. Session page identity and conditional page writes

- A session page is `<project>/sessions/YYYY-MM-DD-<first 8 of id>-<first 12 hex of
  sha256(id)>.md`. The readable prefix matches hook output; the hash keeps ids that share
  their first 8 characters apart (Codex ids are UUIDv7: same prefix within ~65 s).
- **Migration** of pages named the old way (`…-<first 8>.md`) runs once per data directory:
  `reliability_meta.session_pages_v2` records completion. Each session page whose
  frontmatter names a known session moves to its new path (new file and redirect first,
  then the old file is removed), `page_redirects(old_path → new_path)` keeps old paths
  readable, then one `git commit` of the wiki and one reindex. A page that cannot move
  is logged and stays where it is; `Store::open` never fails because of the migration.
  An interrupted run is completed by the next start (commit and reindex run on every run
  until the flag is set).
- Missing session pages are **not** regenerated: finalizing again would issue a fresh
  unaccepted rules handoff for a session that ended long ago.
- `read_page`: a real file at the path wins; otherwise a redirect is followed.
- Page reads return `revision` = SHA-256 of the file's bytes. `write_page` accepts
  `expected_revision`, compared under the write lock: mismatch → 409 / MCP tool error
  naming the path; the resolved file missing → 409 saying so. **Absent or empty = the
  legacy unconditional write** (some clients send "" for every optional string).
- Write path resolution: an explicit `_global/…` path without `scope` is a global page even
  when `project` is given; an explicit path that was moved (redirect) writes to its new
  location when that is inside the area this scope may write.
- `kioku project merge` keeps redirects for moved pages and re-points older redirects.

## 2. Backup and restore

> Amended by SPEC-M2.8 §4 (history as `wiki.bundle`, short write lock) and SPEC-M2.8 §3 (`[retention] backups_keep`).

SQLite is authoritative for sessions, observations, handoffs, aliases, receipts and
redirects; Markdown for page content. A complete backup is wiki + SQLite + raw.

`kioku backup` (`POST /api/v1/backup`, bearer auth) writes `<data_dir>/backups/<id>/`,
published by rename only when complete, with `manifest.json` (format 1: SHA-256 and length
of every file, table row counts). Lock scope:

1. write lock (page writes, finalize, merge wait): remove stale `.<id>.tmp` stages;
2. DB lock only for `VACUUM INTO db/kioku.sqlite`, the row counts, and the lengths of the
   raw logs (raw lines are appended under the DB lock, so those prefixes match the copy);
3. copy the wiki (incl. `.git`) under the write lock only;
4. after both locks: copy the recorded raw prefixes, hash everything, write the manifest.

Excluded: config, tokens, logs, other backups, the search index. Symlinks and special
files are refused. A concurrent `git gc` can make a backup fail; it never yields a
partial one. Scheduling, off-machine copies and pruning are the operator's job. The CLI
prints a summary (files, size, counts, path), not the manifest.

`kioku restore <backup> --into <new dir> [--verify]` runs locally: refuses an existing
destination, symlinked roots and destinations inside the backup; checks the manifest
(paths limited to `wiki/`, `raw/`, `db/kioku.sqlite`), copies into a stage, re-hashes the
stage, checks SQLite `integrity_check` / `foreign_key_check` and row counts, opens it
(rebuilding the index) and requires a consistent wiki/metadata/index, then renames the
stage into place. The `pages` table is rebuilt from the restored wiki, so it is compared
with the wiki, not with the live table's count; pages that cannot be parsed are logged,
not fatal. `--verify` is accepted (verification always runs). Then
`KIOKU_DATA_DIR=<dir> kioku init` provisions fresh credentials.

## 3. Delivery receipts and the offline queue

- `NewObservation.event_id` (optional, 1–200 bytes). Same session + event id → the
  original seq; same id with different content → 409; no id → legacy append. The receipt
  hash is over (kind, ts, sanitized payload) with JSON keys sorted. Receipts are stored in
  the same transaction as the observation. `reliability_meta.last_received` is updated.
- The server advertises `observation_dedup: true` in `GET /health` and in the
  `POST /sessions/start` response. The client records it per server URL at SessionStart
  (`outbox/<sha256(url)>/server-dedups`); no per-hook probe.
- **Happy path unchanged:** one POST per observation; with a dedup server it carries a
  fresh `event_id`. Nothing is written locally.
- A delivery that failed with no HTTP answer (connect error, timeout) or a 5xx, to a dedup
  server, is queued: `outbox/<sha256(url)>/<id>.json` (0600; observation exactly as sent,
  plus the SessionStartRequest needed to re-create the session — project and lane are
  computed only now, after the failure). Bounded to 50 MiB / 10,000 entries; a full or
  busy queue is reported in the hook's error, never evicts. A server without dedup never
  gets anything queued.
- Replay (`kioku sync`, and detached after a hook when something is queued — not within
  60 s of the previous spawn or a failed replay): `GET /health` must advertise dedup; up to
  64 entries per batch under one 30 s deadline; an unknown session is re-created with
  `source = offline-replay`, which never consumes a handoff. Entries refused for good
  (400/404/409/413/422) and unreadable files move to `failed/`; other errors stop the
  batch and are recorded (`last-error.txt`, sanitized). Finalize after a batch only for
  sessions the replay created or whose newest queued observation is ≥ 30 min old; a live
  session is finalized by its own hooks.
- The cached-address attempt (SPEC-M2 §19) uses a connect timeout of half its budget so a
  dead address fails as a connect error and falls back to DNS for POSTs too; a POST whose
  response was lost is not re-sent by that fallback.

## 4. Diagnostics

`GET /api/v1/diagnostics` (bearer auth): last observation received, last backup, last git
commit failure (`git --no-optional-locks -c core.fsmonitor=false status --porcelain`,
truly read-only), page paths whose file, metadata and index disagree, pages that cannot be
parsed, and whether index and metadata counts match. Only the metadata snapshot is read
under the DB lock; files are hashed without locks (a racing write can appear as a
momentary mismatch). `kioku doctor` gives it 20 s and shows: `recording` and a missing
backup as information (idle machines and external backups are normal), `backup` warns
when the last one is older than 7 days, `wiki.git`, `storage.consistency` and
`storage.unparseable` warn, `outbox` warns on queued, refused or failed entries.

## 5. Evaluation

A fixed Japanese corpus (`kioku-core/src/index/search-eval.json`) with near-duplicate
distractors, NFKC / half-width queries and top-1 requirements; Recall@3 ≥ 0.90 and
MRR@3 ≥ 0.75 in every CI run. It is a regression guard, not a quality claim.

## 6. Differences from ca8afe5

> Amended by SPEC-M3.1 §1: a compact / resume of a known session again gets back the handoff it accepted (decided by `source`, not by a retry).

- Page names: date + id prefix + 12-hex hash instead of the full 64-hex hash.
- Migration: once, non-fatal, one commit, no regeneration of missing pages (which re-issued
  old handoffs and consumed the real one); real files win over redirects.
- Dropped: "a retried SessionStart returns the handoff it already accepted" (it fired on
  compact / resume) and "a session id cannot change project or agent → 409" (it broke
  starts after a remote was added on another machine).
- `expected_revision: ""` is unconditional, not create-only; writes follow redirects and
  `_global/` paths; the 409 names the path and says when it does not exist.
- Offline queue: queue only after a failed delivery (was: every observation, with a health
  probe, git calls and an fsync per tool call, and a failed enqueue dropped a deliverable
  observation); no double sanitising; refused entries set aside instead of blocking;
  backoff for spawns; no finalize of live sessions; shared detached-spawn helper.
- Backup / diagnostics: narrow lock scopes (were: whole-tree hashing under both locks);
  restore tolerant of unparseable pages and page drift; stale stages removed; summary
  output.
- Doctor: storage drift is a warning, not a failure; idle / no-backup are informational;
  read-only git status as the original spec intended.
