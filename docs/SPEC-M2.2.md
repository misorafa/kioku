# kioku — SPEC-M2.2: Windows native client

Status: spec, 2026-09-28. Amends SPEC-M2 (and M2.1). Where this file and M2
disagree, this file wins for the points it covers. Read CLAUDE.md,
SPEC-M1.md, SPEC-M2.md (especially §3, §8, §11, §12, §19, §20, §21) first.

## 1. Goal and scope

The user runs the **Codex desktop app (Codex for Windows)** and the **Claude
desktop app** (its Code tab, i.e. Claude Code) on Windows 11, natively rather
than in WSL. The kioku server runs elsewhere (a Mac mini on the LAN, also
reachable over WireGuard). This milestone makes kioku a **client on native
Windows**: hooks for automatic capture and handoffs, plus the `kioku mcp`
stdio bridge (M2 §20) for the tools.

In scope:

- a `kioku.exe` built for `x86_64-pc-windows-msvc`, released and
  self-updating;
- `install.ps1`, a PowerShell installer;
- `kioku setup --client-only`, `install`, `uninstall`, `doctor`, `update`,
  `status`, `search`, `hook`, `mcp` and `rotate-token` (the last refuses as
  on any client) working on Windows;
- hook registration for **Claude Code** and **Codex** (primary), and for
  Cursor (best effort, UNVERIFIED);
- CI: build and test on `windows-latest`.

Out of scope: running the **server** on Windows (`serve` may compile but is
unsupported), `kioku service` on Windows, Gemini CLI and Antigravity on
Windows (leave them "not detected" unless trivially correct), aarch64
Windows, and Orca's WSL-isolated Codex homes (§9).

## 2. Sources (researched 2026-09-27; re-check before relying on UNVERIFIED items)

| id | what | where |
|----|------|-------|
| W1 | Codex for Windows: native agent with PowerShell by default; "Agent environment" can switch to WSL | https://learn.chatgpt.com/docs/windows/windows-app |
| W2 | Codex hooks: `~/.codex/hooks.json` / `config.toml`; per-hook `commandWindows` (`command_windows` in TOML) override | https://learn.chatgpt.com/docs/hooks |
| W3 | Codex runs `commandWindows` via the session shell (`pwsh -NoProfile -Command`), falling back to `%COMSPEC% /C`; a bare quoted path is a PowerShell string, not a call, so `& "C:\…\kioku.exe" …` is needed | openai/codex issues #4239 (worktrunk), gsd-core #4557 |
| W4 | Claude Code desktop reads the same `~/.claude/settings.json` hooks and `~/.claude.json` MCP as the CLI | https://code.claude.com/docs/en/desktop |
| W5 | Claude Code hooks on Windows run through Git Bash, else PowerShell; there is an **exec form** (`command` + `args`) that needs a real `.exe` and skips any shell | https://code.claude.com/docs/en/hooks |
| W6 | Claude Code writes hook stdin as raw UTF-8; runtimes decoding stdin with the ANSI code page (CP932 on Japanese Windows) garble it | anthropics/claude-code #96285 |
| W7 | Orca on Windows: terminals are PowerShell/CMD/WSL; a WSL-hosted Codex gets an isolated home | https://www.onorca.dev/docs/terminal, /docs/agents/codex |

## 3. Build and release

- **Target:** `x86_64-pc-windows-msvc`, built on `windows-latest`, and added
  to the release matrix in `.github/workflows/release.yml`.
- **Asset:** `kioku-<tag>-x86_64-pc-windows-msvc.tar.gz` containing
  `kioku.exe` plus the READMEs and licenses, with `.sha256`, and listed in
  `SHA256SUMS` like the others. Windows 10+ ships `tar.exe`, so
  `kioku update` keeps working, since it shells out to `tar`. No zip, and no
  new dependency.
- **`KIOKU_TARGET`:** baked in by build.rs as today, so `update` picks the
  Windows asset.
- **Code signing:** none for Windows in this milestone (note it in README;
  SmartScreen may warn on first run of the downloaded exe).
- **CI (`ci.yml`):** add `windows-latest` to the fmt/clippy/test matrix.
  Tests that are inherently unix (install.sh, launchd/systemd, file modes,
  `sh` fixtures) are `#[cfg(unix)]` or skipped on Windows with a reason. Every
  other test must pass on Windows.

## 4. Platform rules in the code

Follow CLAUDE.md: concrete types, no new crates. If something truly needs a
crate (e.g. `windows-sys` for a flag), ask in the PR description instead of
adding it.

1. **Home and data dir:** `%USERPROFILE%` via the existing `home_dir`, then
   `%USERPROFILE%\.kioku`. Keep the existing `KIOKU_DATA_DIR` handling.
   Paths are built with `Path::join`, never with `/` string concatenation.
2. **Canonical paths:** on Windows, `std::fs::canonicalize` returns `\\?\C:\…`
   (verbatim). git and humans want `C:\…`, so add `util::plain_path` that
   strips `\\?\` (and turns `\\?\UNC\` into `\\`). Use it wherever
   `project::identify` / `git_toplevel` canonicalize. A repo cloned from the
   same remote must get the **same project id** on Windows as on macOS; the id
   comes from the normalized remote.
3. **Spawning git (and any child) from a hook** must not flash a console
   window: on Windows set `CREATE_NO_WINDOW` (0x08000000) via
   `std::os::windows::process::CommandExt::creation_flags`, in one helper
   used by `kioku_core::git` and `project`.
4. **stdin/stdout encoding:** read hook stdin as bytes. Strip a leading UTF-8
   BOM, decode as UTF-8 (lossy is fine) and tolerate CRLF. Write
   stdout/stderr as UTF-8 bytes, never through an ANSI code page. Japanese
   prompts must survive; add a test with a BOM + CRLF + Japanese payload.
5. **File modes:** the existing `#[cfg(unix)]` 0600/0700 handling stays; on
   Windows, files inherit the user profile's ACL (document it).
   `made_private` notes are unix-only.
6. **PATH checks** (`doctor` `binary`): split with `std::env::split_paths`,
   and look for `kioku.exe`.
7. **Service:** `Platform::Unsupported("Windows: kioku runs as a client
   only")`. `setup` **without** `--client-only` on Windows fails early with:
   "Windows runs kioku as a client: kioku setup --client-only <url> <token>
   (the server runs on macOS/Linux)". `rotate-token` already refuses on
   clients.

## 5. `kioku update` on Windows

A running `kioku.exe` cannot be overwritten, but it can be renamed. So:

1. download the asset, verify it and extract it next to the exe
   (`kioku.exe.new`);
2. rename `kioku.exe` → `kioku.exe.old` (deleting a stale `.old` first,
   ignoring errors) and `kioku.exe.new` → `kioku.exe`;
3. the next run deletes a leftover `kioku.exe.old` (best effort).

There is no service to restart on Windows.

## 6. `install.ps1` (repo root, next to install.sh)

Usage, printed by `--print-client-command` when the server machine asks
for Windows (§8):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1))) -ClientOnly http://192.168.1.240:7391 <token>
```

1. Detect the arch. x64 → `x86_64-pc-windows-msvc`; anything else → a clear
   error.
2. Resolve the release: latest, or `-Version vX.Y.Z`. Honor
   `KIOKU_DOWNLOAD_BASE` and `KIOKU_REPO` like install.sh.
3. Download the asset and `SHA256SUMS`, and verify with `Get-FileHash`. On a
   mismatch, stop.
4. Install to `%LOCALAPPDATA%\Programs\kioku\kioku.exe`, overridable with
   `-InstallDir`. Replace an existing exe by the rename dance of §5.
5. PATH: if the dir is not on the **user** PATH, print the exact command to
   add it. Like install.sh, never edit it silently; `-AddToPath` opts in.
6. With `-ClientOnly <url> <token>`, run `kioku.exe setup --client-only
   <url> <token>` (plus `-NoSetup` / pass-through args as install.sh does).
7. Behaviour tests: `scripts/test-install.ps1` runs on the `windows-latest`
   CI job against a local fixture server, mirroring `scripts/test-install.sh`
   where it applies.

## 7. Hook and MCP registration on Windows

All paths are the absolute path of `kioku.exe`, with backslashes, JSON-escaped.

### 7.1 Claude Code (CLI and the desktop app's Code tab)

- **Hooks:** `%USERPROFILE%\.claude\settings.json`. Use the **exec form**
  (W5) so no shell is involved:
  `{"type": "command", "command": "C:\\Users\\u\\AppData\\Local\\Programs\\kioku\\kioku.exe", "args": ["hook", "stop"]}`.
  - UNVERIFIED: the exact exec-form field names. Check against the
    current Claude Code hooks doc. If the doc's shape differs, follow the doc
    and record it here.
  - `is_kioku_command` / `our_commands_at` / doctor must recognise both the
    string form (macOS/Linux) and the exec form (a `command` ending in
    `kioku.exe` plus `args[0] == "hook"`).
- **MCP:** unchanged from M2 §20.2:
  `{"type": "stdio", "command": "<kioku.exe>", "args": ["mcp"]}` in
  `%USERPROFILE%\.claude.json`.

### 7.2 Codex (Codex for Windows app and CLI)

- **Hooks:** `%USERPROFILE%\.codex\hooks.json` (`$CODEX_HOME` honoured). Each
  handler gets **both** fields:
  - `command`: `"C:\…\kioku.exe" hook <event> --agent codex` (a cmd.exe-safe
    quoted form, used if Codex falls back to `%COMSPEC% /C`);
  - `commandWindows`: `& "C:\…\kioku.exe" hook <event> --agent codex` (the
    PowerShell call operator; W3).

  UNVERIFIED: whether `commandWindows` is honoured in `hooks.json` (W2 says
  yes, TOML spells it `command_windows`), and which one the desktop app runs.
  Resolve with a capture (§10).
- **MCP:** the managed block in `config.toml` with
  `command = 'C:\…\kioku.exe'` (TOML literal string, so backslashes survive)
  and `args = ["mcp"]`.
- **Trust:** the doctor WARN "run /hooks in Codex" stays.

### 7.3 Cursor (best effort)

`%USERPROFILE%\.cursor\hooks.json` string command `"C:\…\kioku.exe" hook …
--agent cursor`, and MCP `{"command": …, "args": ["mcp"]}`. How Cursor runs
hook commands on Windows is UNVERIFIED.

### 7.4 Rendering rule

The hook-command renderers take the platform as a parameter
(`HookPlatform::{Unix, Windows}`, a concrete enum), not `cfg!`, so every
Windows shape is unit-tested on every OS. The installer passes
`HookPlatform::Windows` when `cfg!(windows)`.

## 8. `--print-client-command` for Windows

On the server machine, `kioku setup --print-client-command` prints the
install.sh line as today, and after it the `install.ps1` line of §6 for
Windows clients (same URL and token rule; marked as containing the token).

## 9. Orca, WSL and mixed setups (documented, not built)

- A Windows agent in a repo under `\\wsl.localhost\<distro>\…`: identity via
  git on the UNC path should still produce the remote-based id. Not tested;
  document it.
- Codex "Agent environment = WSL" and Orca's WSL agents run Linux processes.
  They need the **Linux** kioku inside WSL (`install.sh --client-only`),
  which already works. Orca's isolated Codex home under WSL may not see
  `~/.codex/hooks.json`, which is a known limitation.

## 10. Verification on the user's Windows (after CI is green)

1. Install with `install.ps1 -ClientOnly http://<server>:7391 <token>`.
2. `kioku doctor` shows no FAIL.
3. Turn on `[client] hook_dump = true`, then run one short task in the Claude
   desktop Code tab and one in Codex for Windows.
4. `kioku hook-dump extract claude-code <event>` / `codex <event>`, then
   anonymise the output (user name, paths) into
   `tests/fixtures/windows/{claude-code,codex}/*.captured.json`, and resolve
   the UNVERIFIED items of §7 here.
5. Check that a session from Windows and a session from the Mac land in the
   same project and that the handoff crosses machines.

## 11. Deliverables and definition of done (for the implementing session)

- Code, tests and docs on a branch `m2.2-windows`, with a PR against `main`
  that describes what was verified and what is still UNVERIFIED.
- CI: all jobs green, including the new `windows-latest` test job and the
  install.ps1 test. The release workflow gains the Windows build and still
  passes (dry-run it by pushing a `v0.0.0-test*` tag **only on the branch**
  if needed, then delete that tag and release).
- README / README.ja: a Windows section covering install.ps1, the
  client-only nature, the SmartScreen note and the Orca/WSL notes.
- SPEC updates in this file (UNVERIFIED items resolved or kept, with
  reasons).
- Do **not** merge, tag a real release, or touch the maintainer's machines;
  the maintainer reviews and releases.
