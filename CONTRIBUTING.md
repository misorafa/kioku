# Contributing

Thanks for helping. Issues and pull requests in Japanese or English are welcome.

## Spec first

The specifications in [docs/](docs/INDEX.md) are the contract. [docs/INDEX.md](docs/INDEX.md)
lists the section that is authoritative for each topic. When code and spec disagree, the
spec wins; if the spec is wrong or impossible, fix the spec **in the same change** and say so
in the commit message. A new feature starts as a spec section (or an ADR in
[docs/adr/](docs/adr/) for a lasting decision). After editing `docs/INDEX.md`, run
`sh scripts/check-docs.sh`.

## Toolchain (pinned)

- Rust MSRV 1.91 (`rust-version` in `Cargo.toml`), CI on 1.95, edition 2024.
- Do not add or upgrade dependencies without a reason in the commit message; the pinned list
  and the crates that must not be added (git2, database clients, ORMs, async-trait, anything
  pulling in openssl) are in [CLAUDE.md](CLAUDE.md).

## Before you push

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these on Linux, macOS and Windows, plus the `install.sh` / `install.ps1` tests.

## Code and tests

- Follow the style rules in [CLAUDE.md](CLAUDE.md): owned types in public APIs, `anyhow`
  (`thiserror` only for `kioku-core`'s error type), blocking work in `spawn_blocking`, a doc
  comment on every public function and a `//!` header on every module.
- Tests live next to the code (`#[cfg(test)]`) and use `tempfile` for data directories —
  never your real `~/.kioku`, never a running kioku service.
- Every bug fix adds a test.
- **Japanese is a first-class citizen**: any test involving search includes at least one
  Japanese query (SPEC-M1 §6.4).
- Hooks must stay fail-open and fast; do not add I/O to a hook's hot path.

## Commits and pull requests

Conventional commit messages (`feat(core): …`, `fix(cli): …`, `docs: …`). Open the PR
against `main`; keep it to one milestone or fix.
