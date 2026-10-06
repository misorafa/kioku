# Clean-install e2e (public release path)

Before v1.0.0 nobody can install kioku on a fleet of real, fresh machines for every
release, so these scripts do it virtually: pristine machines install the **published**
release (GitHub release assets, `install.sh` / `install.ps1` from `main`, the ghcr.io
image) exactly as README "Install" and "Adding another machine" describe, join a server
with the line `kioku invite` prints, simulate a Claude Code session with the real hook
binary, and check the product promise: the next session on **another** machine starts with
the first machine's handoff. Nothing here builds kioku from this checkout.

| script | where | what |
|--------|-------|------|
| `clean-install.sh` | any machine with Docker | ubuntu:24.04 server (README one-liner `--bind 0.0.0.0 --no-service --no-agents`, `kioku serve`, `kioku invite --host <container> --uses 2`); debian:bookworm-slim and alpine:3 clients paste the invite line; then the same with `ghcr.io/misorafa/kioku` as the server |
| `clean-install.sh --host` | macOS / Linux without Docker (CI: macos-latest) | the same install flow, with separate `HOME` directories on this host as the machines (`env -i`, port 17391, bind 127.0.0.1) |
| `macos-signature.sh` | macOS | the binary install.sh installs: `codesign --verify --strict`, Developer ID + team + hardened runtime, notarization (`spctl … context:primary-signature` → `Notarized Developer ID`) |
| `clean-install.ps1` | a throwaway Windows runner only | install.ps1 `-NoSetup` + `kioku serve` as a stand-in server, two profile directories paste the Windows invite line, same session flow with `kioku.exe` |

## Running

```sh
sh scripts/e2e/clean-install.sh                      # latest release, Docker
KIOKU_RELEASE_TAG=v0.9.4 sh scripts/e2e/clean-install.sh
sh scripts/e2e/clean-install.sh --keep               # leave containers / network for inspection
sh scripts/e2e/clean-install.sh --only install       # or --only image
sh scripts/e2e/clean-install.sh --host               # no Docker (KIOKU_E2E_PORT, default 17391)
sh scripts/e2e/macos-signature.sh                    # macOS only
```

In CI: `.github/workflows/clean-install.yml` (Actions → "clean install (e2e)" → Run
workflow, optional `tag`), weekly on Monday, and on pull requests that touch `install.sh`,
`install.ps1`, `scripts/e2e/` or the workflow. It needs no secrets.

Each step prints `PASS`, `FAIL` (with the last lines of output), `KNOWN` or `SKIP`; the
exit code is 1 when anything failed. `KNOWN` marks a failure of a bug that is already fixed
on `main` but not yet in the release under test (`known()` in `clean-install.sh` lists them
with the release after which they must pass); it turns into `FAIL` for later releases.
`SKIP`: the Docker image cannot be pulled (offline, not published yet).

## What a run checks

Server: pristine (non-root, no kioku, no Rust), the one-liner installs the release's
binary and writes `bind`, `kioku serve` answers `/api/v1/health`, `kioku setup` is
idempotent, `kioku invite` prints both lines in the documented shape.

Each client: pristine, `~/.claude` exists (Claude Code "installed"), the pasted line
installs that same version, joins, sets up Claude Code hooks (6 events) and MCP, and a new
login shell finds `kioku` on `PATH`.

Session (client A, `https` clone): SessionStart prints the `<kioku>` block; UserPromptSubmit
with a Japanese prompt containing a fake secret (`tests/fixtures/user_prompt_submit.json`);
PostToolUse (Edit, Bash); `kioku_handoff_write` through the `kioku mcp` command registered in
`~/.claude.json`; Stop. Client B (`ssh` clone of the same repository → same project id):
SessionStart shows A's handoff summary and next step and `@<A>`; `kioku search 引き継ぎ書`
finds A's session with the secret redacted; a short session without a handoff; A's next
SessionStart shows B's automatic (rules) handoff. `kioku doctor` exits 0 on every machine
with only the warnings listed in `ALLOWED_WARN_*` (`service` on a server without a
LaunchAgent / systemd unit). Image scenario: also `docker exec … kioku doctor`, and the
server's token appears in no output at all.

The scripts never print a token or a full invite code (`<code>`), and never touch a real
server or the real `~/.kioku`: Docker mode runs everything in its own containers on its own
network; `--host` runs every command with `env -i` and a temporary `HOME`, on port 17391 (it
refuses 7391), bound to 127.0.0.1.

## What cannot be tested virtually

- **LAN discovery and real networks**: `<host>.local` (mDNS), the LAN IP `kioku invite`
  picks, VPNs (Tailscale / WireGuard), Wi-Fi changes — containers use Docker DNS names, the
  host mode uses 127.0.0.1.
- **macOS firewalls and privacy prompts**: Little Snitch / LuLu allow prompts, the macOS
  Local Network permission (TCC) for a LaunchAgent, Gatekeeper's first-run prompt for a
  quarantined binary (it waits for a click), the LaunchAgent / LaunchDaemon itself
  (`--no-service` everywhere).
- **systemd --user services** (containers have no systemd) and Windows SmartScreen /
  Defender reactions to the downloaded `kioku.exe`.
- **Real agents**: Claude Code, Codex, Cursor and the desktop apps are simulated by feeding
  captured payload shapes to the real `kioku hook` binary and calling the MCP bridge
  directly; whether a new agent version still sends those shapes is
  `scripts/probe-agents.sh --check` (docs/INDEX.md "Fixture freshness").
- **Automatic updates across versions** (the server pins auto-update off during a run) and
  a Windows *server* (unsupported; `clean-install.ps1` uses one only as a stand-in).
