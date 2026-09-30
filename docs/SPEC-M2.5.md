# kioku — SPEC-M2.5: automatic updates (server follows releases, clients follow the server)

Status: spec, 2026-09-30. Amends SPEC-M2 (update, service) and SPEC-M2.2 §5 (Windows
swap). Read CLAUDE.md, SPEC-M2.md (§ update / service), SPEC-M2.2.md §5 and
`crates/kioku-cli/src/update.rs` first.

Field problem: a typical install is one server (a Mac mini running 24/7) and several
clients (another Mac, a Windows PC). Every release needs `kioku update` on every machine;
users forget, and a client older or newer than the server is the one combination nobody
tests. Users should never have to think about updates.

## 1. Principle

- **The server follows GitHub releases.** It checks once a day and updates itself.
- **Clients follow the server, not GitHub.** A client updates to *the server's version*
  (up or, never, down — §3.3), so the fleet converges on one version and a client is never
  ahead of the server it talks to.
- **Nothing blocks an agent.** All checks and downloads happen off the hook's critical path.
- **Same verification as `kioku update`, plus the macOS signature** (§4).

## 2. Configuration

New table in `config.toml` (both roles read it):

```toml
[update]
auto = true          # default true; false = never update automatically (notify only)
channel = "stable"   # "stable" = releases without "-" in the tag (only value in M2.5)
```

- Missing table → defaults (auto on). `kioku setup`, `kioku join` and the installers do not
  write it; `kioku doctor` shows the effective values.
- Env override for tests and CI: `KIOKU_AUTO_UPDATE=0|1`.
- `kioku update` (manual) keeps working exactly as today, regardless of `auto`.

## 3. Behaviour

### 3.1 Server

A task inside `kioku serve` (tokio interval, runs the blocking work in `spawn_blocking`):

1. First check 10 minutes after start, then every 24 h ± up to 1 h of jitter.
2. Resolve the latest release tag the same way as `kioku update` (`HEAD …/releases/latest`;
   `KIOKU_REPO` / `KIOKU_DOWNLOAD_BASE` still apply). Tags containing `-` are ignored.
3. If newer than the running version and `auto` is on: download, verify (§4), swap the
   binary with the existing `replace` / `swap_binary`, write a line to the server log, then
   **exit with code 75** after finishing in-flight requests (graceful shutdown, max 10 s).
   The service manager restarts it: launchd `KeepAlive.SuccessfulExit=false`, systemd
   `Restart=on-failure` — both treat 75 as a failure and restart. Do **not** call
   `restart_service` from inside the server (it would kill itself mid-way).
4. If `auto` is off: log once per new tag and expose it (§3.4). No download.
5. Only when kioku runs under the service manager (`kioku service` installed and the process
   was started by it — env marker `KIOKU_SERVICE=1` set in the plist / unit). A foreground
   `kioku serve` in a terminal never self-updates (it would just exit).
   Existing services were installed without the marker: `restart_service` rewrites the plist / unit when it lacks `KIOKU_SERVICE=1` before
   restarting, so the one manual update to the first M2.5 release enables it.
6. Failures (network, checksum, signature, not writable) are logged and retried at the next
   interval; the running server is never affected.

### 3.2 Server version in responses

- `POST /api/v1/sessions/start` response gains `server_version` (additive; older clients
  ignore it). `GET /api/v1/health` already has `version`.

### 3.3 Client

In the SessionStart hook, after the context has been printed:

1. If `server_version` is present, strictly newer than the client's own version, has no `-`,
   and `auto` is on → spawn a **detached** background process
   `kioku update --version v<server_version> --background` and return immediately.
   - Detached: on Unix `setsid`-style (new process group, stdio to null); on Windows
     `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`. It must survive the
     hook process and the agent exiting.
   - Never downgrade: a client newer than the server does nothing (logs a note in
     `kioku doctor`).
2. Throttle: at most one attempt per target version per 6 h, recorded in
   `~/.kioku/state/auto-update.json` (`{target, last_attempt, last_error}`). A lock file
   (`auto-update.lock`, create-exclusive, stale after 15 min) prevents parallel updates when
   several agents start at once.
3. `--background` mode of `kioku update`: no stdout, writes the result to the kioku log file
   and to `auto-update.json`; exit code as usual. Everything else is the existing flow
   (checksum, rename dance on Windows, `restart_service` for a client with a service —
   clients normally have none).
4. The update takes effect for the next hook call; the running `kioku mcp` bridge keeps the
   old binary until the agent restarts it (Windows: the rename dance already handles the
   locked exe; macOS/Linux: the old inode stays valid).
5. `auto` off, or a winget install (`is_winget_install`), or the binary directory is not
   writable → no update; instead the `<kioku>` block adds one line:
   `kioku v<server> is available (you have v<client>): <how>` where `<how>` is
   `winget upgrade misorafa.kioku`, `kioku update`, or the installer line from the existing
   not-writable message. Shown at most once per day per machine (same state file).

### 3.4 Visibility

- `kioku status` / `GET /api/v1/status`: `update: {auto, latest_seen, last_check,
  last_error}` for the server (additive).
- `kioku doctor`: new check `update` — effective config, server vs client version
  (warn when they differ for more than 24 h), last auto-update result from the state file.
- Every successful automatic update writes one line to the log:
  `kioku: auto-updated vA -> vB (server|client)`.

## 4. Verification

1. SHA-256 against `SHA256SUMS` / `<asset>.sha256` exactly as `replace` does today. No
   checksum → refuse.
2. **macOS only:** before the swap, verify the extracted binary:
   `codesign --verify --strict <bin>` succeeds **and** `codesign -dv` reports
   `TeamIdentifier=7F6HLTW75D` (constant `APPLE_TEAM_ID` in update.rs). Failure → refuse,
   keep the old binary. Applies to automatic updates only; manual `kioku update` warns but
   proceeds (so a self-built fork still works), unless `--require-signature`.
3. Windows / Linux: checksum only (no code signing in M2.5; see SPEC-M2.2).
4. The new binary must run `--version` and report the expected tag before the swap
   (today it only has to run).

## 5. Out of scope

- Pre-release / beta channel (field reserved).
- Updating agents' hook or MCP config on update (paths don't change).
- Coordinated rollback. A bad release is fixed by releasing a newer one; `kioku update
  --version <old>` still works manually and a client then stays behind the server until the
  next release (it never downgrades automatically).

## 6. Tests

- update.rs: tag filtering (`-` ignored), "server newer / equal / older" decision, throttle
  and lock (stale lock taken over), `--background` writes the state file, expected-version
  check in `replace`, macOS team-id parsing of `codesign -dv` output (unit test on a
  captured string; the real `codesign` call is `#[cfg(target_os = "macos")]` and uses the
  test binary itself → not signed by the team → refused).
- Server: the update task with a local fake release server (existing `serve` helper) —
  newer tag → binary swapped and the task requests exit 75; same tag → nothing; checksum
  mismatch → nothing, error recorded; `auto = false` → no download, `latest_seen` set.
  Use a copy of a dummy binary in a tempdir, never the test's own exe.
- API: `server_version` in session start; `update` block in status.
- CLI e2e: SessionStart against a server reporting a newer version spawns exactly one
  background updater (observe via the state file with `KIOKU_DOWNLOAD_BASE` pointing at the
  fake server); a second SessionStart within 6 h does not; a winget-path client prints the
  notice line instead.
- Service files: plist and unit carry `KIOKU_SERVICE=1`; exit 75 is restarted (unit test on
  the generated text).
- Japanese: the notice line has a Japanese variant when `client.lang = ja` (CLAUDE.md rule 8
  is for search tests; this is just the existing bilingual rule).

## 7. Deliverables (for the implementing session)

Branch `m2.5-auto-update`, draft PR against `main`, CI green on every job. README /
README.ja: short "Updates" section (automatic by default, how to turn off, winget).
Record anything that turned out different in this spec. Do not merge, tag or change
secrets.
