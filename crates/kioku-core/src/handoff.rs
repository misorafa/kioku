//! Handoff types and rendering of agent-written handoffs (spec §7.1, §7.5).

use serde::{Deserialize, Serialize};

use crate::strings::{Lang, fill, strings};
use crate::util::one_line;

/// Who wrote a handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HandoffSource {
    /// The agent, via `kioku_handoff_write`.
    Agent,
    /// Rule-based digest at finalize.
    Rules,
}

impl HandoffSource {
    /// Lowercase name as stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffSource::Agent => "agent",
            HandoffSource::Rules => "rules",
        }
    }

    /// Parses `agent` / `rules`.
    pub fn parse(s: &str) -> Option<HandoffSource> {
        match s {
            "agent" => Some(HandoffSource::Agent),
            "rules" => Some(HandoffSource::Rules),
            _ => None,
        }
    }
}

/// A stored handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// ULID-like id.
    pub id: String,
    /// Project id.
    pub project_id: String,
    /// Session that produced it, if known.
    pub session_id: Option<String>,
    /// Agent or rules.
    pub source: HandoffSource,
    /// Rendered Markdown.
    pub content_md: String,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time it was consumed (or superseded).
    pub accepted_at: Option<String>,
    /// Session (or caller) that consumed it.
    pub accepted_by: Option<String>,
    /// Agent of the producing session, if known.
    pub agent: Option<String>,
    /// RFC 3339 time of the last in-place refresh (rules handoffs), if any.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Lane it lives on (the producing session's branch, M2.4 §1.3); `None` = project lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
}

/// Result of a pending-handoff lookup routed by lane (M2.4 §1.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingHandoff {
    /// The newest pending handoff on the requested lane (accepted when asked to).
    pub handoff: Option<Handoff>,
    /// Only on a branch lane without its own handoff: the project lane's pending handoff,
    /// for reference, never accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_handoff: Option<Handoff>,
    /// Why `reference_handoff` is only a reference (SPEC-M3.1 §1): [`REFERENCE_MAIN_LINE`],
    /// [`REFERENCE_CONCURRENT`] or [`REFERENCE_RESUMED`]; absent = main line (older server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_reason: Option<String>,
    /// The last handoffs of the lane, newest first, when asked for (`history`, ≤ 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Handoff>,
}

/// `reference_reason`: the main line's handoff on a branch lane without its own (M2.4 §1.4).
pub const REFERENCE_MAIN_LINE: &str = "main_line";
/// `reference_reason`: another session is active on the lane (SPEC-M3.1 §1 rule 3).
pub const REFERENCE_CONCURRENT: &str = "concurrent";
/// `reference_reason`: a resumed / compacted session that never accepted one (§1 rule 1).
pub const REFERENCE_RESUMED: &str = "resumed";
/// `accepted_by` of a handoff that a newer one replaced when it was accepted (§1).
pub const SUPERSEDED: &str = "superseded";
/// Max `history` of `kioku_handoff_pending`.
pub const MAX_HISTORY: usize = 20;

impl Handoff {
    /// `pending`, `superseded` or `accepted by <session>`.
    pub fn status(&self) -> String {
        match (&self.accepted_at, self.accepted_by.as_deref()) {
            (None, _) => "pending".to_string(),
            (Some(_), Some(SUPERSEDED)) => SUPERSEDED.to_string(),
            (Some(_), Some(by)) => format!("accepted by {by}"),
            (Some(_), None) => "accepted".to_string(),
        }
    }
}

/// Input of `kioku_handoff_write` / `POST /api/v1/handoffs` (spec §7.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffInput {
    /// Project id.
    pub project: String,
    /// Session id; when omitted, the open session of the project with the newest observation.
    #[serde(default)]
    pub session: Option<String>,
    /// What was done.
    pub summary: String,
    /// What to do next.
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// Unresolved questions.
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// Decisions taken.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// Pitfalls the next session should know about (SPEC-M3.0 §2); older clients omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gotchas: Vec<String>,
    /// Facts that were checked and hold (SPEC-M3.0 §2); older clients omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified: Vec<String>,
}

/// Renders an agent-written handoff as Markdown (spec §7.5). `agent` is the heading label
/// (`claude-code@mini` when the machine is known, SPEC-M3.0 §6). `verified` and `gotchas`
/// (SPEC-M3.0 §2) get their sections only when non-empty, so a handoff without them renders
/// exactly as before. A multi-line item is written on one line, so every item stays one
/// `- ` line that [`crate::carry::handoff_items`] can read back.
pub fn render_agent_handoff(lang: Lang, agent: &str, date: &str, input: &HandoffInput) -> String {
    let s = strings(lang);
    let mut out = vec![
        fill(s.agent_handoff_heading, &[("agent", agent), ("date", date)]),
        s.handoff_summary.to_string(),
        input.summary.trim().to_string(),
    ];
    let lists = [
        (s.handoff_next_steps, &input.next_steps, true),
        (s.handoff_open_questions, &input.open_questions, true),
        (s.handoff_decisions, &input.decisions, true),
        (s.handoff_verified, &input.verified, false),
        (s.handoff_gotchas, &input.gotchas, false),
    ];
    for (heading, items, always) in lists {
        let items: Vec<String> = items
            .iter()
            .map(|i| one_line(i))
            .filter(|i| !i.is_empty())
            .collect();
        if items.is_empty() && !always {
            continue;
        }
        out.push(heading.to_string());
        if items.is_empty() {
            out.push(format!("- {}", s.none));
        } else {
            out.extend(items.iter().map(|i| format!("- {i}")));
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_sections() {
        let input = HandoffInput {
            project: "p".into(),
            summary: "検索を実装した".into(),
            next_steps: vec!["テストを書く".into(), "".into()],
            open_questions: vec![],
            decisions: vec!["lindera を使う".into()],
            ..HandoffInput::default()
        };
        let md = render_agent_handoff(Lang::Ja, "claude-code", "2026-09-25", &input);
        assert_eq!(
            md,
            "## 引き継ぎ（claude-code, 2026-09-25）\n### 要約\n検索を実装した\n### 次にやること\n- テストを書く\n### 未解決の質問\n- （なし）\n### 決定事項\n- lindera を使う\n"
        );
    }

    /// SPEC-M3.0 §2: `verified` and `gotchas` render after the decisions (only when given),
    /// with the machine in the heading; an older client's JSON without them still parses.
    #[test]
    fn renders_verified_and_gotchas_and_parses_old_input() {
        let input = HandoffInput {
            project: "p".into(),
            summary: "検索を実装した".into(),
            decisions: vec!["lindera を使う".into()],
            verified: vec!["cargo test は全件通る".into()],
            gotchas: vec!["Windows では\nCRLF に注意".into(), " ".into()],
            ..HandoffInput::default()
        };
        let md = render_agent_handoff(Lang::Ja, "claude-code@mini", "2026-10-01 10:12", &input);
        assert!(md.starts_with("## 引き継ぎ（claude-code@mini, 2026-10-01 10:12）\n"));
        assert!(md.ends_with(
            "### 決定事項\n- lindera を使う\n### 確認済みの事実\n- cargo test は全件通る\n### 落とし穴・注意点\n- Windows では CRLF に注意\n"
        ));
        let en = render_agent_handoff(Lang::En, "codex", "d", &input);
        assert!(en.contains("### Verified\n- cargo test は全件通る\n### Gotchas\n- Windows"));

        let json = serde_json::to_value(&input).unwrap();
        let back: HandoffInput = serde_json::from_value(json).unwrap();
        assert_eq!(back, input);
        let old: HandoffInput =
            serde_json::from_str(r#"{"project":"p","summary":"s","decisions":["d"]}"#).unwrap();
        assert!(old.gotchas.is_empty() && old.verified.is_empty());
        // without them the output is exactly the M1 layout
        let plain = HandoffInput {
            verified: vec![],
            gotchas: vec![],
            ..input
        };
        assert!(!render_agent_handoff(Lang::Ja, "a", "d", &plain).contains("確認済み"));
    }
}
