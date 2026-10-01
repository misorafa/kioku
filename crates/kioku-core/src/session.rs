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
    /// The agent's final reply of a turn (`last_assistant_message` on Stop, SPEC-M3.0 §3):
    /// payload `{text}`, sanitized, at most [`ASSISTANT_MAX`] chars.
    Assistant,
}

/// Max chars of an `assistant` observation's text (SPEC-M3.0 §3).
pub const ASSISTANT_MAX: usize = 2000;

impl ObservationKind {
    /// snake_case name as stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationKind::Prompt => "prompt",
            ObservationKind::ToolUse => "tool_use",
            ObservationKind::Stop => "stop",
            ObservationKind::Compact => "compact",
            ObservationKind::Note => "note",
            ObservationKind::Assistant => "assistant",
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
            "assistant" => Some(ObservationKind::Assistant),
            _ => None,
        }
    }
}

/// Input of `Store::add_observation` (`POST /api/v1/observations`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewObservation {
    /// Stable delivery ID; absent for legacy append-only clients.
    #[serde(default)]
    pub event_id: Option<String>,
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
    /// Handoff lane (the git branch, M2.4 §1.1); `None` = the project lane.
    #[serde(default)]
    pub lane: Option<String>,
    /// Host name of the machine that ran it (SPEC-M3.0 §6); `None` from older clients.
    #[serde(default)]
    pub machine: Option<String>,
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
    /// Handoff lane captured by the client (M2.4 §1.1); absent from older clients and on the
    /// default branch = the project lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Host name of the client machine (≤ 64 chars, `KIOKU_MACHINE` overrides; SPEC-M3.0
    /// §6); absent from older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// Max chars of a machine name (SPEC-M3.0 §6).
pub const MACHINE_MAX: usize = 64;

/// A machine name as stored: control characters dropped, trimmed, at most [`MACHINE_MAX`]
/// chars; `None` when nothing is left.
pub fn normalize_machine(name: &str) -> Option<String> {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.chars().take(MACHINE_MAX).collect())
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
    /// Agent of the session (SPEC-M3.0 §1; empty from an older server).
    #[serde(default)]
    pub agent: String,
    /// Lane of the session, if not the project lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Machine that ran the session (SPEC-M3.0 §6), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// A line carried forward from earlier agent handoffs (decisions, verified facts, open
/// questions, gotchas; SPEC-M3.0 §1–§2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedItem {
    /// The item text as written.
    pub text: String,
    /// `YYYY-MM-DD` of the handoff it comes from.
    pub date: String,
    /// Id of that handoff.
    pub handoff_id: String,
}

/// A page tagged `pinned`, shown at session start (SPEC-M3.0 §1 section 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPage {
    /// Wiki-relative path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// The start of the body (at most 400 chars).
    pub excerpt: String,
}

/// [`SessionStartResponse::context_version`] of a server that computes the SPEC-M3.0
/// sections (carried items, pinned pages, last reply).
pub const CONTEXT_VERSION: u32 = 1;

/// Output of `Store::start_session`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStartResponse {
    /// Project id the session was attached to.
    pub project_id: String,
    /// The handoff consumed by this session, if one was pending.
    pub pending_handoff: Option<Handoff>,
    /// First 60 lines of STATE.md (body only), if it exists.
    pub state_excerpt: Option<String>,
    /// Latest session pages of the project, newest first.
    pub recent_sessions: Vec<RecentSession>,
    /// The session's lane (M2.4 §1.5); `None` = the project lane (or an older server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// On a branch lane without a handoff of its own: the project lane's pending handoff,
    /// shown for reference and NOT accepted (M2.4 §1.4 rule 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_handoff: Option<Handoff>,
    /// [`CONTEXT_VERSION`] when the fields below are computed; 0 (absent) from an older
    /// server, whose block is rendered as before (handoff + STATE excerpt).
    #[serde(default)]
    pub context_version: u32,
    /// Decisions of earlier agent handoffs, newest first (SPEC-M3.0 §1 section 3).
    #[serde(default)]
    pub decisions: Vec<CarriedItem>,
    /// Verified facts of earlier agent handoffs, shown after the decisions (§2).
    #[serde(default)]
    pub verified: Vec<CarriedItem>,
    /// Open questions of earlier agent handoffs not resolved since (§1 section 4).
    #[serde(default)]
    pub open_questions: Vec<CarriedItem>,
    /// Gotchas of earlier agent handoffs, shown after the open questions (§2).
    #[serde(default)]
    pub gotchas: Vec<CarriedItem>,
    /// Pages tagged `pinned` in the project or `_global`, newest first (§1 section 5).
    #[serde(default)]
    pub pinned: Vec<PinnedPage>,
    /// The last reply of the previous session on this lane (§1 section 7, ≤ 600 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply: Option<String>,
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
    /// Seconds since the session's latest agent handoff, or since the session started when
    /// it has none, by the server's clock (SPEC-M3.0 §4); `None` from an older server.
    #[serde(default)]
    pub secs_since_handoff: Option<u64>,
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
        ObservationKind::Assistant => payload
            .get("text")
            .and_then(s)
            .or_else(|| s(payload))
            .unwrap_or_default(),
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
