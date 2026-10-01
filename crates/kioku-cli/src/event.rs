//! Agent-neutral hook events (M1 §8.1, M2 §3) and the per-agent stdin parsers.
//!
//! `kioku hook <neutral event> --agent <name>` picks the parser; each one maps its agent's
//! payload onto [`HookEvent`] — session id / cwd resolution (M2 §3.4) and tool
//! normalization onto Claude Code's tool names (§3.5) included — so the handlers and the
//! core digest have a single code path. Parsers are tolerant: a missing optional field
//! never fails the event, only a missing session id does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::ValueEnum;
use serde_json::{Map, Value, json};

/// Which agent produced the hook payload on stdin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    /// Anthropic Claude Code.
    ClaudeCode,
    /// OpenAI Codex CLI.
    Codex,
    /// Cursor (desktop editor and the `agent` CLI).
    Cursor,
    /// Google Gemini CLI.
    GeminiCli,
    /// Google Antigravity CLI (`agy`, M2.1).
    Antigravity,
}

/// Every agent, in the order of the spec tables.
pub const ALL_AGENTS: [Agent; 5] = [
    Agent::ClaudeCode,
    Agent::Codex,
    Agent::Cursor,
    Agent::GeminiCli,
    Agent::Antigravity,
];

impl Agent {
    /// Agent name as stored on sessions and passed to `--agent` (`claude-code`, `codex`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
            Agent::Cursor => "cursor",
            Agent::GeminiCli => "gemini-cli",
            Agent::Antigravity => "antigravity",
        }
    }

    /// Product name for messages (`Claude Code`, `Codex`, …).
    pub fn display_name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Cursor => "Cursor",
            Agent::GeminiCli => "Gemini CLI",
            Agent::Antigravity => "Antigravity",
        }
    }

    /// Environment variables naming the project dir, in resolution order (M2 §3.4 step 3).
    pub fn project_dir_env(self) -> &'static [&'static str] {
        match self {
            Agent::ClaudeCode => &["CLAUDE_PROJECT_DIR"],
            Agent::Codex => &[],
            Agent::Cursor => &["CURSOR_PROJECT_DIR", "CLAUDE_PROJECT_DIR"],
            Agent::GeminiCli => &["GEMINI_PROJECT_DIR", "GEMINI_CWD", "CLAUDE_PROJECT_DIR"],
            Agent::Antigravity => &[],
        }
    }
}

/// The lifecycle event a hook invocation handles (`kioku hook <event>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HookEventKind {
    /// Session started, resumed, cleared or compacted.
    SessionStart,
    /// The user submitted a prompt.
    UserPromptSubmit,
    /// A tool call finished.
    PostToolUse,
    /// The agent finished a turn.
    Stop,
    /// Context is about to be compacted.
    PreCompact,
    /// The session ended.
    SessionEnd,
}

/// Every event kind, in lifecycle order.
pub const ALL_EVENTS: [HookEventKind; 6] = [
    HookEventKind::SessionStart,
    HookEventKind::UserPromptSubmit,
    HookEventKind::PostToolUse,
    HookEventKind::Stop,
    HookEventKind::PreCompact,
    HookEventKind::SessionEnd,
];

impl HookEventKind {
    /// kebab-case name used on the command line (`session-start`, …).
    pub fn cli_name(self) -> &'static str {
        match self {
            HookEventKind::SessionStart => "session-start",
            HookEventKind::UserPromptSubmit => "user-prompt-submit",
            HookEventKind::PostToolUse => "post-tool-use",
            HookEventKind::Stop => "stop",
            HookEventKind::PreCompact => "pre-compact",
            HookEventKind::SessionEnd => "session-end",
        }
    }

    /// Claude Code's `hook_event_name` / settings.json key (`SessionStart`, …).
    pub fn claude_code_name(self) -> &'static str {
        match self {
            HookEventKind::SessionStart => "SessionStart",
            HookEventKind::UserPromptSubmit => "UserPromptSubmit",
            HookEventKind::PostToolUse => "PostToolUse",
            HookEventKind::Stop => "Stop",
            HookEventKind::PreCompact => "PreCompact",
            HookEventKind::SessionEnd => "SessionEnd",
        }
    }
}

/// Hook timeout (ms) that `kioku install` registers for an agent's event (M2 §4.2, §5.2,
/// §6.2, M2.1 §3.8 — Antigravity writes whole seconds; Claude Code: 10 s SessionStart, the agent's 60 s default elsewhere).
pub fn registered_timeout_ms(agent: Agent, event: HookEventKind) -> u64 {
    use HookEventKind::*;
    match (agent, event) {
        (_, SessionStart) => 10_000,
        (Agent::ClaudeCode, _) => 60_000,
        (Agent::Codex, SessionEnd) => 3_000,
        (_, Stop) => 10_000,
        _ => 5_000,
    }
}

/// Hard deadline (ms) of one invocation: `[client] timeout_ms`, capped at the registered
/// agent timeout minus 500 ms (M2 §3.10; Codex SessionEnd → 2 500 ms).
pub fn hook_deadline_ms(agent: Agent, event: HookEventKind, timeout_ms: u64) -> u64 {
    let cap = registered_timeout_ms(agent, event).saturating_sub(500);
    timeout_ms.min(cap).max(1)
}

/// The environment a hook runs in; injected so tests never read or mutate the real one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookEnv {
    /// Environment variables.
    pub vars: HashMap<String, String>,
    /// Home directory (`~/.cursor/hooks.json`, `~/.kioku` fallback).
    pub home: Option<PathBuf>,
    /// Process working directory (last-resort cwd for every agent but Cursor).
    pub cwd: Option<PathBuf>,
}

impl HookEnv {
    /// The real process environment, home directory and cwd.
    pub fn from_process() -> HookEnv {
        HookEnv {
            vars: kioku_core::util::env_vars(),
            home: kioku_core::util::home_dir_opt(),
            cwd: std::env::current_dir().ok(),
        }
    }

    /// A non-empty environment variable.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// A hook payload in agent-neutral form.
#[derive(Clone, Debug, PartialEq)]
pub struct HookEvent {
    /// Agent label (`claude-code`, `codex`, `cursor`, `gemini-cli`).
    pub agent: String,
    /// Which event this is.
    pub event: HookEventKind,
    /// Agent session id.
    pub session_id: String,
    /// Resolved working directory (M2 §3.4 steps 1–3); empty when none was found.
    pub cwd: String,
    /// SessionStart source (`startup` | `resume` | `clear` | `compact` | `fork`).
    pub source: Option<String>,
    /// UserPromptSubmit prompt text.
    pub prompt: Option<String>,
    /// PostToolUse tool name, normalized to Claude Code's names (M2 §3.5).
    pub tool_name: Option<String>,
    /// PostToolUse tool input (normalized).
    pub tool_input: Option<Value>,
    /// PostToolUse tool response (normalized).
    pub tool_response: Option<Value>,
    /// PostToolUse tool call id.
    pub tool_use_id: Option<String>,
    /// Stop: true when the agent is already continuing because of a Stop hook
    /// (Cursor: `loop_count > 0`).
    pub stop_hook_active: bool,
    /// PreCompact trigger (`manual` | `auto`).
    pub trigger: Option<String>,
    /// SessionEnd reason.
    pub reason: Option<String>,
    /// The payload exactly as received.
    pub raw: Value,
    /// Native event name as sent (`hook_event_name`), empty if absent.
    pub native_event: String,
    /// Workspace roots (Cursor `workspace_roots`, Antigravity `workspacePaths`).
    pub workspace_roots: Vec<String>,
    /// Native tool name before normalization, e.g. `apply_patch`, `Shell`, `run_shell_command`.
    pub native_tool: Option<String>,
    /// Turn id (Codex `turn_id`), informational.
    pub turn_id: Option<String>,
    /// Cursor stop `loop_count` (0 on the first stop of a follow-up chain).
    pub loop_count: Option<u32>,
    /// Cursor stop `status` (`completed` | `aborted` | `error`).
    pub stop_status: Option<String>,
    /// Stop: the agent's final reply of the turn — Claude Code / Codex
    /// `last_assistant_message`, Gemini CLI `prompt_response` (SPEC-M3.0 §3).
    pub assistant_message: Option<String>,
}

/// Parses a hook payload using only the payload (no environment fallbacks).
pub fn parse_event(agent: Agent, event: HookEventKind, stdin: &str) -> anyhow::Result<HookEvent> {
    parse_event_env(agent, event, stdin, &HookEnv::default())
}

/// Parses a hook payload from stdin for the given agent and event; `env` supplies the
/// project-dir variables of M2 §3.4.
pub fn parse_event_env(
    agent: Agent,
    event: HookEventKind,
    stdin: &str,
    env: &HookEnv,
) -> anyhow::Result<HookEvent> {
    let raw: Value = serde_json::from_str(stdin.trim()).context("hook stdin is not JSON")?;
    parse_value(agent, event, raw, env)
}

/// [`parse_event_env`] on an already-parsed payload.
pub fn parse_value(
    agent: Agent,
    event: HookEventKind,
    raw: Value,
    env: &HookEnv,
) -> anyhow::Result<HookEvent> {
    match agent {
        Agent::ClaudeCode => parse_claude_code_env(event, raw, env),
        Agent::Codex => parse_codex(event, raw, env),
        Agent::Cursor => parse_cursor(event, raw, env),
        Agent::GeminiCli => parse_gemini(event, raw, env),
        Agent::Antigravity => parse_antigravity(event, raw, env),
    }
}

/// Claude Code stdin → [`HookEvent`] (fields per M1 §8.1), without environment fallbacks.
pub fn parse_claude_code(event: HookEventKind, raw: Value) -> anyhow::Result<HookEvent> {
    parse_claude_code_env(event, raw, &HookEnv::default())
}

fn parse_claude_code_env(
    event: HookEventKind,
    raw: Value,
    env: &HookEnv,
) -> anyhow::Result<HookEvent> {
    let ev = common(Agent::ClaudeCode, event, raw, env, &["session_id"])?;
    Ok(HookEvent {
        tool_name: text(&ev.raw, "tool_name"),
        tool_input: value(&ev.raw, "tool_input"),
        tool_response: value(&ev.raw, "tool_response"),
        ..ev
    })
}

/// Codex CLI stdin → [`HookEvent`] (M2 §4.3): `Bash` and `apply_patch` normalized.
pub fn parse_codex(event: HookEventKind, raw: Value, env: &HookEnv) -> anyhow::Result<HookEvent> {
    let mut ev = common(Agent::Codex, event, raw, env, &["session_id"])?;
    ev.tool_response = value(&ev.raw, "tool_response");
    let input = value(&ev.raw, "tool_input");
    if let Some(native) = text(&ev.raw, "tool_name") {
        let (name, input) = match native.as_str() {
            "Bash" | "shell" | "exec_command" => ("Bash".to_string(), bash_input(input)),
            "apply_patch" => ("Edit".to_string(), apply_patch_input(input, &ev.cwd)),
            _ => (native.clone(), input),
        };
        ev.tool_name = Some(name);
        ev.tool_input = input;
        ev.native_tool = Some(native);
    } else {
        ev.tool_input = input;
    }
    Ok(ev)
}

/// Cursor stdin → [`HookEvent`] (M2 §5.3). Accepts the native shape (`conversation_id`,
/// `workspace_roots`) and a Claude-shaped one (`session_id`, `cwd`) alike (§3.7).
pub fn parse_cursor(event: HookEventKind, raw: Value, env: &HookEnv) -> anyhow::Result<HookEvent> {
    let mut ev = common(
        Agent::Cursor,
        event,
        raw,
        env,
        &["conversation_id", "session_id"],
    )?;
    ev.stop_hook_active = ev.stop_hook_active || ev.loop_count.is_some_and(|n| n > 0);
    if event != HookEventKind::PostToolUse {
        return Ok(ev);
    }
    let raw = &ev.raw;
    let is_file_edit = ev.native_event == "afterFileEdit"
        || (raw.get("tool_name").is_none() && raw.get("edits").is_some());
    if is_file_edit {
        let edits = raw
            .get("edits")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        ev.native_tool = Some("afterFileEdit".to_string());
        ev.tool_name = Some("Edit".to_string());
        ev.tool_input = text(raw, "file_path").map(|p| json!({ "file_path": p }));
        ev.tool_response = Some(json!({ "edits": edits }));
        return Ok(ev);
    }
    let input = value(raw, "tool_input");
    let native = text(raw, "tool_name");
    let (name, input) = match native.as_deref() {
        Some("Shell") => (Some("Bash".to_string()), bash_input(input)),
        Some("Read") => {
            let path = input.as_ref().and_then(|i| {
                ["file_path", "path", "target_file", "filePath"]
                    .iter()
                    .find_map(|k| text(i, k))
            });
            let input = match path {
                Some(p) => Some(json!({ "file_path": p })),
                None => input,
            };
            (Some("Read".to_string()), input)
        }
        _ => (native.clone(), input),
    };
    ev.tool_name = name;
    ev.tool_input = input;
    ev.native_tool = native;
    ev.tool_response = match raw.get("tool_output") {
        Some(Value::String(s)) => {
            Some(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())))
        }
        Some(Value::Null) | None => value(raw, "tool_response"),
        Some(other) => Some(other.clone()),
    };
    let failed = ev.native_event == "postToolUseFailure" || raw.get("error_message").is_some();
    if failed {
        let mut resp = Map::new();
        resp.insert("is_error".into(), Value::Bool(true));
        resp.insert(
            "error".into(),
            raw.get("error_message").cloned().unwrap_or(Value::Null),
        );
        if let Some(t) = value(raw, "failure_type") {
            resp.insert("failure_type".into(), t);
        }
        ev.tool_response = Some(Value::Object(resp));
    }
    Ok(ev)
}

/// Gemini CLI stdin → [`HookEvent`] (M2 §6.3): shell / file tools normalized, an
/// `tool_response.error` marks the call as failed.
pub fn parse_gemini(event: HookEventKind, raw: Value, env: &HookEnv) -> anyhow::Result<HookEvent> {
    let mut ev = common(Agent::GeminiCli, event, raw, env, &["session_id"])?;
    let input = value(&ev.raw, "tool_input");
    let file_input = |input: Option<Value>| {
        let path = input.as_ref().and_then(|i| {
            ["file_path", "absolute_path", "path"]
                .iter()
                .find_map(|k| text(i, k))
        });
        match path {
            Some(p) => Some(json!({ "file_path": p })),
            None => input,
        }
    };
    if let Some(native) = text(&ev.raw, "tool_name") {
        let (name, input) = match native.as_str() {
            "run_shell_command" => ("Bash", bash_input(input)),
            "write_file" => ("Write", file_input(input)),
            "replace" => ("Edit", file_input(input)),
            "read_file" => ("Read", file_input(input)),
            other => (other, input),
        };
        ev.tool_name = Some(name.to_string());
        ev.tool_input = input;
        ev.native_tool = Some(native);
    } else {
        ev.tool_input = input;
    }
    ev.tool_response = value(&ev.raw, "tool_response").map(|mut resp| {
        let failed = resp
            .get("error")
            .is_some_and(|e| !e.is_null() && e.as_str() != Some(""));
        if failed && let Value::Object(map) = &mut resp {
            map.insert("is_error".into(), Value::Bool(true));
        }
        resp
    });
    Ok(ev)
}

/// Antigravity CLI stdin → [`HookEvent`] (M2.1 §3.3): `conversationId`, `workspacePaths`;
/// no event name, cwd or prompt in the payload (the prompt comes from the transcript, §3.5).
pub fn parse_antigravity(
    event: HookEventKind,
    raw: Value,
    env: &HookEnv,
) -> anyhow::Result<HookEvent> {
    let mut ev = common(Agent::Antigravity, event, raw, env, &["conversationId"])?;
    if let Some(call) = ev.raw.get("toolCall").cloned() {
        let native = text(&call, "name");
        let args = call.get("args").cloned();
        let arg = |key: &str| args.as_ref().and_then(|a| text(a, key));
        // M2.1 §3.3: `view_file` is verified (agy 1.2.12); the others follow the same
        // argument naming and are UNVERIFIED — unknown tools keep their name and input.
        let (name, input) = match native.as_deref() {
            Some("view_file") => (
                "Read",
                arg("AbsolutePath").map(|p| json!({ "file_path": p })),
            ),
            Some("write_to_file") => (
                "Write",
                arg("TargetFile").map(|p| json!({ "file_path": p })),
            ),
            Some("replace_file_content" | "multi_replace_file_content") => {
                ("Edit", arg("TargetFile").map(|p| json!({ "file_path": p })))
            }
            Some("run_command") => ("Bash", arg("CommandLine").map(|c| json!({ "command": c }))),
            _ => ("", None),
        };
        if name.is_empty() || input.is_none() {
            ev.tool_name = native.clone();
            ev.tool_input = args;
        } else {
            ev.tool_name = Some(name.to_string());
            ev.tool_input = input;
        }
        ev.native_tool = native;
    }
    if event == HookEventKind::PostToolUse {
        ev.tool_response = Some(match text(&ev.raw, "error") {
            Some(e) => json!({ "is_error": true, "error": e }),
            None => json!({}),
        });
    }
    Ok(ev)
}

/// Fields every agent shares; tool fields are left to the agent parser.
fn common(
    agent: Agent,
    event: HookEventKind,
    raw: Value,
    env: &HookEnv,
    session_keys: &[&str],
) -> anyhow::Result<HookEvent> {
    if !raw.is_object() {
        anyhow::bail!("hook stdin is not a JSON object");
    }
    let mut session_id = session_keys.iter().find_map(|k| text(&raw, k));
    if session_id.is_none() {
        let var = match agent {
            Agent::GeminiCli => Some("GEMINI_SESSION_ID"),
            Agent::Antigravity => Some("ANTIGRAVITY_CONVERSATION_ID"),
            _ => None,
        };
        session_id = var.and_then(|k| env.var(k)).map(str::to_string);
    }
    let session_id = session_id.with_context(|| {
        format!(
            "hook payload has no session id ({})",
            session_keys.join(" / ")
        )
    })?;
    let workspace_roots: Vec<String> = raw
        .get("workspace_roots")
        .or_else(|| raw.get("workspacePaths"))
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let cwd = resolve_cwd(agent, &raw, &workspace_roots, env);
    Ok(HookEvent {
        agent: agent.as_str().to_string(),
        event,
        session_id,
        cwd,
        source: text(&raw, "source"),
        // The prompt is kept verbatim even when empty-looking: an empty prompt is still a prompt.
        prompt: raw
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_name: None,
        tool_input: None,
        tool_response: None,
        tool_use_id: text(&raw, "tool_use_id"),
        stop_hook_active: raw
            .get("stop_hook_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        trigger: text(&raw, "trigger"),
        reason: text(&raw, "reason"),
        native_event: text(&raw, "hook_event_name").unwrap_or_default(),
        workspace_roots,
        native_tool: None,
        turn_id: text(&raw, "turn_id"),
        loop_count: raw
            .get("loop_count")
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        stop_status: text(&raw, "status"),
        assistant_message: text(&raw, "last_assistant_message")
            .or_else(|| text(&raw, "prompt_response"))
            .filter(|m| !m.trim().is_empty()),
        raw,
    })
}

/// cwd per M2 §3.4 steps 1–3: payload `cwd`, `workspace_roots[0]` (Cursor: the root first —
/// its payload `cwd` can be a subdirectory or the hook's own `~/.cursor`), then the agent's
/// project-dir variables — the first non-empty absolute path. Empty when none qualifies
/// (the handler decides about the process cwd, which Cursor never uses).
fn resolve_cwd(agent: Agent, raw: &Value, roots: &[String], env: &HookEnv) -> String {
    let payload = text(raw, "cwd");
    let root = roots.first().cloned();
    let (first, second) = if agent == Agent::Cursor {
        (root, payload)
    } else {
        (payload, root)
    };
    let candidates = first.into_iter().chain(second).chain(
        agent
            .project_dir_env()
            .iter()
            .filter_map(|k| env.var(k).map(str::to_string)),
    );
    for c in candidates {
        if is_absolute_anywhere(&c) {
            return c;
        }
    }
    String::new()
}

/// `C:\…` / `C:/…` count as absolute even when kioku runs on unix (payload from Windows).
fn looks_like_windows_abs(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// `rel` joined onto a payload `cwd` with the separator of the OS the cwd comes from (the
/// agent's, which need not be kioku's): `/` for `/…`, `\` for `C:\…` / `\\host\…`.
fn join_payload_path(cwd: &str, rel: &str) -> String {
    if cwd.starts_with('/') {
        format!("{}/{rel}", cwd.trim_end_matches('/'))
    } else if looks_like_windows_abs(cwd) || cwd.starts_with(r"\\") {
        format!(
            "{}\\{}",
            cwd.trim_end_matches(['\\', '/']),
            rel.replace('/', "\\")
        )
    } else {
        Path::new(cwd).join(rel).display().to_string()
    }
}

/// True for an absolute path of any OS, whatever OS kioku runs on: this OS's own rule, a
/// Windows drive path, a UNC path (`\\host\share`), or a unix path (`/…`, which Windows'
/// `Path::is_absolute` rejects for lacking a drive).
fn is_absolute_anywhere(p: &str) -> bool {
    Path::new(p).is_absolute()
        || looks_like_windows_abs(p)
        || p.starts_with('/')
        || p.starts_with(r"\\")
}

/// `{command}` from a shell tool input whose `command` is a string or an argv array.
fn bash_input(input: Option<Value>) -> Option<Value> {
    let command = match input.as_ref().and_then(|i| i.get("command")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => return input,
    };
    Some(json!({ "command": command }))
}

/// Maximum chars of an `apply_patch` patch kept in the observation (M2 §3.5).
pub const PATCH_KEEP_CHARS: usize = 4000;

/// Codex `apply_patch` input → `{file_paths, patch}`; paths are extracted from the whole
/// patch before it is truncated, relative ones joined onto the session cwd.
fn apply_patch_input(input: Option<Value>, cwd: &str) -> Option<Value> {
    let patch = match input.as_ref() {
        Some(Value::String(s)) => Some(s.clone()),
        Some(obj) => ["command", "patch", "input"]
            .iter()
            .find_map(|k| match obj.get(*k) {
                Some(Value::String(s)) => Some(s.clone()),
                // `["apply_patch", "<patch>"]`: the element that holds the patch.
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|s| s.contains("*** "))
                    .map(str::to_string),
                _ => None,
            }),
        None => None,
    };
    let Some(patch) = patch else {
        return input;
    };
    let paths: Vec<Value> = patch_paths(&patch)
        .into_iter()
        .map(|p| {
            if is_absolute_anywhere(&p) || cwd.is_empty() {
                Value::String(p)
            } else {
                Value::String(join_payload_path(cwd, &p))
            }
        })
        .collect();
    Some(json!({
        "file_paths": paths,
        "patch": kioku_core::util::truncate_chars(&patch, PATCH_KEEP_CHARS),
    }))
}

/// File paths named by an `apply_patch` patch: `*** Add|Update|Delete File: <p>` and
/// `*** Move to: <p>` lines, deduplicated in order.
pub fn patch_paths(patch: &str) -> Vec<String> {
    const PREFIXES: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(p) = PREFIXES.iter().find_map(|pre| line.strip_prefix(pre)) {
            let p = p.trim();
            if !p.is_empty() && !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// A non-empty string field.
fn text(raw: &Value, key: &str) -> Option<String> {
    raw.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A non-null field.
fn value(raw: &Value, key: &str) -> Option<Value> {
    raw.get(key).filter(|v| !v.is_null()).cloned()
}

#[cfg(test)]
mod tests;
