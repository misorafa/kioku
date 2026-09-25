//! Session and observation types (spec §5, §7.1) plus searchable-text extraction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handoff::Handoff;
use crate::project::ProjectIdentity;
use crate::util::truncate_chars;

/// What an observation records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// A user prompt (UserPromptSubmit).
    Prompt,
    /// A tool call and its result (PostToolUse).
    ToolUse,
    /// The agent stopped (Stop).
    Stop,
    /// Context compaction (PreCompact).
    Compact,
    /// Free-form note.
    Note,
}

impl ObservationKind {
    /// snake_case name as stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationKind::Prompt => "prompt",
            ObservationKind::ToolUse => "tool_use",
            ObservationKind::Stop => "stop",
            ObservationKind::Compact => "compact",
            ObservationKind::Note => "note",
        }
    }

    /// Parses the snake_case name.
    pub fn parse(s: &str) -> Option<ObservationKind> {
        match s {
            "prompt" => Some(ObservationKind::Prompt),
            "tool_use" => Some(ObservationKind::ToolUse),
            "stop" => Some(ObservationKind::Stop),
            "compact" => Some(ObservationKind::Compact),
            "note" => Some(ObservationKind::Note),
            _ => None,
        }
    }
}

/// Input of `Store::add_observation` (`POST /api/v1/observations`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewObservation {
    /// Agent session id.
    pub session_id: String,
    /// Observation kind.
    pub kind: ObservationKind,
    /// RFC 3339 time of the event; server time when absent or malformed.
    #[serde(default)]
    pub ts: Option<String>,
    /// Neutral event payload (`prompt`, `tool_name`, `tool_input`, `tool_response`, …).
    #[serde(default)]
    pub payload: Value,
}

/// A stored observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Row id.
    pub id: i64,
    /// Agent session id.
    pub session_id: String,
    /// Project id.
    pub project_id: String,
    /// 1-based order within the session.
    pub seq: i64,
    /// Observation kind.
    pub kind: ObservationKind,
    /// RFC 3339 timestamp.
    pub ts: String,
    /// Sanitized payload.
    pub payload: Value,
    /// Sanitized, searchable text.
    pub text: String,
}

/// Lifecycle status of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Still receiving observations.
    Open,
    /// Summarized; a new observation reopens it.
    Finalized,
}

impl SessionStatus {
    /// Lowercase name as stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Open => "open",
            SessionStatus::Finalized => "finalized",
        }
    }
}

/// A session row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Agent session id (verbatim).
    pub id: String,
    /// Project id.
    pub project_id: String,
    /// Agent name, e.g. `claude-code`.
    pub agent: String,
    /// Working directory at start.
    pub cwd: String,
    /// Start source (`startup` | `resume` | `clear` | `compact`).
    pub source: String,
    /// RFC 3339 start time.
    pub started_at: String,
    /// RFC 3339 finalize time.
    pub ended_at: Option<String>,
    /// Lifecycle status.
    pub status: SessionStatus,
    /// Project root on the machine that ran this session (digest paths are relative to it).
    #[serde(default)]
    pub root_path: Option<String>,
}

/// Input of `Store::start_session` (`POST /api/v1/sessions/start`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionStartRequest {
    /// Agent session id.
    pub session_id: String,
    /// Agent name.
    #[serde(default = "default_agent")]
    pub agent: String,
    /// Working directory.
    #[serde(default)]
    pub cwd: String,
    /// Start source.
    #[serde(default)]
    pub source: String,
    /// Project identity computed client-side.
    pub project: ProjectIdentity,
}

fn default_agent() -> String {
    "claude-code".to_string()
}

/// A recent session page, for SessionStart context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentSession {
    /// Session page title.
    pub title: String,
    /// Wiki-relative path of the session page.
    pub path: String,
    /// `YYYY-MM-DD`.
    pub date: String,
}

/// Output of `Store::start_session`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionStartResponse {
    /// Project id the session was attached to.
    pub project_id: String,
    /// The handoff consumed by this session, if one was pending.
    pub pending_handoff: Option<Handoff>,
    /// First 60 lines of STATE.md (body only), if it exists.
    pub state_excerpt: Option<String>,
    /// Latest session pages of the project, newest first.
    pub recent_sessions: Vec<RecentSession>,
}

/// Prompt / tool-use counts of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCounts {
    /// Number of `prompt` observations.
    pub prompts: u32,
    /// Number of `tool_use` observations.
    pub tool_uses: u32,
}

/// Output of `Store::session_info` (`GET /api/v1/sessions/{id}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Project id.
    pub project_id: String,
    /// Lifecycle status.
    pub status: SessionStatus,
    /// Observation counts.
    pub counts: SessionCounts,
    /// Whether the agent wrote a handoff in this session.
    pub has_agent_handoff: bool,
    /// Tool uses after the session's latest agent handoff (all of them when there is none).
    /// `None` only when talking to a server that predates this field.
    #[serde(default)]
    pub tool_uses_since_handoff: Option<u32>,
}

impl SessionInfo {
    /// [`SessionInfo::tool_uses_since_handoff`], derived from the other fields for old servers.
    pub fn tool_uses_since_handoff(&self) -> u32 {
        self.tool_uses_since_handoff
            .unwrap_or(if self.has_agent_handoff {
                0
            } else {
                self.counts.tool_uses
            })
    }
}

/// Tool uses since the last agent handoff at which the Stop hook nudges for a (new) handoff
/// and finalize appends an auto-generated addendum to an existing agent handoff (spec §7.1).
pub const HANDOFF_STALE_TOOL_USES: u32 = 3;

/// Output of `Store::finalize_session`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeResult {
    /// False when the session had no prompt and no tool use (nothing was written).
    pub substantive: bool,
    /// Wiki-relative path of the session page.
    pub session_page: Option<String>,
    /// Id of the session's newest handoff (agent-written or rule-generated).
    pub handoff_id: Option<String>,
}

/// True when a session id is safe to use in file names (`[A-Za-z0-9._-]`, no leading dot).
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Searchable text for an observation (stored in `observations.text`).
pub fn observation_text(kind: ObservationKind, payload: &Value) -> String {
    let s = |v: &Value| v.as_str().map(str::to_string);
    match kind {
        ObservationKind::Prompt => payload
            .get("prompt")
            .and_then(s)
            .or_else(|| s(payload))
            .unwrap_or_default(),
        ObservationKind::ToolUse => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
            let detail = [
                "file_path",
                "notebook_path",
                "command",
                "pattern",
                "url",
                "query",
            ]
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| match &input {
                Value::Null => String::new(),
                Value::String(x) => x.clone(),
                other => other.to_string(),
            });
            truncate_chars(&format!("{tool}: {detail}"), 1000)
        }
        ObservationKind::Note => payload
            .get("text")
            .and_then(s)
            .or_else(|| s(payload))
            .unwrap_or_else(|| payload.to_string()),
        ObservationKind::Stop | ObservationKind::Compact => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_extraction() {
        let p = json!({"prompt": "引き継ぎを自動化して"});
        assert_eq!(
            observation_text(ObservationKind::Prompt, &p),
            "引き継ぎを自動化して"
        );
        let t = json!({"tool_name": "Bash", "tool_input": {"command": "cargo test"}});
        assert_eq!(
            observation_text(ObservationKind::ToolUse, &t),
            "Bash: cargo test"
        );
        assert_eq!(observation_text(ObservationKind::Stop, &json!({})), "");
    }

    #[test]
    fn session_id_validation() {
        assert!(is_valid_session_id("0c2f1a2b-3c4d-5e6f-7a8b-9c0d1e2f3a4b"));
        assert!(!is_valid_session_id("../etc"));
        assert!(!is_valid_session_id("a/b"));
        assert!(!is_valid_session_id(""));
    }
}
