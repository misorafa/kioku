//! Memory carried into the next session (SPEC-M3.0 §1–§2): the items of earlier agent
//! handoffs (decisions, verified facts, open questions, gotchas) read back from their
//! Markdown, de-duplicated by normalised text, open questions dropped once a later
//! decision resolves them, open questions and gotchas dropped once a later handoff lists
//! them as `resolved`, and open questions older than [`OPEN_QUESTION_DAYS`] left out
//! (SPEC-M3.6 §2). Pure functions; the store supplies the handoffs.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use lindera_analysis::character_filter::CharacterFilter;
use lindera_analysis::character_filter::unicode_normalize::{
    UnicodeNormalizeCharacterFilter, UnicodeNormalizeKind,
};

use crate::handoff::Handoff;
use crate::session::CarriedItem;
use crate::strings::{EN, JA};
use crate::util::{display_date, one_line, parse_ts};

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
/// Normalised leading chars of a question that a resolving decision (or `resolved` item)
/// must start with; also the minimum length of a `resolved` item that closes every item
/// starting with it (SPEC-M3.6 §2).
pub const RESOLVE_PREFIX: usize = 12;
/// Open questions of a handoff older than this many days are not carried (SPEC-M3.6 §2).
pub const OPEN_QUESTION_DAYS: i64 = 30;
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
    /// `解決済み` / `Resolved` (SPEC-M3.6 §2): carried items that no longer hold.
    pub resolved: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Decisions,
    Open,
    Verified,
    Gotchas,
    Resolved,
}

/// Reads the items back from a rendered agent handoff ([`crate::handoff::render_agent_handoff`],
/// either language, any heading depth): `- ` lines under the five list headings; the
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
        (title(JA.handoff_resolved), Section::Resolved),
        (title(EN.handoff_resolved), Section::Resolved),
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
            Section::Resolved => &mut items.resolved,
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
    /// RFC 3339 creation time (the age of its open questions, SPEC-M3.6 §2); an
    /// unparsable value counts as new.
    pub created_at: String,
    /// Its items.
    pub items: HandoffItems,
}

impl SourceHandoff {
    /// Parses a stored handoff.
    pub fn from_handoff(h: &Handoff) -> SourceHandoff {
        SourceHandoff {
            id: h.id.clone(),
            date: display_date(&h.created_at),
            created_at: h.created_at.clone(),
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
    /// Open questions not resolved by a later decision or `resolved` item and not older
    /// than [`OPEN_QUESTION_DAYS`], newest first.
    pub open_questions: Vec<CarriedItem>,
    /// Gotchas not listed as `resolved` by a later handoff, newest first.
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

/// True when the `resolved` item `r` closes `item` (both [`normalize`]d, SPEC-M3.6 §2):
/// equal, `r` starts with the item's first [`RESOLVE_PREFIX`] chars, or the item starts
/// with `r` and `r` has at least [`RESOLVE_PREFIX`] chars.
pub fn resolves(r: &str, item: &str) -> bool {
    if r.is_empty() {
        return false;
    }
    let prefix: String = item.chars().take(RESOLVE_PREFIX).collect();
    r == item
        || r.starts_with(&prefix)
        || (item.starts_with(r) && r.chars().count() >= RESOLVE_PREFIX)
}

/// Collects the carried items of `handoffs` (newest first): each list de-duplicated by
/// [`normalize`]d text — a duplicate keeps the place and date of its newest occurrence but
/// the wording of its earliest one, so a decision restated with a different width or case
/// does not change how it reads — items already shown in full in the
/// block's handoff section (`shown`) left out, an open question dropped when a handoff
/// newer than it — or `shown` — has a decision equal to it or starting with its first
/// [`RESOLVE_PREFIX`] normalised chars, an open question or gotcha dropped when such a
/// handoff lists it as `resolved` ([`resolves`]), and open questions of handoffs older than
/// [`OPEN_QUESTION_DAYS`] at `now` not carried (SPEC-M3.6 §2).
pub fn carry(handoffs: &[SourceHandoff], shown: &HandoffItems, now: DateTime<Utc>) -> Carried {
    let norms = |v: &[String]| v.iter().map(|t| normalize(t)).collect::<Vec<String>>();
    // closers[i]: the `resolved` items of `shown` and of every handoff newer than handoffs[i]
    let mut closers: Vec<Vec<String>> = Vec::with_capacity(handoffs.len());
    let mut acc = norms(&shown.resolved);
    for h in handoffs {
        closers.push(acc.clone());
        acc.extend(norms(&h.items.resolved));
    }
    let closed = |i: usize, n: &str| closers[i].iter().any(|r| resolves(r, n));
    let pick = |list: fn(&HandoffItems) -> &Vec<String>, max: usize, resolvable: bool| {
        let mut seen: HashSet<String> = norms(list(shown)).into_iter().collect();
        let mut out: Vec<CarriedItem> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for (i, h) in handoffs.iter().enumerate() {
            for text in list(&h.items) {
                let n = normalize(text);
                if let Some(&k) = at.get(&n) {
                    out[k].text = text.clone();
                } else if out.len() < max
                    && !n.is_empty()
                    && seen.insert(n.clone())
                    && !(resolvable && closed(i, &n))
                {
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
    let cutoff = now - chrono::Duration::days(OPEN_QUESTION_DAYS);
    let shown_decisions = norms(&shown.decisions);
    let mut open: Vec<CarriedItem> = Vec::new();
    let mut open_at: HashMap<String, usize> = HashMap::new();
    let mut seen: HashSet<String> = norms(&shown.open_questions).into_iter().collect();
    for (i, h) in handoffs.iter().enumerate() {
        let too_old = parse_ts(&h.created_at).is_some_and(|t| t < cutoff);
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
            let by_decision = |d: &String| *d == n || d.starts_with(&prefix);
            let resolved = shown_decisions.iter().any(by_decision)
                || handoffs[..i]
                    .iter()
                    .flat_map(|newer| &newer.items.decisions)
                    .map(|d| normalize(d))
                    .any(|d| by_decision(&d))
                || closed(i, &n);
            if !resolved && !too_old {
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
        decisions: pick(|i| &i.decisions, MAX_DECISIONS, false),
        verified: pick(|i| &i.verified, MAX_VERIFIED, false),
        open_questions: open,
        gotchas: pick(|i| &i.gotchas, MAX_GOTCHAS, true),
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
            created_at: format!("{date}T09:00:00Z"),
            items: handoff_items(&render_agent_handoff(lang, "codex", date, &input)),
        }
    }

    /// "Now" of the tests: a week after the newest fixture handoff.
    fn now() -> DateTime<Utc> {
        parse_ts("2026-10-10T00:00:00Z").unwrap()
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
            resolved: vec!["v1.0.0 のタグはいつ打つか".into()],
            ..HandoffInput::default()
        };
        for lang in [Lang::Ja, Lang::En] {
            let items = handoff_items(&render_agent_handoff(lang, "codex", "d", &input));
            assert_eq!(items.decisions, ["lindera を使う", "複数行の 決定"]);
            assert_eq!(items.open_questions, ["再ランキングは？"]);
            assert_eq!(items.verified, ["cargo test は通る"]);
            assert_eq!(items.gotchas, ["Windows ではパス区切りが違う"]);
            assert_eq!(items.resolved, ["v1.0.0 のタグはいつ打つか"]);
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
        let c = carry(&all, &HandoffItems::default(), now());
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
        let c = carry(&all, &HandoffItems::default(), now());
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
        let c = carry(&all, &shown, now());
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
        let c = carry(&[h], &HandoffItems::default(), now());
        assert_eq!(c.decisions.len(), MAX_DECISIONS);
        assert_eq!(c.open_questions.len(), MAX_OPEN_QUESTIONS);
        assert_eq!(c.verified.len(), MAX_VERIFIED);
        assert_eq!(c.gotchas.len(), MAX_GOTCHAS);
    }

    fn resolving(id: &str, date: &str, resolved: &[&str]) -> SourceHandoff {
        source(
            id,
            date,
            HandoffInput {
                resolved: resolved.iter().map(|r| r.to_string()).collect(),
                ..HandoffInput::default()
            },
            Lang::Ja,
        )
    }

    /// SPEC-M3.6 §2 rule 1: a newer handoff's `resolved` closes a carried open question and
    /// a carried gotcha; resolved items are not carried themselves; an older `resolved` does
    /// not close a newer item.
    #[test]
    fn resolved_items_close_questions_and_gotchas() {
        let older = source(
            "h1",
            "2026-10-07",
            HandoffInput {
                open_questions: vec![
                    "v1.0.0 のタグはいつ打つか".into(),
                    "リリース日はいつにするか".into(),
                ],
                gotchas: vec![
                    "Windows の CI は 40 分かかる".into(),
                    "tantivy の writer は一つだけ".into(),
                ],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let newer = resolving(
            "h2",
            "2026-10-08",
            &["v1.0.0 のタグはいつ打つか", "Windows の CI は 40 分かかる"],
        );
        let c = carry(
            &[newer.clone(), older.clone()],
            &HandoffItems::default(),
            now(),
        );
        assert_eq!(texts(&c.open_questions), ["リリース日はいつにするか"]);
        assert_eq!(texts(&c.gotchas), ["tantivy の writer は一つだけ"]);
        let all: Vec<&str> = [&c.decisions, &c.verified, &c.open_questions, &c.gotchas]
            .iter()
            .flat_map(|l| texts(l))
            .collect();
        assert!(!all.contains(&"v1.0.0 のタグはいつ打つか"), "{all:?}");

        // the shown handoff's `resolved` closes too
        let shown = HandoffItems {
            resolved: vec!["リリース日はいつにするか".into()],
            ..HandoffItems::default()
        };
        let c = carry(&[newer.clone(), older.clone()], &shown, now());
        assert!(c.open_questions.is_empty(), "{:?}", c.open_questions);

        // an older `resolved` does not close a newer question (asked again later)
        let c = carry(&[older, newer], &HandoffItems::default(), now());
        assert_eq!(
            texts(&c.open_questions),
            ["v1.0.0 のタグはいつ打つか", "リリース日はいつにするか"]
        );
        assert_eq!(c.gotchas.len(), 2);
    }

    /// SPEC-M3.6 §2 rule 1: the three match forms, on normalised text.
    #[test]
    fn resolved_match_forms() {
        let q = "検索の再ランキングはどうするか決める";
        // equal (width and case folded)
        assert!(resolves(
            &normalize("検索の再ランキングはどうするか決める"),
            &normalize(q)
        ));
        assert!(resolves(
            &normalize("ＣＩ は遅い？"),
            &normalize("ci は遅い？")
        ));
        // r starts with the item's first 12 chars
        assert!(resolves(
            &normalize("検索の再ランキングはどう → M3.1 で決着"),
            &normalize(q)
        ));
        // the item starts with r, r ≥ 12 chars
        assert!(resolves(
            &normalize("検索の再ランキングはどうす"),
            &normalize(q)
        ));
        // ... but not when r is shorter than 12 chars
        assert!(!resolves(&normalize("検索の再ランキング"), &normalize(q)));
        assert!(!resolves(&normalize("別の話"), &normalize(q)));
        assert!(!resolves("", &normalize(q)));

        let h = source(
            "h1",
            "2026-10-07",
            HandoffInput {
                open_questions: vec![q.into(), "machine 名はどこから取る？".into()],
                gotchas: vec!["WAL ファイルが肥大化することがあるので注意".into()],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let newer = resolving(
            "h2",
            "2026-10-08",
            &[
                "検索の再ランキングはどうす",
                "ＷＡＬ ファイルが肥大化することが",
            ],
        );
        let c = carry(&[newer, h], &HandoffItems::default(), now());
        assert_eq!(texts(&c.open_questions), ["machine 名はどこから取る？"]);
        assert!(c.gotchas.is_empty(), "{:?}", c.gotchas);
    }

    /// SPEC-M3.6 §2 rule 2: open questions older than 30 days are not carried; decisions,
    /// verified facts and gotchas have no age limit.
    #[test]
    fn old_open_questions_expire_but_decisions_do_not() {
        let old = source(
            "h-old",
            "2026-09-08",
            HandoffInput {
                open_questions: vec!["リリース日はいつにするか".into()],
                decisions: vec!["lindera を使う".into()],
                verified: vec!["cargo test は通る".into()],
                gotchas: vec!["WAL に注意".into()],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        let recent = source(
            "h-new",
            "2026-10-01",
            HandoffInput {
                open_questions: vec!["machine 名はどこから取る？".into()],
                ..HandoffInput::default()
            },
            Lang::Ja,
        );
        // 2026-09-08 09:00 is 31 days and 15 hours before 2026-10-10 00:00
        let all = [recent, old];
        let c = carry(&all, &HandoffItems::default(), now());
        assert_eq!(texts(&c.open_questions), ["machine 名はどこから取る？"]);
        assert_eq!(texts(&c.decisions), ["lindera を使う"]);
        assert_eq!(texts(&c.verified), ["cargo test は通る"]);
        assert_eq!(texts(&c.gotchas), ["WAL に注意"]);
        // two days earlier (29 days and 15 hours) the old question was still carried
        let earlier = parse_ts("2026-10-08T00:00:00Z").unwrap();
        let c = carry(&all, &HandoffItems::default(), earlier);
        assert_eq!(c.open_questions.len(), 2);
    }
}
