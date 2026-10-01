//! kioku-cli: the `kioku` binary as a library — clap commands, fail-open hook handlers
//! for Claude Code, Codex, Cursor and Gemini CLI (M1 §8, M2 §3–§6), the per-agent
//! installers (M1 §8.5, M2 §8), HTTP-client commands (§11) and machine setup (M2 §10–§12):
//! `serve --log-file`, `service`, `setup` and `doctor`, plus `update` (§13.3), and the
//! one-command join `invite` / `join` (SPEC-M2.3) and automatic updates (SPEC-M2.5).
//!
//! Hooks and `search` / `status` / `reindex` talk to the server over HTTP using the
//! `[client]` config; only `init`, `setup` and `serve` touch the data directory.

#![warn(missing_docs)]

pub mod auto_update;
pub mod bridge;
pub mod cli;
pub mod client;
pub mod commands;
pub mod context;
pub mod doctor;
pub mod dump;
pub mod event;
pub mod hook;
pub mod install;
pub mod invite;
pub mod logfile;
pub mod machine;
pub mod outbox;
pub mod render;
pub mod rotate;
pub mod service;
pub mod setup;
pub mod update;

pub use context::{SESSION_START_CAP, StartContext, render_session_start};
pub use event::{ALL_AGENTS, ALL_EVENTS, Agent, HookEnv, HookEvent, HookEventKind, parse_event};
pub use hook::{
    HookOutcome, NudgePolicy, StopDecision, run_hook, run_hook_with_env, stop_decision,
};
pub use render::{HookResult, render};
