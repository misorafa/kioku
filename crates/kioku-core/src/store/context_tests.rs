//! SPEC-M3.0 tests on the store: the SessionStart sections (carried items, pinned pages,
//! recent sessions with machines, the previous session's last reply), STATE.md's copy of
//! them, `assistant` observations (§3), rules-handoff granularity (§4) and the time since
//! the last handoff (§4).

use super::*;
use crate::session::CarriedItem;
use serde_json::json;

fn project() -> ProjectIdentity {
    ProjectIdentity {
        id: "kioku-3f9a1c2e".into(),
        name: "記憶".into(),
        root: "/home/u/記憶".into(),
        remote: Some("github.com/u/kioku".into()),
    }
}

fn start(
    store: &Store,
    session: &str,
    agent: &str,
    lane: Option<&str>,
    machine: Option<&str>,
) -> SessionStartResponse {
    store
        .start_session(&SessionStartRequest {
            session_id: session.into(),
            agent: agent.into(),
            cwd: "/home/u/記憶".into(),
            source: "startup".into(),
            project: project(),
            lane: lane.map(str::to_string),
            machine: machine.map(str::to_string),
        })
        .unwrap()
}

fn observe(store: &Store, session: &str, kind: ObservationKind, payload: serde_json::Value) {
    store
        .add_observation(&NewObservation {
            event_id: None,
            session_id: session.into(),
            kind,
            ts: None,
            payload,
        })
        .unwrap();
}

fn work(store: &Store, session: &str, prompt: &str) {
    observe(
        store,
        session,
        ObservationKind::Prompt,
        json!({ "prompt": prompt }),
    );
    observe(
        store,
        session,
        ObservationKind::ToolUse,
        json!({"tool_name": "Edit", "tool_input": {"file_path": "/home/u/記憶/src/索引.rs"}, "tool_response": {}}),
    );
}

fn reply(store: &Store, session: &str, text: &str) {
    observe(
        store,
        session,
        ObservationKind::Assistant,
        json!({ "text": text }),
    );
}

#[derive(Default)]
struct Items<'a> {
    decisions: &'a [&'a str],
    open: &'a [&'a str],
    verified: &'a [&'a str],
    gotchas: &'a [&'a str],
}

fn handoff(store: &Store, session: &str, summary: &str, items: Items<'_>) -> Handoff {
    let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    store
        .write_handoff(&HandoffInput {
            project: project().id,
            session: Some(session.into()),
            summary: summary.into(),
            next_steps: vec!["続ける".into()],
            open_questions: v(items.open),
            decisions: v(items.decisions),
            verified: v(items.verified),
            gotchas: v(items.gotchas),
        })
        .unwrap()
}

fn texts(items: &[CarriedItem]) -> Vec<&str> {
    items.iter().map(|i| i.text.as_str()).collect()
}

fn pin(store: &Store, title: &str, global: bool, content: &str, tags: &[&str]) -> String {
    store
        .write_page(&WritePageRequest {
            title: title.into(),
            content: content.into(),
            project: (!global).then(|| project().id),
            scope: Some(if global {
                PageScope::Global
            } else {
                PageScope::Project
            }),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..WritePageRequest::default()
        })
        .unwrap()
}

/// Handoffs across three sessions on two lanes, a duplicate decision, a question resolved
/// later, pinned pages: what the next session on each lane is given.
#[test]
fn start_carries_items_pins_pages_and_names_machines() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();

    start(&store, "s1", "claude-code", None, Some("mini"));
    work(&store, "s1", "検索を設計して");
    handoff(
        &store,
        "s1",
        "設計した",
        Items {
            decisions: &["lindera を使う", "SQLite は WAL"],
            open: &["検索の再ランキングはどうする？", "Windows の CI が遅い"],
            verified: &["cargo test は全件通る"],
            ..Items::default()
        },
    );
    store.finalize_session("s1").unwrap();

    start(&store, "s2", "codex", Some("feature/検索"), Some("win-pc"));
    work(&store, "s2", "ブランチで検索を直して");
    handoff(
        &store,
        "s2",
        "ブランチで直した",
        Items {
            decisions: &[
                "ＬＩＮＤＥＲＡ を使う",
                "検索の再ランキングはどうするか → M3.1 でやる",
            ],
            gotchas: &["Windows ではパス区切りが \\ になる"],
            ..Items::default()
        },
    );
    reply(&store, "s2", "ブランチの作業を終えました。");
    store.finalize_session("s2").unwrap();

    start(&store, "s3", "claude-code", None, Some("mini"));
    work(&store, "s3", "テストを足して");
    handoff(
        &store,
        "s3",
        "テストを足した",
        Items {
            decisions: &["machine 名は hostname から取る"],
            ..Items::default()
        },
    );
    reply(&store, "s3", "検索のテストを追加しました。次は README です。");
    store.finalize_session("s3").unwrap();

    let rules = pin(
        &store,
        "作業ルール",
        true,
        "main に直接 push しない。\n\n## 詳細\n本文",
        &["Pinned", "rules"],
    );
    let design = pin(&store, "設計メモ", false, "索引は一つ", &["pinned"]);
    pin(&store, "普通のページ", false, "固定しない", &["memo"]);

    // s4 on the main line receives s3's handoff (section 2), so s3's items are not repeated.
    let r = start(&store, "s4", "claude-code", None, Some("mini"));
    let s4 = r.clone();
    assert_eq!(r.context_version, CONTEXT_VERSION);
    let shown = r.pending_handoff.clone().unwrap();
    assert_eq!(shown.session_id.as_deref(), Some("s3"));
    assert!(
        shown
            .content_md
            .starts_with("## 引き継ぎ（claude-code@mini, "),
        "{}",
        shown.content_md
    );
    assert_eq!(
        texts(&r.decisions),
        [
            "ＬＩＮＤＥＲＡ を使う",
            "検索の再ランキングはどうするか → M3.1 でやる",
            "SQLite は WAL"
        ]
    );
    assert_eq!(texts(&r.verified), ["cargo test は全件通る"]);
    assert_eq!(texts(&r.open_questions), ["Windows の CI が遅い"]);
    assert_eq!(texts(&r.gotchas), ["Windows ではパス区切りが \\ になる"]);
    assert_eq!(r.decisions[0].date, display_date(&now_ts()));
    let pinned: Vec<(&str, &str)> = r
        .pinned
        .iter()
        .map(|p| (p.path.as_str(), p.excerpt.as_str()))
        .collect();
    assert_eq!(
        pinned,
        [
            (design.as_str(), "索引は一つ"),
            (rules.as_str(), "main に直接 push しない。\n\n## 詳細\n本文")
        ]
    );
    let recent: Vec<(&str, Option<&str>, Option<&str>)> = r
        .recent_sessions
        .iter()
        .map(|s| (s.agent.as_str(), s.lane.as_deref(), s.machine.as_deref()))
        .collect();
    assert_eq!(
        recent,
        [
            ("claude-code", None, Some("mini")),
            ("codex", Some("feature/検索"), Some("win-pc")),
            ("claude-code", None, Some("mini")),
        ]
    );
    // the previous main-line session is s3, whose handoff is section 2: no repeat
    assert_eq!(r.last_reply, None);

    // s5 on the main line: nothing pending (s4 took it) → s3's last reply is shown
    let r = start(&store, "s5", "claude-code", None, None);
    assert!(r.pending_handoff.is_none());
    assert_eq!(
        r.last_reply.as_deref(),
        Some("検索のテストを追加しました。次は README です。")
    );
    // nothing was shown in section 2: every decision is carried, newest first
    assert_eq!(r.decisions[0].text, "machine 名は hostname から取る");

    // the session page and STATE.md show machine and the carried sections
    let page = store.read_page(&r.recent_sessions[0].path).unwrap();
    assert_eq!(page.frontmatter.machine.as_deref(), Some("mini"));
    let state = store
        .read_page(&format!("{}/STATE.md", project().id))
        .unwrap();
    assert!(
        state.body.contains("## 決定事項（これまでの引き継ぎ）\n- ＬＩＮＤＥＲＡ を使う ("),
        "{}",
        state.body
    );
    assert!(state.body.contains("- ✓ cargo test は全件通る ("));
    assert!(
        state
            .body
            .contains("## 未解決（これまでの引き継ぎ）\n- Windows の CI が遅い (")
    );
    assert!(state.body.contains("- ⚠ Windows ではパス区切りが"));
    assert!(state.body.contains(" codex [feature/検索] @win-pc — "));
    // STATE.md was written at s3's finalize, before the pages were pinned
    assert!(!state.body.contains("## ピン留め"));

    // GET /sessions/{id}/context gives the same sections without side effects
    let ctx = store.session_context("s4").unwrap();
    assert_eq!(ctx.context_version, CONTEXT_VERSION);
    assert_eq!(ctx.decisions, s4.decisions);
    assert_eq!(ctx.open_questions, s4.open_questions);
    assert_eq!(ctx.pinned, s4.pinned);
}

/// A pinned page's excerpt is cut at 400 chars; at most three pages, newest first.
#[test]
fn pinned_pages_are_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "s1", "claude-code", None, None);
    for i in 0..5 {
        pin(&store, &format!("固定 {i}"), i % 2 == 0, &"あ".repeat(1000), &["pinned"]);
    }
    let r = start(&store, "s2", "claude-code", None, None);
    assert_eq!(r.pinned.len(), MAX_PINNED);
    assert!(r.pinned.iter().all(|p| p.excerpt.chars().count() == PINNED_EXCERPT));
    assert_eq!(r.pinned[0].title, "固定 4");
}

/// SPEC-M3.0 §3: a reply is stored sanitized and capped, once per distinct text in a row,
/// and reaches the session page and the rules handoff (which then needs no "unknown").
#[test]
fn assistant_reply_is_stored_once_and_reaches_page_and_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "s1", "codex", None, None);
    work(&store, "s1", "README を要約して");
    let long = format!(
        "README を要約しました。token=sk-abcdefghijklmnop1234 を使った。\n## 見出し\n{}",
        "長".repeat(3000)
    );
    reply(&store, "s1", &long);
    reply(&store, "s1", &long);
    let stored: Vec<Observation> = store
        .observations("s1")
        .unwrap()
        .into_iter()
        .filter(|o| o.kind == ObservationKind::Assistant)
        .collect();
    assert_eq!(stored.len(), 1, "a repeated reply is stored once");
    let text = stored[0].payload["text"].as_str().unwrap();
    assert!(!text.contains("sk-abcdefghijklmnop1234"), "{text}");
    assert!(text.contains("[REDACTED]"));
    assert_eq!(text.chars().count(), ASSISTANT_MAX);
    // an empty reply is refused
    assert!(
        store
            .add_observation(&NewObservation {
                event_id: None,
                session_id: "s1".into(),
                kind: ObservationKind::Assistant,
                ts: None,
                payload: json!({"text": "  "}),
            })
            .is_err()
    );

    let done = store.finalize_session("s1").unwrap();
    let page = store.read_page(&done.session_page.unwrap()).unwrap();
    assert!(
        page.body
            .contains("## 最後の回答\n> README を要約しました。token=[REDACTED]"),
        "{}",
        page.body
    );
    assert!(page.body.contains("\n> ## 見出し\n"), "headings are quoted");
    let h = handoff_row(&store, &done.handoff_id.unwrap());
    assert!(
        h.content_md
            .contains("最後の回答（要約）: README を要約しました。"),
        "{}",
        h.content_md
    );
    assert!(!h.content_md.contains("次にやること"), "{}", h.content_md);

    // a different reply is stored; the digest keeps the last one
    reply(&store, "s1", "追加で直しました。");
    reply(&store, "s1", "README を要約しました。");
    assert_eq!(
        store
            .observations("s1")
            .unwrap()
            .iter()
            .filter(|o| o.kind == ObservationKind::Assistant)
            .count(),
        3
    );
    assert_eq!(
        store.digest("s1").unwrap().last_reply.as_deref(),
        Some("README を要約しました。")
    );
}

fn handoff_row(store: &Store, id: &str) -> Handoff {
    db::get_handoff(&store.db.lock(), id).unwrap().unwrap()
}

fn rules_rows(store: &Store, session: &str) -> i64 {
    store
        .db
        .lock()
        .query_row(
            "SELECT COUNT(*) FROM handoffs WHERE session_id = ?1 AND source = 'rules'",
            [session],
            |r| r.get(0),
        )
        .unwrap()
}

fn read(store: &Store, session: &str, n: usize) {
    for i in 0..n {
        observe(
            store,
            session,
            ObservationKind::ToolUse,
            json!({"tool_name": "Read", "tool_input": {"file_path": format!("/home/u/記憶/設計{i}.md")}}),
        );
    }
}

/// SPEC-M3.0 §4: once another session accepted a session's rules handoff, a Stop without a
/// meaningful change refreshes that handoff in place; a prompt, an edit, a reply or five tool
/// uses issue a new one.
#[test]
fn rules_handoff_is_reissued_only_on_meaningful_change() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "s1", "claude-code", None, None);
    work(&store, "s1", "索引を作り直して");
    let first = store.finalize_session("s1").unwrap().handoff_id.unwrap();
    assert_eq!(rules_rows(&store, "s1"), 1);
    // a parallel session takes it
    let r = start(&store, "s2", "codex", None, None);
    assert_eq!(r.pending_handoff.unwrap().id, first);

    // one read: refreshed in place, still accepted, no new row
    read(&store, "s1", 1);
    let again = store.finalize_session("s1").unwrap().handoff_id.unwrap();
    assert_eq!(again, first);
    assert_eq!(rules_rows(&store, "s1"), 1);
    let row = handoff_row(&store, &first);
    assert!(row.accepted_at.is_some() && row.updated_at.is_some());
    // three more reads (4 since it was issued): still the same row
    read(&store, "s1", 3);
    store.finalize_session("s1").unwrap();
    assert_eq!(rules_rows(&store, "s1"), 1);
    // the fifth tool use since it was issued matters: a new, pending row
    read(&store, "s1", 1);
    let next = store.finalize_session("s1").unwrap().handoff_id.unwrap();
    assert_ne!(next, first);
    assert_eq!(rules_rows(&store, "s1"), 2);
    assert!(handoff_row(&store, &next).accepted_at.is_none());

    // accepted again, then a reply alone is a meaningful change
    start(&store, "s3", "codex", None, None);
    reply(&store, "s1", "終わりました。");
    let third = store.finalize_session("s1").unwrap().handoff_id.unwrap();
    assert_ne!(third, next);
    assert_eq!(rules_rows(&store, "s1"), 3);
    // while it is pending, finalize refreshes it in place (as before), whatever changed
    work(&store, "s1", "もう一度");
    assert_eq!(
        store.finalize_session("s1").unwrap().handoff_id.unwrap(),
        third
    );
    assert_eq!(rules_rows(&store, "s1"), 3);
}

/// SPEC-M3.0 §4: the Stop hook's clock is the server's — seconds since the last agent
/// handoff, or since the start without one.
#[test]
fn session_info_reports_seconds_since_the_last_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "s1", "claude-code", None, None);
    work(&store, "s1", "始める");
    let info = store.session_info("s1").unwrap();
    assert!(info.secs_since_handoff.is_some_and(|s| s < 60));
    // back-date the start: the time counts from it while there is no handoff
    store
        .db
        .lock()
        .execute(
            "UPDATE sessions SET started_at = '2026-01-01T00:00:00.000Z' WHERE id = 's1'",
            [],
        )
        .unwrap();
    assert!(
        store
            .session_info("s1")
            .unwrap()
            .secs_since_handoff
            .is_some_and(|s| s > 3600)
    );
    handoff(&store, "s1", "書いた", Items::default());
    assert!(
        store
            .session_info("s1")
            .unwrap()
            .secs_since_handoff
            .is_some_and(|s| s < 60)
    );
}
