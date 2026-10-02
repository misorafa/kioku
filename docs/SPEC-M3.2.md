# kioku — SPEC-M3.2: observability and a coherent set of documents

Status: spec, 2026-10-02. Amends SPEC-M2 (status, doctor), SPEC-M2.5 (update status).
Read CLAUDE.md, SPEC-M2.md §14–§16, SPEC-M2.7.md, SPEC-M2.8.md §5 first. Source: the
v0.8.0 audit (PROD-8/9, CORE-低-9, CLI-6). Depends on M3.1.

Goal: an unattended server can be watched without ssh-ing into it, and a newcomer (or the
author in six months) can find the current rule for anything in one place.

## 1. Metrics and request log

- `GET /api/v1/metrics` (bearer auth): Prometheus text format, no new crate (hand-written
  lines). Gauges: `kioku_sessions_open`, `kioku_sessions_total`,
  `kioku_observations_total`, `kioku_handoffs_pending`, `kioku_index_docs`,
  `kioku_outbox_queued` (0 on the server; clients have no endpoint), `kioku_db_bytes`,
  `kioku_raw_bytes`, `kioku_wiki_bytes`, `kioku_backups_bytes`,
  `kioku_last_backup_age_seconds`, `kioku_last_prune_age_seconds`,
  `kioku_last_observation_age_seconds`, `kioku_update_last_check_age_seconds`,
  `kioku_version_info{version="…"} 1`. Counters since start: `kioku_http_requests_total
  {route,status}`, `kioku_http_request_seconds_sum/count{route}`,
  `kioku_mcp_tool_calls_total{tool,ok}`, `kioku_git_commit_failures_total`.
- Request log: one `tracing::info!` line per request at `target = "kioku_http"`
  (`method route status ms`), bearer header never logged; off unless
  `[server] request_log = true` (default false) or `RUST_LOG=kioku_http=info`.
- `kioku status --watch 5` redraws every 5 s (sessions open, last observation age,
  outbox, update, sizes).
- Tests: metrics endpoint shape (every gauge present, counters increment after a
  request), 401 without a token, request log line format (captured subscriber), no token
  in any log line (regression over hook.log / update.log / serve.log in the e2e fixture).

## 2. Positive liveness: "it is working"

- Clients write `state/last-hook.json` `{agent → {event, at, ok}}` on every hook
  (one small write per hook — allowed; it replaces nothing). `kioku doctor` prints a
  `hooks.liveness` line per installed agent: last successful hook time, warn when an
  agent is installed but has no success within 7 days **and** the agent's binary/app is
  present on this machine.
- `kioku status --agents` prints the same table.
- Tests: the file after a hook; doctor's warn/ok matrix.

## 3. `kioku doctor --fix`

Applies the safe fixes doctor already proposes: `chmod 0600/0700` of config/data,
re-registering missing hooks or MCP entries (`kioku install <agent>`), `kioku service
install` when the marker is missing, `kioku reindex` when the index is outdated or
inconsistent, clearing an expired hook-dump. Never touches a running Claude desktop app
config (prints the quit-first instruction), never rotates tokens, never changes
`[server] bind`. Prints what it did; exit 0 when everything proposed was applied. Tests:
each fixable check on a fixture; `--fix` is idempotent.

### 3.1 Client reachability diagnosis (field, 2026-10-02)

Seen on a macOS client: every `kioku` request to the LAN server failed with
`tcp connect error: No route to host (os error 65)` while `curl`, `nc` and `ping` from the
same shell reached it. Cause class: **per-process network permission** — macOS "Local
Network" privacy (TCC) denies non-system processes; the permission belongs to the
*responsible app* (the terminal / IDE / agent app that launched the hook), so hooks fail
under one app and work under another. Little Snitch rules and silent mode produce the same
symptom. Confirmed on 2026-10-02: in the same second, `/usr/bin/python3` (Apple-signed) connected
and `/opt/homebrew/bin/python3` (ad-hoc signed) got `errno 65`; the responsible app (Orca)
showed as *allowed* in System Settings, but had been updated the evening before — a TCC
grant bound to the previous code signature keeps its toggle on while the new binary's
children are denied. The fix is to toggle the app off and on again (or remove the entry)
and relaunch it, so macOS re-prompts.
`doctor`'s `server` check, when the connect error is `EHOSTUNREACH`/`ENETUNREACH`
to a private address (RFC 1918, link-local, `.local`), must:

1. probe the same address with `/usr/bin/nc -z` (system binary, exempt) — if that
   succeeds, say so and name the likely cause and fix (ja/en): 「この端末の kioku だけが
   LAN に出られません。macOS の「プライバシーとセキュリティ › ローカルネットワーク」で、
   kioku を起動したアプリ（例: Orca / Ghostty / Claude / Codex）を許可してください。既に許可済み
   に見える場合は、そのアプリを一度オフにしてからオンにし、アプリを再起動してください（アプリの
   更新後に起きます）。Little Snitch を使っている場合は kioku 実行ファイル自体に許可ルールを
   作ってください」;
2. print the responsible app (walk `ppid` to the first `.app` bundle) so the user knows
   which toggle to flip;
3. never suggest "check the server" in this case.

`doctor --fix` cannot grant TCC; it prints the exact Settings path. Hooks keep failing open;
with M2.6's queue (server ≥ 0.8.0) the observations are not lost once the permission is
granted. Test: a unit test on the classifier (error kind + address class + nc result →
message), and the ppid walk on a fixture.

## 4. Documents (SPEC-INDEX)

- `docs/INDEX.md`: one table — topic → the section that is currently authoritative
  (e.g. "session page name → SPEC-M2.6 §1", "handoff routing → SPEC-M3.1 §1", "update
  interval → SPEC-M2.5 §3.1 as amended v0.8.2"). Every later SPEC that amends an
  earlier one gets a one-line "superseded by" note at the top of the superseded section.
- README.md / README.ja.md: `Status` line and `Roadmap` rewritten (M2.1–M3.1 done; M3.2,
  M3.3, M4 as planned in the audit page); Data layout lists `outbox/`, `backups/`,
  `state/`, `dict/`, `captures/`, `logs/{serve,update,hook}.log`; Claude desktop app
  config rewrite documented under Agents; MCP tool table complete (`expected_revision`,
  `revision`, `session`, `history`, `since`, `kinds`, `path_prefix`, `gotchas`,
  `verified`); "makes no LLM calls"; Gemini CLI marked legacy.
- SPEC-M1: §5 schema listing updated to the current tables/columns (with the SPEC that
  added each), §7.1/§8.3/§8.5/§11 replaced by pointers to INDEX.md, §9 error table
  includes 409 and 78/75 exit codes.
- CLAUDE.md: `docs/adr/` either created with ADR-0001 (Markdown-in-git + SQLite +
  tantivy; no LLM in the server) and ADR-0002 (clients follow the server's version) or the
  line removed — create them. AGENTS.md becomes a one-line pointer to CLAUDE.md (no second
  copy). Directory layout line for kioku-cli lists the current subcommands.
- Tests: a CI step that fails when `docs/INDEX.md` references a section that does not
  exist (simple grep-based script in `scripts/check-docs.sh`).

## 5. Deliverables

Branch `m3.2-observability-docs`, draft PR against `main`, CI green. Record deviations in
§6. Do not merge, tag or change secrets.

## 6. Implementation notes (2026-10-02, branch `m3.2-observability-docs`)

### Metrics and request log (§1)

- No new crate: `axum::middleware::from_fn_with_state` (tower stays transitive) wraps the
  whole router, outside the bearer check, so refused requests are counted too. The route
  label is axum's `MatchedPath` (`/api/v1/pages/{*path}`), `/mcp` for everything under the
  MCP endpoint, `unmatched` for 404s of no route — a page path or id never becomes a label.
- Age gauges are `-1` when the event never happened (no backup, no prune, no observation,
  no release check yet). `kioku_http_request_seconds` is declared `summary` with `_sum` /
  `_count`. Counters live in memory since process start. `kioku_git_commit_failures_total`
  counts failed `git add` and `git commit` runs (`Git::commit_failures`, shared by clones).
- Request log line: `GET /api/v1/status 200 3ms` at `target = "kioku_http"`; `kioku serve`
  without `RUST_LOG` uses `info,tantivy=warn,kioku_http=off` unless `[server] request_log =
  true` (`commands::default_log_filter`). Headers and query strings are never logged.
- The request-log test lives in its own test binary (`kioku-server/tests/request_log.rs`):
  tracing caches callsite interest process-wide, and a parallel test without a subscriber
  made the scoped subscriber miss events. The token regression (`kioku-cli/tests/
  log_hygiene.rs`) installs the global subscriber `kioku serve` uses and checks serve.log,
  hook.log and update.log (`auto_update::log_update` became public for it).
- `kioku status --watch N` reads `/status` and, when present, `/metrics`; against an older
  server the metric lines say `? (server without /api/v1/metrics)`.

### Liveness (§2)

- Check ids are `hooks.liveness.<agent>` (one per installed agent), not one shared
  `hooks.liveness` id: `--json` consumers and the tests key checks by id.
- `last-hook.json` entries carry `last_ok` besides `{event, at, ok}`, so a failing hook does
  not erase the time of the last success. Written as a temp file renamed over the old one;
  errors are ignored. "Installed" = kioku hooks registered in the agent's user config;
  "present" = the agent's binary on `PATH` / `~/.local/bin` / `~/.claude/local`
  (`claude`, `codex`, `cursor`, `cursor-agent`, `gemini`, `agy`) or its app in
  `/Applications`, `~/Applications` (macOS). A hook that failed after a recent success is
  OK with a note pointing at hook.log.

### doctor --fix (§3)

- Each check carries an optional `FixAction` (`Chmod`, `Install(agent)`, `ServiceInstall`,
  `Reindex`, `ClearHookDump`, `Manual(text)`); `--fix` applies the distinct actions, prints
  `fixed:` / `failed:` / `manual:` lines, re-runs doctor and prints the result. Exit 0 when
  no action failed; `manual:` instructions (TCC, a running Claude app, `KIOKU_HOOK_DUMP` in
  the environment) do not count as failures.
- A detected agent without any kioku hooks is re-installed too (doctor already proposed
  `kioku install <agent>` for it). The index schema-mismatch warning also maps to reindex.
- Claude app running: `pgrep -x Claude` (macOS) / `tasklist` (Windows); an unanswerable
  query counts as running. Then `kioku install claude-code` runs with the new
  `InstallOptions::skip_desktop`, and the quit-first instruction is printed.
- Expired hook dump: `hook_dump = true` in `[client]` is rewritten line by line to
  `hook_dump = false` (comments and the token line untouched, permissions kept) and the
  window marker removed.

### Reachability (§3.1)

- The error kind is read from the error text (reqwest hides the `io::Error`): errno 65/51
  (macOS), 113/101 (Linux), 10065/10051 (Windows) or the strerror text. Private = RFC 1918,
  link-local, IPv6 ULA / link-local, `.local`, or a name resolving to one of those.
- Probe: `/usr/bin/nc -z -G 3 -w 3 host port` (macOS; `-G` dropped elsewhere). Without
  `/usr/bin/nc` the message says what to compare instead.
- Responsible app: `ps -o ppid=,etime=,comm= -p <pid>` up the chain (≤ 32 steps); the
  first process whose path contains a `.app` component; the bundle is the outermost
  `.app`. **Stale app** (the 2026-10-02 cause): that process started (now − etime) more
  than 2 s before the bundle's `Contents/Info.plist` mtime → "<App> was updated while its
  old processes kept running: quit it completely and start it again" (ja/en).
- The `server` check stays FAIL in this case (kioku really cannot reach the server), with
  the Local Network fix and a `Manual` action so `--fix` prints the Settings path.
- Extra: a server version that is unknown or empty prints `reachable; version unknown
  (older client)` and is never compared (an older client printed "runs v, this client is
  v0.7.0" against ≥ 0.8.1).

### Documents (§4)

- `docs/INDEX.md` (81 rows), one-line amendment notes in the older specs, SPEC-M1 §5 /
  §7.1 / §8.3 / §8.5 / §9 / §11 as asked, ADR-0001/0002, AGENTS.md → CLAUDE.md,
  `scripts/check-docs.sh` (CI step "docs index", Linux), plus SECURITY.md, CONTRIBUTING.md,
  CHANGELOG.md (v0.3.0 … v0.9.1, Unreleased = M3.2) and issue templates.
- Contradictions found while indexing (now noted at the sections): SPEC-M1 §6.2 vs
  SPEC-M2.8 §5 (outdated index is rebuilt automatically); SPEC-M1 §9 lacked 429 (invite
  lookups); SPEC-M2.5 §3.2 still listed `version` in `/health` (removed by SPEC-M2.7 §11);
  SPEC-M2.5 §1 "once a day" vs §3.1 hourly (v0.9.1); SPEC-M2.6 §6 dropped the
  "retried SessionStart returns the accepted handoff" rule that SPEC-M3.1 §1 restores for
  compact/resume; SPEC-M2.7 §5 starts `user_version` at 3 but the code is at 5 (M2.8 = 4,
  M3.0 = 5) — SPEC-M1 §5 now records the numbering.
