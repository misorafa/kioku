//! The plain-text `<kioku>` block printed by the SessionStart hook (spec §8.3), capped at
//! 6 000 chars by shrinking the STATE.md excerpt first, then the handoff.

use kioku_core::strings::{Lang, fill, strings};
use kioku_core::util::truncate_chars;

/// Maximum size of the SessionStart block, in chars.
pub const SESSION_START_CAP: usize = 6000;
/// A section is dropped instead of shrunk below this many chars.
const MIN_SECTION: usize = 40;
const CLOSE: &str = "</kioku>\n";

/// Inputs of the SessionStart block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartContext {
    /// Project display name.
    pub project_name: String,
    /// Project id (what the agent must pass as `project` to kioku_* tools).
    pub project_id: String,
    /// Server URL shown to the agent.
    pub server_url: String,
    /// Markdown of the handoff consumed by this session, if any.
    pub handoff: Option<String>,
    /// STATE.md excerpt (first 60 lines), if any.
    pub state: Option<String>,
}

/// Renders the `<kioku>` block, at most [`SESSION_START_CAP`] chars.
pub fn render_session_start(lang: Lang, ctx: &StartContext) -> String {
    render_with_cap(lang, ctx, SESSION_START_CAP)
}

/// [`render_session_start`] with an explicit cap (tests).
pub fn render_with_cap(lang: Lang, ctx: &StartContext, cap: usize) -> String {
    let clean = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut handoff = clean(&ctx.handoff);
    let mut state = clean(&ctx.state);

    let mut out = assemble(lang, ctx, handoff.as_deref(), state.as_deref());
    if len(&out) <= cap {
        return out;
    }
    // Shrink the STATE excerpt first, then the handoff.
    if let Some(s) = state.take() {
        let over = len(&out) - cap;
        state = shrink(&s, len(&s).saturating_sub(over));
        out = assemble(lang, ctx, handoff.as_deref(), state.as_deref());
        if len(&out) <= cap {
            return out;
        }
    }
    if let Some(h) = handoff.take() {
        let over = len(&out) - cap;
        handoff = shrink(&h, len(&h).saturating_sub(over));
        out = assemble(lang, ctx, handoff.as_deref(), state.as_deref());
        if len(&out) <= cap {
            return out;
        }
    }
    // Only the fixed parts are left and they are still too long (absurd project name).
    let body = truncate_chars(&out, cap.saturating_sub(len(CLOSE) + 1));
    format!("{body}\n{CLOSE}")
}

fn assemble(lang: Lang, ctx: &StartContext, handoff: Option<&str>, state: Option<&str>) -> String {
    let t = strings(lang);
    let mut out = String::from("<kioku>\n");
    out.push_str(&fill(
        t.start_project_line,
        &[("name", &ctx.project_name), ("id", &ctx.project_id)],
    ));
    out.push('\n');
    out.push_str(&format!("server: {}\n", ctx.server_url));
    if let Some(h) = handoff {
        out.push_str(&format!("\n{}\n{h}\n", t.start_handoff_heading));
    }
    if let Some(s) = state {
        out.push_str(&format!("\n{}\n{s}\n", t.start_state_heading));
    }
    out.push_str(&format!("\n{}\n{CLOSE}", t.start_footer));
    out
}

/// Keeps whole lines of `text` within `budget` chars (plus a `…` marker); `None` when the
/// budget is too small to be useful.
fn shrink(text: &str, budget: usize) -> Option<String> {
    if budget < MIN_SECTION {
        return None;
    }
    let limit = budget - 2; // room for "\n…"
    let mut out = String::new();
    for line in text.lines() {
        let extra = len(line) + usize::from(!out.is_empty());
        if len(&out) + extra > limit {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.is_empty() {
        return Some(truncate_chars(text.lines().next().unwrap_or(""), budget));
    }
    out.push_str("\n…");
    Some(out)
}

fn len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(handoff: Option<String>, state: Option<String>) -> StartContext {
        StartContext {
            project_name: "kioku".into(),
            project_id: "kioku-3f9a1c2e".into(),
            server_url: "http://127.0.0.1:7391".into(),
            handoff,
            state,
        }
    }

    #[test]
    fn full_block_layout() {
        let out = render_session_start(
            Lang::Ja,
            &ctx(
                Some("## 引き継ぎ（claude-code, 2026-09-25）\n### 要約\nMCP を実装した".into()),
                Some("## 最新の引き継ぎ\n…".into()),
            ),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "<kioku>");
        assert!(lines[1].starts_with("project: kioku (id: kioku-3f9a1c2e)"));
        assert_eq!(lines[2], "server: http://127.0.0.1:7391");
        assert!(out.contains("## 前回からの引き継ぎ\n## 引き継ぎ（claude-code"));
        assert!(out.contains("## 現在の状態（STATE.md 抜粋）"));
        assert!(out.contains("kioku_handoff_write"));
        assert!(out.ends_with("</kioku>\n"));
    }

    #[test]
    fn optional_sections_omitted_and_english() {
        let out = render_session_start(Lang::En, &ctx(None, Some("  ".into())));
        assert!(!out.contains("## Handoff from"));
        assert!(!out.contains("## Current state"));
        assert!(out.contains("pass this id as `project`"));
    }

    #[test]
    fn cap_truncates_state_first() {
        let handoff = "要約: 引き継ぎを自動化した".to_string();
        let state = (0..400)
            .map(|i| format!("- セッション {i} で触ったファイル src/lib.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = render_session_start(Lang::Ja, &ctx(Some(handoff.clone()), Some(state)));
        assert!(out.chars().count() <= SESSION_START_CAP, "{}", out.len());
        assert!(out.contains(&handoff), "handoff must survive intact");
        assert!(out.contains("- セッション 0 で"));
        assert!(out.contains("\n…\n"));
        assert!(out.starts_with("<kioku>\nproject: kioku (id: kioku-3f9a1c2e)"));
        assert!(out.ends_with("</kioku>\n"));
    }

    #[test]
    fn cap_then_truncates_handoff() {
        let handoff = "次にやること: 検索を改善する\n".repeat(600);
        let state = "x".repeat(3000);
        let out = render_session_start(Lang::Ja, &ctx(Some(handoff), Some(state)));
        assert!(out.chars().count() <= SESSION_START_CAP);
        assert!(!out.contains("## 現在の状態"), "state dropped first");
        assert!(out.contains("## 前回からの引き継ぎ\n次にやること"));
        assert!(out.ends_with("</kioku>\n"));

        // A single enormous line is char-truncated rather than dropped.
        let out = render_with_cap(Lang::Ja, &ctx(Some("あ".repeat(10_000)), None), 1000);
        assert!(out.chars().count() <= 1000);
        assert!(out.contains("あああ"));
    }

    #[test]
    fn cap_holds_for_absurd_fixed_parts() {
        let mut c = ctx(None, None);
        c.project_name = "n".repeat(10_000);
        let out = render_session_start(Lang::En, &c);
        assert!(out.chars().count() <= SESSION_START_CAP);
        assert!(out.ends_with("</kioku>\n"));
    }
}
