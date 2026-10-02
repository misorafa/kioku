# kioku — specification index

Where the **current** rule for each topic lives. The specs are a stack: SPEC-M1 is the
base, every later SPEC amends earlier ones, and a section that was replaced carries a
one-line "Superseded by" / "Amended by" note at its top. When two sections disagree, the
one listed here wins; when this table and a SPEC disagree, fix this table (CI runs
`scripts/check-docs.sh`, which fails when a section referenced here does not exist).

Files: [SPEC-M1](SPEC-M1.md) · [SPEC-M2](SPEC-M2.md) · [SPEC-M2.1](SPEC-M2.1.md) ·
[SPEC-M2.2](SPEC-M2.2.md) · [SPEC-M2.3](SPEC-M2.3.md) · [SPEC-M2.4](SPEC-M2.4.md) ·
[SPEC-M2.5](SPEC-M2.5.md) · [SPEC-M2.6](SPEC-M2.6.md) · [SPEC-M2.7](SPEC-M2.7.md) ·
[SPEC-M2.8](SPEC-M2.8.md) · [SPEC-M3.0](SPEC-M3.0.md) · [SPEC-M3.1](SPEC-M3.1.md) ·
[SPEC-M3.2](SPEC-M3.2.md) · [SPEC-M3.3](SPEC-M3.3.md) · decisions: [adr/](adr/)

| topic | authoritative section(s) | notes |
|-------|--------------------------|-------|
| Goal, non-goals (no LLM in the server) | SPEC-M1 §1, SPEC-M2 §1 | [ADR-0001](adr/ADR-0001-markdown-git-sqlite-tantivy.md) |
| Data directory layout | SPEC-M1 §2, SPEC-M2.6 §2, SPEC-M2.8 §3 | `outbox/`, `backups/`, `state/`, `dict/`, `captures/`, `logs/` — README "Data layout" lists them all |
| `config.toml` `[server]` / `[client]` | SPEC-M1 §3, SPEC-M3.0 §4 | `nudge`, `nudge_min_minutes`; `[server] request_log` → SPEC-M3.2 §1 |
| `config.toml` `[update]` | SPEC-M2.5 §2, SPEC-M2.7 §11 | `allow_mirror` |
| `config.toml` `[retention]` | SPEC-M2.8 §3 | supersedes `[server] backup_keep` (SPEC-M2.7 §12) |
| Project identity | SPEC-M1 §4, SPEC-M2.7 §8 | identity cache `state/projects.json` |
| Project aliases, `kioku project merge` | SPEC-M2.4 §2 | |
| Handoff lanes per branch | SPEC-M2.4 §1 | default branch: SPEC-M2.4 §1.2 |
| Handoff routing and consumption | SPEC-M3.1 §1 | supersedes SPEC-M2.4 §1.4 and the acceptance rule of SPEC-M1 §7.1 |
| Handoff contents (`gotchas`, `verified`) | SPEC-M1 §7.5, SPEC-M3.0 §2 | |
| SQLite schema | SPEC-M1 §5 | updated listing with the SPEC that added each column |
| Schema version, downgrade refusal | SPEC-M2.7 §5 | `PRAGMA user_version`; `kioku serve` exits 78 |
| One process per data directory | SPEC-M2.7 §6 | `kioku.lock`, `needs_reindex` self-heal |
| Page format | SPEC-M1 §6.1 | |
| Session page name | SPEC-M2.6 §1 | `YYYY-MM-DD-<8>-<12 hex>.md`, redirects |
| Page revisions, `expected_revision` (409) | SPEC-M2.6 §1 | |
| tantivy schema, analyzers | SPEC-M1 §6.2, SPEC-M3.1 §2 | index schema v3 (`code`, `ja_bigram`) |
| Search query, re-rank, `since` / `kinds` | SPEC-M3.1 §2 | amends SPEC-M1 §6.3 |
| User dictionary `dict/user.csv` | SPEC-M3.1 §2 | |
| Required Japanese search tests, evaluation | SPEC-M1 §6.4, SPEC-M3.1 §2 | thresholds in SPEC-M3.1 §5 |
| Sessions of a file (`path_prefix`) | SPEC-M3.1 §3 | |
| Search result line format | SPEC-M3.0 §5, SPEC-M3.1 §2 | |
| Session lifecycle | SPEC-M1 §7.1, SPEC-M3.0 §4 | finalize on every Stop |
| Finalize (incremental, cached digest) | SPEC-M2.8 §1 | |
| One commit per turn | SPEC-M2.8 §2 | |
| Digest rules | SPEC-M1 §7.2, SPEC-M2.8 §6 | last reply: SPEC-M3.0 §3 |
| Session page / STATE.md contents | SPEC-M1 §7.3, SPEC-M1 §7.4, SPEC-M3.0 §1 | STATE.md carries block sections 3–6 |
| The `<kioku>` block at SessionStart | SPEC-M3.0 §1 | supersedes SPEC-M1 §8.3 |
| Stop nudge | SPEC-M3.0 §4 | supersedes SPEC-M1 §8.4 conditions |
| Agent's last reply | SPEC-M3.0 §3 | |
| Machine identity (`@machine`) | SPEC-M3.0 §6 | |
| Neutral hook event model | SPEC-M2 §3 | |
| Hooks are fail-open | SPEC-M2.7 §1 | |
| Implicit session start | SPEC-M2 §3.9 | |
| Per-agent hook deadlines | SPEC-M2 §3.10, SPEC-M2.7 §8 | git budget 40 % of the deadline |
| Hook payload capture (`KIOKU_HOOK_DUMP`) | SPEC-M2 §3.8, SPEC-M2.8 §7 | 24 h window |
| Sanitization / redaction | SPEC-M1 §8.2, SPEC-M2.7 §9 | pages and handoffs redacted too |
| Memory is untrusted data | SPEC-M2.7 §3 | |
| Offline queue, delivery receipts | SPEC-M2.6 §3 | `kioku sync` |
| Backup and restore | SPEC-M2.6 §2, SPEC-M2.8 §4 | bundle format 2; keep count SPEC-M2.8 §3 |
| Retention, `kioku prune`, `kioku forget` | SPEC-M2.8 §3 | |
| Self-maintaining server (startup sweep, deferred reindex) | SPEC-M2.8 §5 | |
| Storage diagnostics (`/diagnostics`) | SPEC-M2.6 §4 | |
| Claude Code hooks and MCP | SPEC-M2 §8, SPEC-M2.2 §7.1 | M1 §8.5 replaced by the installer rules |
| Claude desktop app (chat, Cowork) config | SPEC-M2.2 §7.3a | quit the app before installing |
| Codex CLI | SPEC-M2 §4, SPEC-M2.2 §7.2 | |
| Cursor | SPEC-M2 §5, SPEC-M2.2 §7.3 | late context SPEC-M2 §5.6 |
| Gemini CLI (legacy) | SPEC-M2 §6, SPEC-M3.3 §4 | retired for personal accounts; successor Antigravity; fixtures in `tests/fixtures/legacy/` |
| Antigravity CLI | SPEC-M2.1 §3, SPEC-M2.1 §4 | |
| Installers (common rules, `install all`) | SPEC-M2 §8 | |
| Instruction snippet | SPEC-M2 §7 | |
| `kioku mcp` stdio bridge | SPEC-M2 §20 | default MCP registration |
| HTTP API | SPEC-M1 §9, SPEC-M2 §9 | additions in SPEC-M2.3 §3.2, SPEC-M2.6 §3, SPEC-M2.8 §3, SPEC-M3.1 §2, SPEC-M3.2 §1 |
| Error responses and exit codes | SPEC-M1 §9 | 409; exit 78 (newer schema), 75 (restart after update), 2 (Stop nudge) |
| MCP tools | SPEC-M1 §10 | parameters added by SPEC-M2.6 §1, SPEC-M3.0 §2, SPEC-M3.1 §1, SPEC-M3.1 §2, SPEC-M3.1 §3 |
| CLI commands | SPEC-M1 §11, SPEC-M2 §14 | full list: CLAUDE.md "Directory layout" and `kioku --help` |
| Metrics endpoint, request log, `status --watch` | SPEC-M3.2 §1 | |
| Positive liveness (`state/last-hook.json`, `status --agents`) | SPEC-M3.2 §2 | |
| `kioku doctor` | SPEC-M2 §12, SPEC-M2.6 §4, SPEC-M2.5 §3.4 | `--fix`: SPEC-M3.2 §3 |
| `kioku doctor --fix` | SPEC-M3.2 §3 | |
| Client reachability (Local Network permission) | SPEC-M3.2 §3.1 | |
| `kioku setup` | SPEC-M2 §11 | |
| Service management (launchd / systemd) | SPEC-M2 §10, SPEC-M2.8 §5 | headless Mac daemon |
| LAN reachability of a macOS server | SPEC-M2 §10.3.1 | |
| Connection robustness (dual stack, last-good address) | SPEC-M2 §19 | |
| Tokens (never on argv / stdout) | SPEC-M2.7 §10 | amends SPEC-M2 §15 |
| `kioku rotate-token` | SPEC-M2 §21, SPEC-M2.7 §10 | |
| Invite / join | SPEC-M2.3 §3, SPEC-M2.7 §4 | Windows line SPEC-M2.3 §9 |
| Security model | SPEC-M2 §15, SPEC-M2.7 §3 | see also SECURITY.md |
| Automatic updates (principle) | SPEC-M2.5 §1 | [ADR-0002](adr/ADR-0002-clients-follow-server-version.md) |
| Server update interval | SPEC-M2.5 §3.1 | as amended v0.8.2 / v0.9.1: default 1 h, first check 3 min after start |
| Client follows the server | SPEC-M2.5 §3.3 | |
| Update verification order | SPEC-M2.7 §2, SPEC-M2.5 §4 | |
| Update source hardening (mirrors) | SPEC-M2.7 §11 | |
| Server rollback, `kioku update --rollback` | SPEC-M2.7 §7 | |
| `install.sh`, release assets | SPEC-M2 §13, SPEC-M2.3 §4 | |
| `kioku update` (manual) | SPEC-M2 §13.3, SPEC-M2.5 §4 | |
| Windows client, `install.ps1` | SPEC-M2.2 §4, SPEC-M2.2 §6 | update on Windows SPEC-M2.2 §5 |
| Orca, WSL, mixed setups | SPEC-M2.2 §9 | |
| Homebrew tap, `brew upgrade` notice | SPEC-M3.3 §1 | notes SPEC-M3.3 §6; template `packaging/homebrew/` |
| Docker image (ghcr.io), container detection | SPEC-M3.3 §2 | notes SPEC-M3.3 §6; no self-update in a container |
| `kioku uninstall` (whole machine) | SPEC-M3.3 §3 | per agent: SPEC-M2 §8; notes SPEC-M3.3 §6 |
| Fixture freshness, `_meta.captured_with` | SPEC-M3.3 §4 | **monthly manual task**: capture with `sh scripts/probe-agents.sh`, `kioku hook-dump extract`, then `sh scripts/probe-agents.sh --check`; doctor `[INFO] agent.<a>.fixtures` |
| Documents index (this file), ADRs | SPEC-M3.2 §4 | |
