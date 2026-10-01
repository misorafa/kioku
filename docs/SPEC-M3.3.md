# kioku — SPEC-M3.3: distribution, uninstall, and fixture freshness

Status: spec, 2026-10-02. Amends SPEC-M2 §12–§14 (install, setup, service), SPEC-M2.2
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
