//! Memory carried into the next session (SPEC-M3.0 §1–§2): the items of earlier agent
//! handoffs (decisions, verified facts, open questions, gotchas) read back from their
//! Markdown, de-duplicated by normalised text, and open questions dropped once a later
//! decision resolves them. Pure functions; the store supplies the handoffs.

use std::collections::HashSet;

use lindera_analysis::character_filter::CharacterFilter;
use lindera_analysis::character_filter::unicode_normalize::{
    UnicodeNormalizeCharacterFilter, UnicodeNormalizeKind,
};

use crate::handoff::Handoff;
use crate::session::CarriedItem;
use crate::strings::{EN, JA};
use crate::util::{display_date, one_line};

/// Agent handoffs of a project whose items are carried (SPEC-M3.0 §1).
pub const CARRY_HANDOFFS: usize = 20;
/// Max carried decisions.
pub const MAX_DECISIONS: usize = 15;
/// Max carried verified facts (shown after the decisions).
pub const MAX_VERIFIED: usize = 5;
/// Max carried open questions.
pub const MAX_OPEN_QUESTIONS: usize = 8;
/// Max carried gotchas (shown after the open questions).
pub const MAX_GOTCHAS: usize = 5;
/// Normalised leading chars of a question that a resolving decision must start with.
pub const RESOLVE_PREFIX: usize = 12;
/// Tag that puts a page into the SessionStart block (SPEC-M3.0 §1 section 5).
pub const PINNED_TAG: &str = "pinned";
/// Max pinned pages shown.
pub const MAX_PINNED: usize = 3;
/// Chars of a pinned page's body shown.
pub const PINNED_EXCERPT: usize = 400;
/// Chars of the previous session's last reply shown at session start.
pub const LAST_REPLY_MAX: usize = 600;
/// Chars of the last reply in the auto-generated handoff (SPEC-M3.0 §3).
pub const HANDOFF_REPLY_MAX: usize = 400;

/// The list items of one handoff, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandoffItems {
    /// `決定事項` / `Decisions`.
    pub decisions: Vec<String>,
    /// `未解決の質問` / `Open questions`.
    pub open_questions: Vec<String>,
    /// `確認済みの事実` / `Verified`.
    pub verified: Vec<String>,
    /// `落とし穴・注意点` / `Gotchas`.
    pub gotchas: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Decisions,
    Open,
    Verified,
    Gotchas,
}

/// Reads the items back from a rendered agent handoff ([`crate::handoff::render_agent_handoff`],
/// either language, any heading depth): `- ` lines under the four headings; the
/// `（なし）` / `(none)` placeholder is skipped.
pub fn handoff_items(md: &str) -> HandoffItems {
    let title = |h: &str| h.trim_start_matches('#').trim().to_string();
    let headings = [
        (title(JA.handoff_decisions), Section::Decisions),
        (title(EN.handoff_decisions), Section::Decisions),
        (title(JA.handoff_open_questions), Section::Open),
        (title(EN.handoff_open_questions), Section::Open),
        (title(JA.handoff_verified), Section::Verified),
        (title(EN.handoff_verified), Section::Verified),
        (title(JA.handoff_gotchas), Section::Gotchas),
        (title(EN.handoff_gotchas), Section::Gotchas),
    ];
    let mut items = HandoffItems::default();
    let mut current = None;
    for line in md.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            let t = title(line);
            current = headings.iter().find(|(h, _)| *h == t).map(|(_, s)| *s);
            continue;
        }
        let Some(section) = current else { continue };
        let Some(item) = line.strip_prefix("- ").map(str::trim) else {
            continue;
        };
        if item.is_empty() || item == JA.none || item == EN.none {
            continue;
        }
        let list = match section {
            Section::Decisions => &mut items.decisions,
            Section::Open => &mut items.open_questions,
            Section::Verified => &mut items.verified,
            Section::Gotchas => &mut items.gotchas,
        };
        list.push(item.to_string());
    }
    items
}

/// Text as compared for de-duplication: NFKC, whitespace collapsed, trimmed, lowercased.
pub fn normalize(text: &str) -> String {
    let mut s = one_line(text);
    let nfkc = UnicodeNormalizeCharacterFilter::new(UnicodeNormalizeKind::NFKC);
    if nfkc.apply(&mut s).is_err() {
        s = one_line(text);
    }
    s.trim().to_lowercase()
}

/// One agent handoff as a source of carried items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHandoff {
    /// Handoff id.
    pub id: String,
    /// `YYYY-MM-DD` of its creation.
    pub date: String,
    /// Its items.
    pub items: HandoffItems,
}

impl SourceHandoff {
    /// Parses a stored handoff.
    pub fn from_handoff(h: &Handoff) -> SourceHandoff {
        SourceHandoff {
            id: h.id.clone(),
            date: display_date(&h.created_at),
            items: handoff_items(&h.content_md),
        }
    }
}

/// What is carried into the next session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Carried {
    /// Decisions, newest first.
    pub decisions: Vec<CarriedItem>,
    /// Verified facts, newest first.
    pub verified: Vec<CarriedItem>,
    /// Open questions not resolved by a later decision, newest first.
    pub open_questions: Vec<CarriedItem>,
    /// Gotchas, newest first.
    pub gotchas: Vec<CarriedItem>,
}

impl Carried {
    /// True when nothing is carried.
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
            && self.verified.is_empty()
            && self.open_questions.is_empty()
            && self.gotchas.is_empty()
    }
}

/// Collects the carried items of `handoffs` (newest first): each list de-duplicated by
/// [`normalize`]d text — a duplicate keeps the place and date of its newest occurrence but
/// the wording of its earliest one, so a decision restated with a different width or case
/// does not change how it reads — items already shown in full in the
/// block's handoff section (`shown`) left out, and an open question dropped when a handoff
/// newer than it — or `shown` — has a decision equal to it or starting with its first
/// [`RESOLVE_PREFIX`] normalised chars.
pub fn carry(handoffs: &[SourceHandoff], shown: &HandoffItems) -> Carried {
    let norms = |v: &[String]| v.iter().map(|t| normalize(t)).collect::<HashSet<String>>();
    let pick = |list: fn(&HandoffItems) -> &Vec<String>, max: usize| {
        let mut seen = norms(list(shown));
        let mut out: Vec<CarriedItem> = Vec::new();
        let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for h in handoffs {
            for text in list(&h.items) {
                let n = normalize(text);
                if let Some(&i) = at.get(&n) {
                    out[i].text = text.clone();
                } else if out.len() < max && !n.is_empty() && seen.insert(n.clone()) {
                    at.insert(n, out.len());
                    out.push(CarriedItem {
                        text: text.clone(),
                        date: h.date.clone(),
                        handoff_id: h.id.clone(),
                    });
                }
            }
        }
        out
    };
    let shown_decisions: Vec<String> = shown.decisions.iter().map(|d| normalize(d)).collect();
    let mut open: Vec<CarriedItem> = Vec::new();
    let mut open_at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut seen = norms(&shown.open_questions);
    for (i, h) in handoffs.iter().enumerate() {
        for q in &h.items.open_questions {
            let n = normalize(q);
            if let Some(&at) = open_at.get(&n) {
                open[at].text = q.clone();
                continue;
            }
            if open.len() >= MAX_OPEN_QUESTIONS || n.is_empty() || !seen.insert(n.clone()) {
                continue;
            }
            let prefix: String = n.chars().take(RESOLVE_PREFIX).collect();
            let resolves = |d: &String| *d == n || d.starts_with(&prefix);
            let resolved = shown_decisions.iter().any(resolves)
                || handoffs[..i]
                    .iter()
                    .flat_map(|newer| &newer.items.decisions)
                    .map(|d| normalize(d))
                    .any(|d| resolves(&d));
            if !resolved {
                open_at.insert(n, open.len());
                open.push(CarriedItem {
                    text: q.clone(),
                    date: h.date.clone(),
                    handoff_id: h.id.clone(),
                });
            }
        }
    }
    Carried {
        decisions: pick(|i| &i.decisions, MAX_DECISIONS),
        verified: pick(|i| &i.verified, MAX_VERIFIED),
        open_questions: open,
        gotchas: pick(|i| &i.gotchas, MAX_GOTCHAS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::{HandoffInput, render_agent_handoff};
    use crate::strings::Lang;

    fn source(id: &str, date: &str, input: HandoffInput, lang: Lang) -> SourceHandoff {
        SourceHandoff {
            id: id.into(),
            date: date.into(),
            items: handoff_items(&render_agent_handoff(lang, "codex", date, &input)),
        }
    }

    fn texts(items: &[CarriedItem]) -> Vec<&str> {
        items.iter().map(|i| i.text.as_str()).collect()
    }

    #[test]
    fn items_round_trip_through_markdown_in_both_languages() {
        let input = HandoffInput {
            project: "p".into(),
            summary: "検索を実装した".into(),
            next_steps: vec!["テスト".into()],
            open_questions: vec!["再ランキングは？".into()],
            decisions: vec!["lindera を使う".into(), "複数行の\n決定".into()],
            verified: vec!["cargo test は通る".into()],
            gotchas: vec!["Windows ではパス区切りが違う".into()],
            ..HandoffInput::default()
        };
        for lang in [Lang::Ja, Lang::En] {
            let items = handoff_items(&render_agent_handoff(lang, "codex", "d", &input));
            assert_eq!(items.decisions, ["lindera を使う", "複数行の 決定"]);
            assert_eq!(items.open_questions, ["再ランキングは？"]);
            assert_eq!(items.verified, ["cargo test は通る"]);
            assert_eq!(items.gotchas, ["Windows ではパス区切りが違う"]);
        }
        // placeholders and STATE.md's demoted headings
        let md = "### 引き継ぎ\n#### 決定事項\n- （なし）\n#### 未解決の質問\n- 残り\n## 他\n- x";
        let items = handoff_items(md);
        assert!(items.decisions.is_empty());
        assert_eq!(items.open_questions, ["残り"]);
    }

    #[test]
    fn normalize_folds_width_case_and_spaces() {
        assert_eq!(normalize("  Ｌｉｎｄｅｒａ を\n使う "), "lindera を 使う");
        assert_eq!(normalize("ｱﾌﾟﾘ"), normalize("アプリ"));
    }

    /// Review of M3.0: a restated item keeps the wording it was first written with, while
    /// its date (and place in the list) is the latest restatement's.
    #[test]
    fn duplicates_keep_the_earliest_wording_and_the_latest_date() {
        let mk = |id: &str, date: &str, decision: &str, question: &str, gotcha: &str| {
            source(
                id,
                date,
                HandoffInput {
                    decisions: vec![decision.into(), format!("{id} だけの決定")],
                    open_questions: vec![question.into()],
                    gotchas: vec![gotcha.into()],
                    ..HandoffInput::default()
                },
                Lang::Ja,
            )
        };
        let all = [
            mk(
                "h3",
                "2026-10-03",
                "ＫＩＯＫＵ は 日本語優先",
                "ＣＩ は遅い？",
                "ＷＡＬ に注意",
            ),
            mk(
                "h2",
                "2026-10-02",
                "Kioku は 日本語優先",
                "CI は遅い？",
                "wal に注意",
            ),
            mk(
                "h1",
                "2026-10-01",
                "kioku は 日本語優先",
                "ci は遅い？",
                "WAL に注意",
            ),
        ];
        let c = carry(&all, &HandoffItems::default());
        assert_eq!(
            texts(&c.decisions),
            [
                "kioku は 日本語優先",
                "h3 だけの決定",
                "h2 だけの決定",
                "h1 だけの決定"
            ]
        );
        assert_eq!(c.decisions[0].date, "2026-10-03");
        assert_eq!(c.decisions[0].handoff_id, "h3");
        assert_eq!(texts(&c.open_questions), ["ci は遅い？"]);
        assert_eq!(c.open_questions[0].date, "2026-10-03");
        assert_eq!(texts(&c.gotchas), ["WAL に注意"]);
        assert_eq!(c.gotchas[0].date, "2026-10-03");
    }

    #[test]
    fn carry_dedupes_excludes_shown_and_resolves_questions() {
        let newest = source(
            "h3",
            "2026-09-28",
            HandoffInput {
                decisions: vec![
                    "ＬＩＮＤＥＲＡ を使う".into(),
                    "検索の再ランキングはどうするか → M3.1 でやる".into(),
                ],
                open_questions: vec!["Windows の CI が遅い".into()],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let middle = source(
            "h2",
            "2026-09-27",
            HandoffInput {
                decisions: vec!["lindera を使う".into(), "SQLite は WAL".into()],
                open_questions: vec![
                    "検索の再ランキングはどうする？".into(),
                    "machine 名はどこから取る？".into(),
                ],
                gotchas: vec!["tantivy の writer は一つだけ".into()],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let oldest = source(
            "h1",
            "2026-09-26",
            HandoffInput {
                decisions: vec!["Use git CLI, not git2".into()],
                open_questions: vec!["Windows の CI が遅い".into()],
                verified: vec!["install.sh works on macOS".into()],
                ..HandoffInput::default()
            },
            Lang::En,
        );
        let all = [newest, middle, oldest];
        let c = carry(&all, &HandoffItems::default());
        // duplicate decision (width / case) kept once: newest place and date, earliest wording
        assert_eq!(
            texts(&c.decisions),
            [
                "lindera を使う",
                "検索の再ランキングはどうするか → M3.1 でやる",
                "SQLite は WAL",
                "Use git CLI, not git2"
            ]
        );
        assert_eq!(c.decisions[0].date, "2026-09-28");
        assert_eq!(c.decisions[0].handoff_id, "h3");
        // "検索の再ランキングはどうする？" is resolved by the newer decision starting with its
        // first 12 chars; the repeated CI question is kept once (newest)
        assert_eq!(
            texts(&c.open_questions),
            ["Windows の CI が遅い", "machine 名はどこから取る？"]
        );
        assert_eq!(c.open_questions[0].handoff_id, "h3");
        assert_eq!(texts(&c.verified), ["install.sh works on macOS"]);
        assert_eq!(texts(&c.gotchas), ["tantivy の writer は一つだけ"]);

        // items shown in full in section 2 are not repeated; its decisions resolve questions
        let shown = HandoffItems {
            decisions: vec!["SQLite は WAL".into(), "machine 名はどこから取る？".into()],
            open_questions: vec!["Windows の CI が遅い".into()],
            ..HandoffItems::default()
        };
        let c = carry(&all, &shown);
        assert!(!texts(&c.decisions).contains(&"SQLite は WAL"));
        assert!(c.open_questions.is_empty(), "{:?}", c.open_questions);
    }

    #[test]
    fn carry_caps_each_list() {
        let many = |prefix: &str, n: usize| (0..n).map(|i| format!("{prefix} {i}")).collect();
        let h = source(
            "h",
            "2026-09-28",
            HandoffInput {
                decisions: many("決定", 30),
                open_questions: many("質問", 30),
                verified: many("確認", 30),
                gotchas: many("注意", 30),
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let c = carry(&[h], &HandoffItems::default());
        assert_eq!(c.decisions.len(), MAX_DECISIONS);
        assert_eq!(c.open_questions.len(), MAX_OPEN_QUESTIONS);
        assert_eq!(c.verified.len(), MAX_VERIFIED);
        assert_eq!(c.gotchas.len(), MAX_GOTCHAS);
    }
}
