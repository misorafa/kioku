//! Markdown rendering of session pages (spec §7.3) and STATE.md (spec §7.4).

use crate::carry::Carried;
use crate::digest::{FileCount, SessionDigest, prompt_title_line};
use crate::handoff::Handoff;
use crate::session::{CarriedItem, PinnedPage, Session};
use crate::strings::{Lang, fill, strings};
use crate::util::{display_date, display_minute, one_line, truncate_chars};

/// Wiki-relative path of a session page:
/// `<project>/sessions/YYYY-MM-DD-<first 8 of id>-<first 12 hex of sha256(id)>.md`. The
/// readable prefix matches what users see in hook output; the hash keeps ids that share
/// their first 8 characters (Codex UUIDv7 ids started within ~65 s) apart (SPEC-M2.6 §1).
pub fn session_page_path(session: &Session) -> String {
    let short: String = session
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(8)
        .collect();
    let hash = crate::util::sha256_hex(&session.id);
    format!(
        "{}/sessions/{}-{}-{}.md",
        session.project_id,
        display_date(&session.started_at),
        short,
        &hash[..12]
    )
}

/// Session page title: `<YYYY-MM-DD HH:MM> <agent> — <first prompt truncated 60>`; a
/// multi-line prompt contributes its first non-empty line (SPEC-M2.8 §6).
pub fn session_title(lang: Lang, session: &Session, digest: &SessionDigest) -> String {
    let first = digest
        .prompts
        .first()
        .map(|p| prompt_title_line(p))
        .filter(|p| !p.is_empty())
        .map(|p| truncate_chars(&one_line(&p), 60))
        .unwrap_or_else(|| strings(lang).no_prompt.to_string());
    format!(
        "{} {} — {}",
        display_minute(&session.started_at),
        session.agent,
        first
    )
}

/// Session page body; `handoff_md` is the agent handoff or the auto-generated section.
pub fn session_body(
    lang: Lang,
    session: &Session,
    digest: &SessionDigest,
    handoff_md: &str,
) -> String {
    let s = strings(lang);
    let mut out: Vec<String> = Vec::new();
    out.push(s.session_overview.to_string());
    let end = session
        .ended_at
        .as_deref()
        .map(display_minute)
        .unwrap_or_else(|| "-".to_string());
    out.push(fill(
        s.session_overview_line,
        &[
            ("agent", &session.agent),
            ("start", &display_minute(&session.started_at)),
            ("end", &end),
            ("prompts", &digest.prompt_count.to_string()),
            ("tools", &digest.tool_use_count.to_string()),
        ],
    ));
    if digest.errors > 0 {
        out.push(fill(
            s.session_errors_line,
            &[("n", &digest.errors.to_string())],
        ));
    }
    out.push(String::new());

    out.push(s.session_prompts.to_string());
    if digest.prompts.is_empty() {
        out.push(s.none.to_string());
    }
    for (i, p) in digest.prompts.iter().enumerate() {
        out.push(format!("{}. {}", i + 1, one_line(p)));
    }
    out.push(String::new());

    out.push(s.session_files.to_string());
    push_files(&mut out, &digest.files, s.none);
    out.push(String::new());

    out.push(s.session_commands.to_string());
    if digest.commands.is_empty() {
        out.push(s.none.to_string());
    }
    for c in &digest.commands {
        out.push(format!("- `{}`", c.replace('`', "'")));
    }
    out.push(String::new());

    if !digest.git_commits.is_empty() {
        out.push(s.session_commits.to_string());
        for c in &digest.git_commits {
            out.push(format!("- {c}"));
        }
        out.push(String::new());
    }

    if let Some(reply) = &digest.last_reply {
        out.push(s.session_last_reply.to_string());
        out.extend(quote_lines(reply));
        out.push(String::new());
    }

    // The handoff content carries its own `## 引き継ぎ…` heading (spec §7.2 / §7.5).
    out.push(handoff_md.trim_end().to_string());
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// STATE.md title.
pub fn state_title(lang: Lang, project_name: &str) -> String {
    fill(strings(lang).state_title, &[("name", project_name)])
}

/// One recent-session line of STATE.md.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSession {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Agent name.
    pub agent: String,
    /// Handoff lane of the session (`None` = project lane), shown as `[<lane>]`.
    pub lane: Option<String>,
    /// Machine that ran the session, shown as `@<machine>` (SPEC-M3.0 §6).
    pub machine: Option<String>,
    /// Session page title.
    pub title: String,
    /// Path relative to the project directory (`sessions/…md`).
    pub rel_path: String,
}

/// STATE.md body (spec §7.4, SPEC-M3.0 §1): latest handoff, carried decisions and open
/// questions and pinned pages (each only when non-empty), recent sessions, hot files.
pub fn state_body(
    lang: Lang,
    latest: Option<&Handoff>,
    carried: &Carried,
    pinned: &[PinnedPage],
    recent: &[StateSession],
    hot_files: &[FileCount],
) -> String {
    let s = strings(lang);
    let mut out: Vec<String> = vec![s.state_latest_handoff.to_string()];
    match latest {
        Some(h) => {
            out.push(fill(
                s.state_handoff_meta,
                &[
                    ("date", &display_minute(&h.created_at)),
                    ("agent", h.agent.as_deref().unwrap_or("unknown")),
                    ("source", h.source.as_str()),
                ],
            ));
            out.push(String::new());
            out.push(demote_headings(h.content_md.trim_end()));
        }
        None => out.push(s.state_no_handoff.to_string()),
    }
    out.push(String::new());

    let (decisions, open) = carried_lines(carried);
    for (heading, lines) in [
        (s.carried_decisions, decisions),
        (s.carried_open_questions, open),
        (s.pinned_pages, pinned.iter().flat_map(pinned_lines).collect()),
    ] {
        if !lines.is_empty() {
            out.push(heading.to_string());
            out.extend(lines);
            out.push(String::new());
        }
    }

    out.push(s.state_recent_sessions.to_string());
    if recent.is_empty() {
        out.push(s.none.to_string());
    }
    for r in recent {
        out.push(recent_line(
            &r.date,
            &r.agent,
            r.lane.as_deref(),
            r.machine.as_deref(),
            &r.title,
            &r.rel_path,
        ));
    }
    out.push(String::new());

    out.push(s.state_hot_files.to_string());
    push_files(&mut out, hot_files, s.none);
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn push_files(out: &mut Vec<String>, files: &[FileCount], none: &str) {
    if files.is_empty() {
        out.push(none.to_string());
    }
    for f in files {
        out.push(format!("- {} ({})", f.path, f.count));
    }
}

/// `agent@machine` when the machine is known, else `agent` (SPEC-M3.0 §6).
pub fn agent_label(agent: &str, machine: Option<&str>) -> String {
    match machine.filter(|m| !m.is_empty()) {
        Some(m) => format!("{agent}@{m}"),
        None => agent.to_string(),
    }
}

/// `MM-DD` of a `YYYY-MM-DD…` date (the date itself when it is shorter).
fn month_day(date: &str) -> String {
    let d: Vec<char> = date.chars().collect();
    if d.len() >= 10 {
        d[5..10].iter().collect()
    } else {
        date.to_string()
    }
}

/// One carried line: `- <prefix><text> (MM-DD)` (SPEC-M3.0 §1).
pub fn carried_line(prefix: &str, item: &CarriedItem) -> String {
    format!("- {prefix}{} ({})", one_line(&item.text), month_day(&item.date))
}

/// The lines of the carried sections: decisions then `✓` verified facts (section 3), open
/// questions then `⚠` gotchas (section 4).
pub fn carried_lines(carried: &Carried) -> (Vec<String>, Vec<String>) {
    let decisions = carried
        .decisions
        .iter()
        .map(|i| carried_line("", i))
        .chain(carried.verified.iter().map(|i| carried_line("✓ ", i)))
        .collect();
    let open = carried
        .open_questions
        .iter()
        .map(|i| carried_line("", i))
        .chain(carried.gotchas.iter().map(|i| carried_line("⚠ ", i)))
        .collect();
    (decisions, open)
}

/// A pinned page: `- <title> (<path>)` and its excerpt as `  > ` lines (blank lines dropped).
pub fn pinned_lines(page: &PinnedPage) -> Vec<String> {
    let mut out = vec![format!("- {} ({})", one_line(&page.title), page.path)];
    out.extend(
        page.excerpt
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("  > {l}")),
    );
    out
}

/// One recent-session line: `- <date> <agent> [<lane>] @<machine> — <title> (<path>)`; the
/// title loses its `<date time agent> — ` prefix.
pub fn recent_line(
    date: &str,
    agent: &str,
    lane: Option<&str>,
    machine: Option<&str>,
    title: &str,
    path: &str,
) -> String {
    let summary = title.split_once(" — ").map(|(_, t)| t).unwrap_or(title);
    let lane = lane.map(|l| format!(" [{l}]")).unwrap_or_default();
    let machine = machine
        .filter(|m| !m.is_empty())
        .map(|m| format!(" @{m}"))
        .unwrap_or_default();
    let agent = if agent.is_empty() {
        String::new()
    } else {
        format!(" {agent}")
    };
    format!("- {date}{agent}{lane}{machine} — {summary} ({path})")
}

/// `text` as Markdown quote lines (`> …`), so a reply's own headings cannot start sections.
pub fn quote_lines(text: &str) -> Vec<String> {
    text.trim()
        .lines()
        .map(|l| {
            let l = l.trim_end();
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect()
}

/// Pushes Markdown headings one level down so embedded handoffs nest under a `##` section.
fn demote_headings(md: &str) -> String {
    md.lines()
        .map(|l| {
            if l.starts_with('#') {
                format!("#{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::HandoffSource;
    use crate::session::SessionStatus;

    fn session() -> Session {
        Session {
            machine: None,
            id: "0c2f1a2b-3c4d".into(),
            project_id: "kioku-3f9a1c2e".into(),
            agent: "claude-code".into(),
            cwd: "/x".into(),
            source: "startup".into(),
            started_at: "2026-09-25T02:14:00.000Z".into(),
            ended_at: Some("2026-09-25T03:40:11.000Z".into()),
            status: SessionStatus::Open,
            root_path: None,
            lane: None,
        }
    }

    #[test]
    fn session_path_and_title() {
        let s = session();
        assert_eq!(
            session_page_path(&s),
            "kioku-3f9a1c2e/sessions/2026-09-25-0c2f1a2b-9618b70bdda7.md"
        );
        let d = SessionDigest {
            prompts: vec!["引き継ぎサーバーの設計\nをしたい".into()],
            ..SessionDigest::default()
        };
        assert_eq!(
            session_title(Lang::Ja, &s, &d),
            "2026-09-25 02:14 claude-code — 引き継ぎサーバーの設計"
        );
    }

    #[test]
    fn session_body_sections() {
        let d = SessionDigest {
            prompts: vec!["a".into(), "b".into()],
            files: vec![FileCount {
                path: "src/a.rs".into(),
                count: 2,
            }],
            commands: vec!["cargo test".into()],
            prompt_count: 2,
            tool_use_count: 3,
            ..SessionDigest::default()
        };
        let body = session_body(Lang::Ja, &session(), &d, "## 引き継ぎ（自動生成）\nx\n");
        assert!(body.starts_with("## 概要\n- エージェント: claude-code / 開始: 2026-09-25 02:14 / 終了: 2026-09-25 03:40 / プロンプト 2 / ツール実行 3\n"));
        assert!(body.contains("## 指示 (prompts)\n1. a\n2. b\n"));
        assert!(body.contains("## 変更したファイル\n- src/a.rs (2)\n"));
        assert!(body.contains("## 実行したコマンド\n- `cargo test`\n"));
        assert!(body.ends_with("## 引き継ぎ（自動生成）\nx\n"));
    }

    #[test]
    fn state_body_sections() {
        let h = Handoff {
            id: "h".into(),
            project_id: "p".into(),
            session_id: Some("s".into()),
            source: HandoffSource::Rules,
            content_md: "## 引き継ぎ（自動生成）\n最後の指示: x\n".into(),
            created_at: "2026-09-25T03:40:11.000Z".into(),
            accepted_at: None,
            accepted_by: None,
            agent: Some("claude-code".into()),
            updated_at: None,
            lane: None,
        };
        let recent = vec![
            StateSession {
                machine: None,
                date: "2026-09-25".into(),
                agent: "claude-code".into(),
                lane: None,
                title: "2026-09-25 02:14 claude-code — 設計".into(),
                rel_path: "sessions/2026-09-25-0c2f1a2b.md".into(),
            },
            StateSession {
                machine: Some("mini".into()),
                date: "2026-09-25".into(),
                agent: "codex".into(),
                lane: Some("feature/検索".into()),
                title: "2026-09-25 03:00 codex — 検索".into(),
                rel_path: "sessions/2026-09-25-11111111.md".into(),
            },
        ];
        let item = |text: &str, date: &str| CarriedItem {
            text: text.into(),
            date: date.into(),
            handoff_id: "h1".into(),
        };
        let carried = Carried {
            decisions: vec![item("lindera を使う", "2026-09-28")],
            verified: vec![item("cargo test は通る", "2026-09-27")],
            open_questions: vec![item("再ランキングは？", "2026-09-27")],
            gotchas: vec![item("Windows の\nパス", "2026-09-26")],
        };
        let pinned = [PinnedPage {
            path: "_global/rules.md".into(),
            title: "作業ルール".into(),
            excerpt: "main に直接 push しない\n\n## 手順".into(),
        }];
        let files = [FileCount {
            path: "a.rs".into(),
            count: 3,
        }];
        let body = state_body(Lang::Ja, Some(&h), &carried, &pinned, &recent, &files);
        assert!(body.starts_with("## 最新の引き継ぎ\n_2026-09-25 03:40 / claude-code / source: rules_\n\n### 引き継ぎ（自動生成）\n"));
        assert!(body.contains("\n## 決定事項（これまでの引き継ぎ）\n- lindera を使う (09-28)\n- ✓ cargo test は通る (09-27)\n\n## 未解決（これまでの引き継ぎ）\n- 再ランキングは？ (09-27)\n- ⚠ Windows の パス (09-26)\n\n## ピン留め\n- 作業ルール (_global/rules.md)\n  > main に直接 push しない\n  > ## 手順\n\n## 最近のセッション\n"), "{body}");
        assert!(body.contains("## 最近のセッション\n- 2026-09-25 claude-code — 設計 (sessions/2026-09-25-0c2f1a2b.md)\n- 2026-09-25 codex [feature/検索] @mini — 検索 (sessions/2026-09-25-11111111.md)\n"));
        // nothing carried, nothing pinned → no empty sections
        let body = state_body(Lang::En, None, &Carried::default(), &[], &[], &files);
        assert!(!body.contains("(carried)") && !body.contains("Pinned"), "{body}");
        assert!(body.ends_with("## Frequently touched files (last 10 sessions)\n- a.rs (3)\n"));
    }
}
