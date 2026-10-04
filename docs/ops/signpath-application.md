# SignPath Foundation — OSS code-signing application (draft, 2026-10-04)

Apply at <https://signpath.org/apply> (log in with GitHub). The form fields change from time
to time; the answers below cover everything they have asked for so far. Keep the English
as is; the Japanese lines are notes for the person filling the form.

## Project

- **Project name:** kioku
- **Repository:** https://github.com/misorafa/kioku
- **Website:** https://github.com/misorafa/kioku (README in English and Japanese)
- **License:** MIT OR Apache-2.0 (dual-licensed; LICENSE-MIT and LICENSE-APACHE in the repo)
- **Short description:**
  kioku (記憶) is a self-hosted, Japanese-first shared memory server for AI coding agents
  (Claude Code, Codex CLI, Cursor, Antigravity). A single Rust binary stores session
  summaries and handoffs as Markdown in a git repository plus SQLite, indexes them with
  tantivy/lindera for Japanese full-text search, and exposes them to agents over MCP and
  lifecycle hooks, so the next session — on any agent and any machine — starts with the
  previous handoff.
- **Why code signing:** the Windows client (`kioku.exe`) is installed by end users from
  GitHub Releases, winget and a PowerShell one-liner. Unsigned, it triggers SmartScreen
  "unknown publisher" warnings and has been flagged by Defender once (an earlier
  `ExecutionPolicy Bypass` installer line, since removed). The macOS builds are already
  signed and notarized with the maintainer's Apple Developer ID; we would like the Windows
  build to meet the same bar.

## Build and release

- **Build system:** Cargo (Rust 1.95, MSRV 1.91), GitHub Actions only. No binaries are
  built on developer machines for release.
- **Release workflow:** `.github/workflows/release.yml`, triggered by a `v*` tag on `main`.
  The `x86_64-pc-windows-msvc` job builds `kioku.exe` on `windows-latest`, packages it as
  `kioku-<tag>-x86_64-pc-windows-msvc.zip` (and `.tar.gz`), computes SHA-256 sums, and the
  `release` job publishes the assets to the GitHub Release. Every third-party action is
  pinned to a commit SHA.
- **Artifact to sign:** `kioku.exe` inside the Windows zip (one PE file, ~70 MB because the
  Japanese dictionary is embedded). We would add the SignPath GitHub Action between the
  build and the release job so that only the signed binary is published.
- **Reproducibility:** builds are deterministic for a given tag and toolchain within the
  limits of Cargo; dependencies are locked (`Cargo.lock` committed).
- **Release frequency:** currently several patch releases per week during the 0.9.x
  stabilization; expected to settle to a few releases per month after 1.0.

## Team and process

- **Maintainer / approver:** Shinichiro Goto (GitHub: ShinichiroGoto), owner of the
  `misorafa` organization. Single maintainer; AI coding agents contribute under review, and
  every change lands through a pull request with CI (fmt, clippy -D warnings, tests on
  Linux/macOS/Windows, installer tests) green before merge.
- **Security contact:** see `SECURITY.md` in the repository.
- **Signing policy we propose:** release-signing only for tags on `main`; manual approval of
  each signing request by the maintainer in the SignPath portal; test-signing not needed.

## Distribution channels

- GitHub Releases (primary), `install.sh` / `install.ps1` one-liners that verify SHA-256,
  winget (`misorafa.kioku`, first submission under review), Homebrew tap (macOS/Linux),
  Docker image on GHCR (Linux only; not affected).

## 記入メモ（日本語）

- ログインは GitHub の ShinichiroGoto で。組織 misorafa の管理者であることを聞かれたら Yes。
- "Project type" は CLI / developer tool。"Number of maintainers" は 1。
- 審査結果はメールで来る（1〜3 週間）。来たら、SignPath 側で Project と Signing Policy を
  作る手順と GitHub Actions への組み込みはこちらで行う（SPEC で管理）。
- 承認されるまで v1.0.0 は無署名で出し、README に SmartScreen の説明を書く。
