//! Handoff types and rendering of agent-written handoffs (spec §7.1, §7.5).

use serde::{Deserialize, Serialize};

use crate::strings::{Lang, fill, strings};

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
}

/// Input of `kioku_handoff_write` / `POST /api/v1/handoffs` (spec §7.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffInput {
    /// Project id.
    pub project: String,
    /// Session id; the newest open session of the project when omitted.
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
}

/// Renders an agent-written handoff as Markdown (spec §7.5).
pub fn render_agent_handoff(lang: Lang, agent: &str, date: &str, input: &HandoffInput) -> String {
    let s = strings(lang);
    let mut out = vec![
        fill(s.agent_handoff_heading, &[("agent", agent), ("date", date)]),
        s.handoff_summary.to_string(),
        input.summary.trim().to_string(),
    ];
    for (heading, items) in [
        (s.handoff_next_steps, &input.next_steps),
        (s.handoff_open_questions, &input.open_questions),
        (s.handoff_decisions, &input.decisions),
    ] {
        out.push(heading.to_string());
        let items: Vec<&String> = items.iter().filter(|i| !i.trim().is_empty()).collect();
        if items.is_empty() {
            out.push(format!("- {}", s.none));
        } else {
            out.extend(items.iter().map(|i| format!("- {}", i.trim())));
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
}
