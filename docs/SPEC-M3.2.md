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
symptom. `doctor`'s `server` check, when the connect error is `EHOSTUNREACH`/`ENETUNREACH`
to a private address (RFC 1918, link-local, `.local`), must:

1. probe the same address with `/usr/bin/nc -z` (system binary, exempt) — if that
   succeeds, say so and name the likely cause and fix (ja/en): 「この端末の kioku だけが
   LAN に出られません。macOS の「プライバシーとセキュリティ › ローカルネットワーク」で、
   kioku を起動したアプリ（例: Orca / Ghostty / Claude / Codex）を許可してください。Little Snitch
   を使っている場合は kioku 実行ファイル自体に許可ルールを作ってください」;
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
