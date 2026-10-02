# kioku — SPEC-M3.3: distribution, uninstall, and fixture freshness

Status: implemented on branch `m3.3-distribution` (2026-10-02; notes in §6). Amends SPEC-M2 §12–§14 (install, setup, service), SPEC-M2.2
(Windows), SPEC-M2.5. Read CLAUDE.md, those specs and `packaging/winget/README.md` first.
Source: the v0.8.0 audit (PROD-9/10, CLI-L10/L13). Depends on M3.2 (docs index).

Goal: public adoption needs `brew install`, a Docker image and a clean uninstall; the
author's own three machines need nothing new. Fixtures from real agents must not rot.

## 1. Homebrew tap

- Repository `misorafa/homebrew-tap` (created by the user; the workflow needs a
  fine-grained PAT `TAP_TOKEN` with contents:write on that repo — the user adds it). The
  release workflow's last job renders `Formula/kioku.rb` from a template
  (`packaging/homebrew/kioku.rb.tmpl`) with the tag and the sha256 of the two macOS and
  two Linux tarballs, and pushes it to the tap. Skipped (notice, not failure) while
  `TAP_TOKEN` is unset.
- `brew install misorafa/tap/kioku` then `kioku setup` (or the invite line). `kioku
  update` on a brew install prints `brew upgrade kioku` instead of replacing the binary
  (detect `/opt/homebrew/Cellar/kioku` or `/usr/local/Cellar/kioku` in
  `current_exe()`, like `is_winget_install`). Auto-update on such a client only notifies.
- Tests: formula rendering golden test (script under `scripts/` with a fixture); the
  brew-path detection; the notice.

## 2. Docker image

- `release.yml` builds and pushes `ghcr.io/misorafa/kioku:<tag>` and `:latest`
  (linux/amd64 + linux/arm64, from the musl tarballs; `Dockerfile` already exists —
  make it multi-arch and use the release tarballs instead of a source build). Runs as
  non-root, `VOLUME /data`, `KIOKU_DATA_DIR=/data`, `EXPOSE 7391`, healthcheck on
  `/api/v1/health`.
- Auto-update inside a container is **off** (the image is immutable: `KIOKU_SERVICE`
  unset, and `kioku serve` detects `/.dockerenv` → notify only). Document `docker compose`
  with watchtower or a manual `docker pull` as the update path.
- `kioku init` in a container prints the token once to stdout (there is no local
  config to read) and supports `KIOKU_AUTH_TOKEN` to provision it non-interactively.
- Tests: `docker build` + `docker run … kioku --version` in CI (ubuntu job only);
  container detection unit test.

## 3. `kioku uninstall`

- `kioku uninstall [--everything] [--purge-data] [--yes]`: removes hooks and MCP entries
  from every agent config it wrote (already exists per agent), stops and removes the
  service, removes the binary and `.prev` (asks first unless `--yes`), removes the PATH
  lines install.sh/ps1 added to rc files / the user PATH (Windows), removes
  `~/.kioku/config.toml` with `--everything`, and the data directory only with
  `--purge-data` (double confirmation; prints the `kioku backup` hint first). Prints
  everything it will do, then does it. Claude desktop app: prints the quit-first note.
- winget / brew installs: leaves the binary to the package manager and says so.
- Tests: e2e on a fixture home for macOS/Linux (rc file PATH line removed, service
  definition removed, agent configs restored to their pre-install content); Windows
  test in `scripts/test-install.ps1` (user PATH entry removed).

## 4. Fixture freshness

- `scripts/probe-agents.sh` gains `--check`: compares the schema (key set) of each
  `*.captured.json` fixture with a fresh capture and prints a diff; documented as a
  monthly manual task in `docs/INDEX.md`. `kioku doctor` prints an INFO line when the
  installed agent's version (where obtainable: `codex --version`, Cursor/Claude
  settings) is newer than the one recorded in the fixture's `_meta.captured_with`
  (add that field to each fixture now).
- Capture now, on the author's machines: Antigravity Stop, Cursor `stop` and
  `beforeSubmitPrompt`, Claude Code Stop with `last_assistant_message` on macOS. Gemini
  CLI fixtures are moved to `fixtures/legacy/gemini-cli/` and its install path is
  marked legacy in README (kept working, no new features).
- Tests: `--check` on identical fixtures reports no diff; on a changed key set reports
  it; doctor INFO line.

## 5. Deliverables

Branch `m3.3-distribution`, draft PR against `main`, CI green. The user creates the tap
repository and the two secrets (`TAP_TOKEN`, and GHCR uses `GITHUB_TOKEN` with
`packages: write`). Record deviations in §6. Do not merge, tag or change secrets.

## 6. Implementation notes (2026-10-02, branch `m3.3-distribution`)

### Homebrew (§1)

- Template `packaging/homebrew/kioku.rb.tmpl`, renderer `scripts/render-homebrew-formula.sh
  <tag> <SHA256SUMS>` (POSIX sh; fails without output when a checksum is missing or
  malformed). The Linux bottles-free formula uses the **musl** tarballs (static: any
  distro). Golden test: `scripts/test-scripts.sh` against `scripts/fixtures/homebrew/`.
- `release.yml` job `homebrew` (stable tags only) renders from the release's own
  `SHA256SUMS` and pushes `Formula/kioku.rb` to `misorafa/homebrew-tap`. Notice-and-skip
  while `TAP_TOKEN` is unset or the tap cannot be cloned; the job is
  `continue-on-error`, so it can never fail a release.
- Detection (`update::package_manager`) matches any path containing `/Cellar/kioku/`
  (covers `/opt/homebrew`, `/usr/local` and Linuxbrew's `/home/linuxbrew/.linuxbrew`), on
  the given path and its resolved form (a `<prefix>/bin/kioku` link counts). Wider than
  the two prefixes the spec names, on purpose.
- Not in the spec but needed: a keg path (`…/Cellar/kioku/<version>/bin/kioku`) disappears
  after `brew upgrade` + cleanup, so hooks and the service are registered with the stable
  `<prefix>/bin/kioku` link (`update::stable_binary_path`), else every upgrade would
  break them.
- winget and Homebrew share one rule: `kioku update`, `--rollback`, the client's
  SessionStart decision and the server's update task never replace a package-managed
  binary; the notice names the manager's command. A Homebrew *server* under the kioku
  service only logs the new release (notify only), like a container.

### Docker (§2)

- `Dockerfile` no longer builds from source: the build context holds
  `linux/<arch>/kioku` and `COPY ${TARGETPLATFORM}/kioku` picks the binary. The release
  job `docker` downloads the two musl tarballs, verifies them against `SHA256SUMS`, and
  builds linux/amd64 + linux/arm64 with buildx/QEMU (actions pinned by SHA:
  setup-qemu-action v4.4.0, setup-buildx-action v4.4.1, login-action v4.6.0,
  build-push-action v7.4.0). A pre-release tag gets `:<tag>` only, never `:latest`.
- Base image `debian:trixie-slim` (was bookworm): the CI smoke test uses a glibc binary
  built on ubuntu-latest (glibc 2.39), which bookworm's glibc 2.36 cannot run; the
  release image's musl binary runs on either. `curl` is installed for the health check.
- `ENTRYPOINT ["kioku"]`, `CMD ["serve"]` (was `ENTRYPOINT ["kioku", "serve"]`) so that
  `docker run … init` works; serve options now need the subcommand
  (`… serve --port 8000`). Documented in README and CHANGELOG.
- Container detection: `/.dockerenv`, Podman's `/run/.containerenv`, or
  `KIOKU_CONTAINER=1`, which the image sets (a `/.dockerenv` is not guaranteed under
  other runtimes such as containerd/Kubernetes). In a container `kioku serve` runs the
  release check without the service marker but with `auto` forced off: it only logs
  "available … pull the new image".
- `kioku init` prints the token only when it generated it *and* runs in a container
  (once: a second `init` keeps it and prints "kept existing"); with `KIOKU_AUTH_TOKEN` it
  says "from KIOKU_AUTH_TOKEN" and never echoes it.
- CI: `scripts/test-docker.sh target/debug/kioku` in the ubuntu `check` job (image
  config, uid 10001, `init` token, a `serve` container answering `/api/v1/health` and
  reported `healthy`, no token in the log). Not run on macOS / Windows runners.

### `kioku uninstall` (§3)

- `kioku uninstall <agent>|all [--project]` already existed (per-agent removal). The
  spec's machine-wide command is the same subcommand **without** a target:
  `kioku uninstall [--everything] [--purge-data] [--yes] [--dry-run]`; the new flags
  conflict with a target. The agent part is exactly `uninstall all` (user level).
- PATH lines: install.sh already marks its line (`# added by the kioku installer`).
  Only lines that are byte-for-byte what install.sh writes for the binary's directory
  (`$HOME/…` or absolute spelling; POSIX and fish forms) are removed, from `~/.zshrc`
  (and `$ZDOTDIR/.zshrc`), `~/.bashrc`, `~/.bash_profile`, `~/.profile` and
  `~/.config/fish/conf.d/kioku.fish` (deleted when nothing else is left in it). A PATH
  entry in the Windows user PATH has no room for a comment: install.ps1 now leaves
  `kioku-path-entry.txt` next to kioku.exe when it adds the entry, and uninstall removes
  the entry only when that marker exists or the directory is install.ps1's default
  `%LOCALAPPDATA%\Programs\kioku` (installs from before the marker).
- The service is removed only when its definition exists in this home (no
  `launchctl bootout` for a service kioku did not install here).
- Confirmation: one "Proceed? [y/N]" for the whole plan (the spec's "asks first" for the
  binary is this question), and `--purge-data` additionally wants `DELETE` typed; `--yes`
  skips both. No answer on stdin (EOF) aborts with "re-run with --yes".
- A cargo build output (`…/target/…`) is never deleted (developer builds, and the test
  binary itself). On Windows the running kioku.exe is renamed aside and deleted by a
  detached `cmd` after the process exits.
- Agent configs come back to their pre-install content (JSON compared structurally,
  text byte-for-byte); the one-time `*.kioku-bak` backups install wrote are left in
  place (they are the user's own originals).

### Fixture freshness (§4)

- `probe-agents.sh --check [<dir>]` compares top-level key sets (ignoring `_meta`) of
  every `*.captured.json` under `crates/kioku-cli/tests/fixtures/` (except `legacy/`)
  with the same relative file in a fresh capture (default: the newest
  `~/.kioku/captures/<date>/`, as `kioku hook-dump extract` writes it); a
  `<event>_<variant>` fixture falls back to the fresh `<event>` file. Nested keys are not
  compared (tool inputs differ per tool). Needs `python3`. Exit 1 on a changed key set.
- `_meta.captured_with` was filled from what the fixtures themselves record:
  `cursor-agent 2026.09.26-dd393fe` (Cursor's `cursor_version` field) and `agy 1.2.12`
  (recorded with the Antigravity capture, commit 40b0e40); every other fixture (Codex,
  the Windows Claude Code and Codex captures) says `"unknown"` until it is re-captured.
- The doctor INFO line (`[INFO] agent.<a>.fixtures`, new `info` status that never changes
  the exit code) compares `<agent> --version` (`claude`, `codex`, `cursor-agent`, `agy`;
  run only for detected agents) with the `_meta.captured_with` of one embedded fixture
  per agent. "Cursor/Claude settings" in the spec turned out not to carry a version, so
  all four use `--version`. Nothing is printed for `unknown`, an equal or an older
  version.
- Gemini CLI fixtures moved to `tests/fixtures/legacy/gemini-cli/` (they are docs-derived;
  no captured Gemini payload exists). README marks the install path legacy.
- Not done here (needs the author's machines): captures of Antigravity Stop, Cursor
  `stop` / `beforeSubmitPrompt`, and Claude Code Stop with `last_assistant_message` on
  macOS; then set their `_meta.captured_with`.
