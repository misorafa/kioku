# Changelog

Every release of kioku, newest first. Built from the release commits and the merged pull
requests of each tag (`git log <previous tag>..<tag>`). Specifications are in
[docs/](docs/INDEX.md); the section that is current for each topic is listed in
[docs/INDEX.md](docs/INDEX.md).

## v0.9.3 — 2026-10-02

- Homebrew: `brew install misorafa/tap/kioku`. Each stable release renders
  `Formula/kioku.rb` (`packaging/homebrew/kioku.rb.tmpl`) and pushes it to the tap
  (skipped with a notice while `TAP_TOKEN` is not set). A Homebrew binary is never
  replaced: `kioku update` and the automatic updates print `brew upgrade kioku`; hooks and
  the service use the stable `<prefix>/bin/kioku` link (SPEC-M3.3 §1).
- Docker: every release publishes `ghcr.io/misorafa/kioku:<tag>` and `:latest`
  (linux/amd64 + arm64) built from the release's musl binaries; non-root, `VOLUME /data`,
  health check on `/api/v1/health`. A container never updates itself (update with
  `docker compose pull`, or watchtower); `kioku init` in a container prints the token it
  generates once (SPEC-M3.3 §2). **Changed:** the image's entrypoint is `kioku` with the
  default command `serve`, so serve options now follow `serve`
  (`docker run … ghcr.io/misorafa/kioku serve --port 8000`); the Dockerfile packages a
  release binary instead of building from source.
- `kioku uninstall` (no agent) removes kioku from the machine: every agent's hooks and MCP
  entries, the service, the PATH line install.sh / install.ps1 added, the binary and
  `.prev`; `--everything` also `config.toml`, `--purge-data` also the data directory
  (typed confirmation). `kioku uninstall <agent>` is unchanged (SPEC-M3.3 §3).
  install.ps1 now marks the user PATH entry it adds (`kioku-path-entry.txt`).
- Fixture freshness: `scripts/probe-agents.sh --check` compares captured fixtures' key
  sets with a fresh capture; fixtures record `_meta.captured_with`; `kioku doctor` prints
  an `[INFO]` line when an installed agent is newer than its fixtures. Gemini CLI is
  legacy (fixtures under `tests/fixtures/legacy/`) (SPEC-M3.3 §4).

## v0.9.2 — 2026-10-02

- `GET /api/v1/metrics` (bearer auth): Prometheus text with counts, sizes, ages of the last
  backup / prune / observation / update check, request, MCP tool-call and git-failure
  counters; optional per-request log (`[server] request_log = true`) (SPEC-M3.2 §1).
- `kioku status --watch N` redraws the server's state; `kioku status --agents` and a
  `hooks.liveness` doctor line per agent show the last successful hook
  (`state/last-hook.json`) (SPEC-M3.2 §1–§2).
- `kioku doctor --fix` applies the safe fixes doctor proposes (permissions, re-registering
  hooks / MCP, service install, reindex, an expired hook dump) (SPEC-M3.2 §3).
- Doctor diagnoses a client that alone cannot reach a LAN server — macOS Local Network
  permission / Little Snitch, including an app updated while its old processes kept
  running — instead of blaming the server (SPEC-M3.2 §3.1).
- Doctor no longer compares an unknown server version ("version unknown (older client)").
- Documents: `docs/INDEX.md`, "superseded by" notes in older specs, ADRs, SECURITY.md,
  CONTRIBUTING.md, this changelog, issue templates (SPEC-M3.2 §4).

## v0.9.1 — 2026-10-02 — M3.1: handoff consumption and better search

- Handoff consumption rules: compact / resume get back what they accepted, a session
  never receives its own handoff, a busy lane leaves the handoff pending; `superseded`
  status and `kioku_handoff_pending(history)` (#11, SPEC-M3.1 §1).
- Search re-ranked by recency, kind and `pinned`; `since` / `kinds` filters; code
  identifiers; user dictionary `dict/user.csv`; partial-match fallback; `path_prefix`
  lists the sessions that edited a file (SPEC-M3.1 §2–§3).
- Claude Code's `/clear` is a new session, not a resume.
- Hourly release checks by default, the first 3 minutes after start.

## v0.9.0 — 2026-10-02 — M3.0: what an agent sees at session start

- New `<kioku>` block: carried decisions / open questions, pinned pages, recent sessions
  and the previous session's last reply (#10, SPEC-M3.0 §1).
- `gotchas` and `verified` in handoffs; the agent's last reply recorded on Stop.
- Time-based Stop nudge (`nudge`, `nudge_min_minutes`); machine name (`@machine`).

## v0.8.2 — 2026-10-01 — M2.8: per-turn cost, retention, hygiene

- Incremental finalize with a cached digest; one git commit per turn, none when nothing
  changed (#9, SPEC-M2.8 §1–§2).
- `[retention]`, `kioku prune`, `kioku forget`; backups with the history as a git bundle
  and a short write lock (SPEC-M2.8 §3–§4).
- Self-maintaining server (startup sweep, deferred reindex), headless Mac LaunchDaemon,
  hook-dump expiry, rollback `skip_tag`.
- The server checks for releases every 4 hours (`[update] interval_hours`).

## v0.8.1 — 2026-10-01 — M2.7: hardening after the v0.8.0 audit

- Hooks always fail open; updates verify checksum and signature before running anything;
  memory shown as untrusted data; invite line fetches the script over https (#8,
  SPEC-M2.7).
- Database schema version with downgrade refusal (exit 78); one process per data
  directory (`kioku.lock`); server rollback after three failed starts.
- SessionStart git budget and identity cache; tokens off argv and stdout; more secret
  patterns, also in pages and handoffs; `/health` without version.

## v0.8.0 — 2026-10-01 — M2.6: reliable memory and recovery

- `kioku backup` / `kioku restore`; offline queue with delivery receipts (`kioku sync`);
  conditional page writes (`expected_revision`, 409); collision-free session page names;
  storage diagnostics (#7, SPEC-M2.6).

## v0.7.1 — 2026-09-30

- Doctor recognises Codex 0.159's hook trust keys and warns when the service definition
  predates automatic updates.

## v0.7.0 — 2026-09-30 — M2.5: automatic updates

- The server follows releases, clients follow the server's version (#6, SPEC-M2.5).
- Releases are submitted to winget; tell users to quit the Claude app before registering
  kioku in it.

## v0.6.5 — 2026-09-30

- Register kioku in the Claude desktop app's chat / Cowork config (SPEC-M2.2 §7.3a).
- A finalize the server accepted but did not answer in time is not a hook failure.

## v0.6.4 — 2026-09-29 — M2.4: handoff lanes and project aliases

- Handoffs per git branch (parallel worktrees) and project aliases for a remote added later;
  `kioku project merge` (#5, SPEC-M2.4).
- `kioku mcp` re-reads `config.toml` before every call; `kioku update` defers to winget for
  winget installs.

## v0.6.3 — 2026-09-29

- Windows `.zip` release asset for winget.

## v0.6.2 — 2026-09-29

- Windows updates no longer blocked by an in-use `kioku.exe.old` (#4).

## v0.6.1 — 2026-09-29

- Windows invite line that Defender does not flag (#3, SPEC-M2.3 §9).

## v0.6.0 — 2026-09-28 — M2.3: one-command join

- `kioku invite` / `kioku join` with one-time invite codes; installers put kioku on PATH by
  default (#2, SPEC-M2.3).

## v0.5.1 — 2026-09-28

- Doctor finds `PATH` in any casing (Windows); real Windows 11 hook payloads as fixtures.

## v0.5.0 — 2026-09-28 — M2.2: Windows native client

- `kioku.exe`, `install.ps1`, Claude Code and Codex hooks on Windows, rename-dance update,
  Windows in CI and releases (#1, SPEC-M2.2).

## v0.4.2 — 2026-09-28

- `kioku rotate-token` (SPEC-M2 §21).

## v0.4.1 — 2026-09-28

- Developer ID signed and notarized macOS builds.

## v0.4.0 — 2026-09-28

- `kioku mcp` stdio bridge, registered as every agent's MCP server (SPEC-M2 §20).

## v0.3.3 — 2026-09-28

- Connect timeout capped at 4 s; IPv4 kept behind a link-local peer in the address cache.

## v0.3.2 — 2026-09-28

- Connection robustness: dual-stack server, split connect timeout, last-good addresses
  (SPEC-M2 §19); a former server switching to `--client-only` retires its server.

## v0.3.1 — 2026-09-28

- Stable macOS code identity and a doctor LAN check (`server.lan`, SPEC-M2 §10.3.1).

## v0.3.0 — 2026-09-27 — M2.1: Antigravity CLI

- Antigravity CLI (`agy`) support and captured Cursor / agy payloads (SPEC-M2.1). Before
  it, untagged: M1 (server, Markdown/git store, Japanese search, MCP tools, Claude Code
  hooks and handoffs) and M2 (Codex CLI, Cursor, Gemini CLI, `install.sh`, `kioku setup` /
  `doctor` / `service` / `update`).
