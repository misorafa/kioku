//! kioku-cli: the `kioku` binary as a library — clap commands, fail-open hook handlers
//! (spec §8), the Claude Code installer (§8.5) and HTTP-client commands (§11).
//!
//! Hooks and `search` / `status` / `reindex` talk to the server over HTTP using the
//! `[client]` config; only `init` and `serve` touch the data directory.

#![warn(missing_docs)]

pub mod cli;
pub mod client;
pub mod commands;
pub mod context;
pub mod event;
pub mod hook;
pub mod install;

pub use context::{SESSION_START_CAP, StartContext, render_session_start};
pub use event::{ALL_EVENTS, Agent, HookEvent, HookEventKind, parse_event};
pub use hook::{HookOutcome, StopDecision, run_hook, stop_decision};
