# kioku — agent conventions

kioku (記憶) is a self-hosted, Japanese-first shared memory server for AI coding
agents (Claude Code, Codex CLI, Cursor, Gemini CLI, …). Single Rust binary,
Markdown-in-git as the source of truth, tantivy + lindera for full-text search,
MCP over streamable HTTP, lifecycle hooks for automatic capture and handoffs.

Read `docs/SPEC-M1.md` before touching code. It is the contract; when the spec
and the code disagree, the spec wins unless the spec is impossible — then fix
the spec in the same change and say so in the commit message.

## Toolchain (pinned — do not "upgrade" without a reason stated in the commit)

- Rust: MSRV 1.91 (`rust-version` in Cargo.toml; CI and Docker build on 1.95), edition 2024, workspace at repo root.
- tantivy 0.26, lindera-tantivy 5 (+ lindera 5 with `embed-ipadic`, and
  lindera-analysis 5 for the NFKC character filter — already a lindera-tantivy
  dependency), rmcp 3.4
  (`server`, `transport-streamable-http-server`, `macros`, `schemars`),
  axum 0.8, tokio (multi-thread), rusqlite 0.40 (`bundled`), clap 4 (`derive`),
  serde / serde_json, serde_yaml_ng (frontmatter), anyhow + thiserror, tracing,
  chrono, regex, sha2, reqwest (`json`, `rustls-tls`, no `default-features`).
- Do NOT add: git2 (shell out to `git`), Neo4j/Postgres clients, ORMs, any
  async trait crate, any crate that pulls in openssl. Ask in the summary if
  you think another dependency is needed.

## Rust style rules (these exist to keep the agent loop fast — follow them)

1. No lifetimes in public APIs. Return owned `String` / `Vec<T>`; clone freely.
2. Shared state = `Arc<T>` with `parking_lot::Mutex` (or tokio `Mutex` only when
   held across `.await`). No `RefCell`, no `unsafe`, no raw pointers.
3. Errors: `anyhow::Result` everywhere except `kioku-core`'s public error type
   (`thiserror`). Add context with `.with_context(|| ...)`.
4. Concrete types over generics. No custom traits unless two implementations
   exist today.
5. Blocking work (SQLite, tantivy, git, filesystem) runs inside
   `tokio::task::spawn_blocking`; never block the async runtime.
6. Every public function has a one-line doc comment. Every module has a
   `//!` header saying what it owns.
7. Tests live next to the code (`#[cfg(test)]`) and use `tempfile` for data
   dirs. Every bug fix adds a test.
8. Japanese is a first-class citizen: any test involving search MUST include
   at least one Japanese query (see spec §6.4 for the required cases).
9. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean.

## Workflow for a coding step

- Work only on the step you were given. Do not start the next step.
- Loop: `cargo check -p <crate>` → write tests → `cargo test -p <crate>`.
  Use `cargo build` only when you need the binary.
- First build of tantivy/lindera takes several minutes on this machine. Run it
  once in the background early (`cargo build 2>&1 | tail -3`), then rely on
  incremental checks.
- Commit at the end with a conventional message (`feat(core): …`). Do not
  commit `target/`.
- Finish with a short summary: what was built, what tests exist, what is
  deliberately left out, and anything in the spec that turned out wrong.

## Directory layout

```
crates/kioku-core     store, index, project identity, observations, handoffs, summary rules
crates/kioku-server   axum HTTP API + MCP (rmcp) — no business logic here
crates/kioku-cli      the `kioku` binary: serve / init / hook / install / search / status
docs/                 SPEC-M1.md and later specs; ADRs in docs/adr/
```
