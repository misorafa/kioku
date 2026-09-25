//! Agent-neutral hook events (spec §8.1) and the per-agent stdin parsers.
//!
//! M1 ships only the Claude Code parser; `--agent <name>` selects it so later agents
//! (Codex, Cursor, Gemini) can add their own without touching the handlers.

use anyhow::Context;
use clap::ValueEnum;
use serde_json::Value;

/// Which agent produced the hook payload on stdin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Agent {
    /// Anthropic Claude Code.
    ClaudeCode,
}

impl Agent {
    /// Agent name as stored on sessions (`claude-code`).
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
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

/// A hook payload in agent-neutral form.
#[derive(Clone, Debug, PartialEq)]
pub struct HookEvent {
    /// Agent name (`claude-code`).
    pub agent: String,
    /// Which event this is.
    pub event: HookEventKind,
    /// Agent session id.
    pub session_id: String,
    /// Working directory of the agent (may be empty if the agent did not send one).
    pub cwd: String,
    /// SessionStart source (`startup` | `resume` | `clear` | `compact`).
    pub source: Option<String>,
    /// UserPromptSubmit prompt text.
    pub prompt: Option<String>,
    /// PostToolUse tool name.
    pub tool_name: Option<String>,
    /// PostToolUse tool input.
    pub tool_input: Option<Value>,
    /// PostToolUse tool response.
    pub tool_response: Option<Value>,
    /// PostToolUse tool call id.
    pub tool_use_id: Option<String>,
    /// Stop: true when the agent is already continuing because of a Stop hook.
    pub stop_hook_active: bool,
    /// PreCompact trigger (`manual` | `auto`).
    pub trigger: Option<String>,
    /// SessionEnd reason.
    pub reason: Option<String>,
    /// The payload exactly as received.
    pub raw: Value,
}

/// Parses a hook payload from stdin for the given agent and event.
pub fn parse_event(agent: Agent, event: HookEventKind, stdin: &str) -> anyhow::Result<HookEvent> {
    let raw: Value = serde_json::from_str(stdin.trim()).context("hook stdin is not JSON")?;
    match agent {
        Agent::ClaudeCode => parse_claude_code(event, raw),
    }
}

/// Claude Code stdin → [`HookEvent`] (fields per spec §8.1). The event comes from the
/// command line; `hook_event_name` is informational only.
pub fn parse_claude_code(event: HookEventKind, raw: Value) -> anyhow::Result<HookEvent> {
    if !raw.is_object() {
        anyhow::bail!("hook stdin is not a JSON object");
    }
    let text = |k: &str| {
        raw.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let session_id = text("session_id").context("hook payload has no session_id")?;
    let value = |k: &str| raw.get(k).filter(|v| !v.is_null()).cloned();
    Ok(HookEvent {
        agent: Agent::ClaudeCode.as_str().to_string(),
        event,
        session_id,
        cwd: text("cwd").unwrap_or_default(),
        source: text("source"),
        // The prompt is kept verbatim even when empty-looking: an empty prompt is still a prompt.
        prompt: raw
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_name: text("tool_name"),
        tool_input: value("tool_input"),
        tool_response: value("tool_response"),
        tool_use_id: text("tool_use_id"),
        stop_hook_active: raw
            .get("stop_hook_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        trigger: text("trigger"),
        reason: text("reason"),
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "8d3c1f0e-5b7a-4c2d-9e1f-0a2b3c4d5e6f";

    fn fixture(kind: HookEventKind, file: &str) -> HookEvent {
        let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap();
        let ev = parse_event(Agent::ClaudeCode, kind, &text).unwrap();
        assert_eq!(ev.agent, "claude-code");
        assert_eq!(ev.event, kind);
        assert_eq!(ev.session_id, SID);
        assert_eq!(ev.cwd, "/home/u/kioku");
        assert_eq!(
            ev.raw["hook_event_name"].as_str(),
            Some(kind.claude_code_name())
        );
        ev
    }

    #[test]
    fn session_start_fixture() {
        let ev = fixture(HookEventKind::SessionStart, "session_start.json");
        assert_eq!(ev.source.as_deref(), Some("startup"));
        assert!(ev.prompt.is_none() && ev.tool_name.is_none());
    }

    #[test]
    fn user_prompt_submit_fixture() {
        let ev = fixture(HookEventKind::UserPromptSubmit, "user_prompt_submit.json");
        assert!(
            ev.prompt
                .as_deref()
                .unwrap()
                .starts_with("引き継ぎ書の自動生成")
        );
    }

    #[test]
    fn post_tool_use_fixture() {
        let ev = fixture(HookEventKind::PostToolUse, "post_tool_use.json");
        assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
        assert_eq!(
            ev.tool_input.as_ref().unwrap()["file_path"],
            "/home/u/kioku/crates/kioku-core/src/handoff.rs"
        );
        assert_eq!(ev.tool_response.as_ref().unwrap()["userModified"], false);
        assert_eq!(
            ev.tool_use_id.as_deref(),
            Some("toolu_01KxQ7mY3bT9pVwE2rN8sL4d")
        );
    }

    #[test]
    fn stop_fixture() {
        let ev = fixture(HookEventKind::Stop, "stop.json");
        assert!(!ev.stop_hook_active);
        let mut raw = ev.raw.clone();
        raw["stop_hook_active"] = Value::Bool(true);
        let ev = parse_claude_code(HookEventKind::Stop, raw).unwrap();
        assert!(ev.stop_hook_active);
    }

    #[test]
    fn pre_compact_fixture() {
        let ev = fixture(HookEventKind::PreCompact, "pre_compact.json");
        assert_eq!(ev.trigger.as_deref(), Some("auto"));
    }

    #[test]
    fn session_end_fixture() {
        let ev = fixture(HookEventKind::SessionEnd, "session_end.json");
        assert_eq!(ev.reason.as_deref(), Some("prompt_input_exit"));
    }

    #[test]
    fn rejects_garbage() {
        let k = HookEventKind::Stop;
        assert!(parse_event(Agent::ClaudeCode, k, "").is_err());
        assert!(parse_event(Agent::ClaudeCode, k, "[1]").is_err());
        assert!(parse_event(Agent::ClaudeCode, k, r#"{"cwd":"/x"}"#).is_err());
        let ev = parse_event(Agent::ClaudeCode, k, r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(ev.cwd, "");
        assert!(!ev.stop_hook_active);
    }

    #[test]
    fn cli_names_match_value_enum() {
        for kind in ALL_EVENTS {
            assert_eq!(
                HookEventKind::from_str(kind.cli_name(), false).unwrap(),
                kind
            );
        }
        assert_eq!(
            Agent::from_str("claude-code", false).unwrap(),
            Agent::ClaudeCode
        );
    }
}
