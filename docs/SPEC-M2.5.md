# kioku — SPEC-M2.5: automatic updates (server follows releases, clients follow the server)

Status: spec, 2026-09-30. Amends SPEC-M2 (update, service) and SPEC-M2.2 §5 (Windows
swap). Read CLAUDE.md, SPEC-M2.md (§ update / service), SPEC-M2.2.md §5 and
`crates/kioku-cli/src/update.rs` first.

> **Amended by SPEC-M2.7 (v0.9):** the order of checks is checksum → extract → signature →
> `--version` → swap (§2: nothing unverified runs). `KIOKU_DOWNLOAD_BASE` / `KIOKU_REPO`
> count only with `[update] allow_mirror = true`, and a mirror must be https unless on
> loopback (§11). `GET /api/v1/health` no longer has `version`; it is in `/status` and the
> SessionStart answer (§11). Every update keeps the replaced binary as `<exe>.prev`; a
> managed server that fails to start three times rolls back to it, and `kioku update
> --rollback` does so by hand (§7).

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

> Amended by SPEC-M2.7 §11 (`allow_mirror`) and v0.8.2 / v0.9.1 (`interval_hours`, default 1; see §3.1).

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

1. First check 3 minutes after start, then every `[update] interval_hours` (default 1 since
   v0.9.1; 4 in v0.8.2;
   clamped 1–168; was 24 h until v0.8.1 — too slow in practice) ± an eighth of jitter.
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
   restarting. **Correction (field, 2026-09-30):** the update *to* the first M2.5 release is
   run by the pre-M2.5 binary, whose `restart_service` has no such rewrite — so after that
   one manual `kioku update`, `kioku service install` is needed once. `kioku doctor`'s
   `update` check warns (fix: `kioku service install`) while an installed definition lacks
   the marker.
6. Failures (network, checksum, signature, not writable) are logged and retried at the next
   interval; the running server is never affected.

### 3.2 Server version in responses

> Amended by SPEC-M2.7 §11: `/health` no longer has `version`; it is in `/status` and the SessionStart answer.

- `POST /api/v1/sessions/start` response gains `server_version` (additive; older clients
  ignore it). `GET /api/v1/health` already has `version`.

### 3.3 Client

> Amended by SPEC-M3.4 §1: the `kioku mcp` stdio bridge runs the same decision after its
> first successful tool call, so machines that only use desktop apps (no hooks) update too.

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

## 8. Implementation notes

Recorded by the implementing session (branch `m2.5-auto-update`); where these differ from
§1–§7, this section is what the code does.

1. **Client log file.** `kioku update --background` writes to `<kioku dir>/logs/update.log`
   (rotated at 256 KiB), not `hook.log`: `kioku doctor` counts every recent `hook.log` line
   as a hook error, so a success line there would raise a false warning. The server logs to
   `serve.log` via tracing as specified.
2. **State file fields.** `auto-update.json` holds `{target, last_attempt, last_error}` as
   specified plus `last_notice` (once-per-day notice, §3.3 step 5), `last_result` (the last
   successful automatic update, for doctor) and `mismatch_since` (first SessionStart that saw
   a server of another version; doctor warns 24 h after it). Times are RFC 3339. The server
   task writes `target` / `last_attempt` / `last_error` / `last_result` to
   `<data dir>/state/auto-update.json` too, so doctor on the server machine shows its result.
3. **Who records the attempt.** The SessionStart hook records `target` + `last_attempt` when
   it decides to spawn, so a burst of SessionStarts spawns one updater even before that
   updater runs; the updater itself takes `auto-update.lock` (another updater running → exit
   0) and records `last_error` / `last_result`. The hook decides inside the handler and
   spawns in `kioku hook` after stdout/stderr are written and flushed (the spec's "after the
   context has been printed").
4. **Detaching on Unix** is `Command::process_group(0)` with null stdio, not `setsid`:
   `setsid` needs `pre_exec`, i.e. `unsafe` (CLAUDE.md rule 2). The child is in its own
   process group (no terminal signals), is never waited for, and is re-parented when the hook
   exits. Windows uses the specified creation flags.
5. **`update` status block** has one more field, `managed` (the server runs with
   `KIOKU_SERVICE=1`, the only case in which it checks). `kioku status` prints the block as
   one `update :` line.
6. **Verification is explicit** (`update::Verify { signature, exact_version }`). Automatic
   updates use `Verify::automatic()`: signature `Require` on macOS (`Skip` elsewhere) and the
   exact version. Manual `kioku update` uses `Verify::manual()`: signature `Warn`, or
   `Require` with the new flag `--require-signature`. The server-task tests pass
   `Verify::unsigned()` (exact version, no signature) because their dummy binary is a shell
   script; the macOS-only tests check that `Verify::automatic()` refuses that dummy and that
   the test executable itself fails `verify_signature` (it is only inspected, never replaced).
7. **Expected version (§4.4)** is enforced for automatic updates only: the new binary's
   `--version` output must contain the tag's version as a word, else nothing changes. A
   manual `kioku update` prints a warning and proceeds — like the signature, so a re-tagged
   mirror or self-built fork still works, and `scripts/test-install.ps1` (which re-packages
   the real `kioku.exe` under fixture tags) keeps working.
8. **Old service definitions (§3.1 step 5).** `restart_service` calls `install()` instead of
   `restart()` when the plist / unit lacks `KIOKU_SERVICE`, which rewrites the definition and
   reloads it (launchd: bootout + bootstrap; systemd: daemon-reload + restart).
9. **Graceful exit.** `kioku_server::serve_with` takes a shutdown request (`watch`
   channel); after it fires, in-flight requests get up to 10 s, then `kioku serve` returns
   exit code 75. `auto = false` logs "available" once per new tag per process (not persisted).
10. **Pre-release tags** from `releases/latest` are ignored and not recorded as
    `latest_seen` (GitHub never reports a pre-release as latest anyway).
11. **Proxy.** The update HTTP client skips environment proxies when the release base is on
    this machine or the local network (a `KIOKU_DOWNLOAD_BASE` mirror, the tests), like the
    API client already does.
12. **Notice text** lives in `auto_update.rs` (ja / en), not in `kioku_core::strings`: it is
    CLI-only. Japanese: `kioku v<server> が利用できます（この端末は v<client>）: <how>`.
13. **Windows servers** do not exist (no service manager, SPEC-M2.2), so §3.1 is
    launchd / systemd only; Windows clients follow §3.3 with the rename dance.
