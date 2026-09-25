//! kioku-cli: the `kioku` binary as a library — clap commands, fail-open hook handlers
//! for Claude Code, Codex, Cursor and Gemini CLI (M1 §8, M2 §3–§6), the Claude Code
//! installer (§8.5) and HTTP-client commands (§11).
//!
//! Hooks and `search` / `status` / `reindex` talk to the server over HTTP using the
//! `[client]` config; only `init` and `serve` touch the data directory.

#![warn(missing_docs)]

pub mod cli;
pub mod client;
pub mod commands;
pub mod context;
pub mod dump;
pub mod event;
pub mod hook;
pub mod install;
pub mod render;

pub use context::{SESSION_START_CAP, StartContext, render_session_start};
pub use event::{ALL_AGENTS, ALL_EVENTS, Agent, HookEnv, HookEvent, HookEventKind, parse_event};
pub use hook::{HookOutcome, StopDecision, run_hook, run_hook_with_env, stop_decision};
pub use render::{HookResult, render};
