//! SPEC-M3.1 §1 tests on the store: who consumes a handoff — new / compact / resume /
//! a second concurrent session / a session's own handoff / explicit accept — on the project
//! lane and on a branch lane, `superseded`, and `kioku_handoff_pending(history)`.

use super::*;
use crate::handoff::{REFERENCE_CONCURRENT, REFERENCE_RESUMED, SUPERSEDED};
use serde_json::json;

fn project() -> ProjectIdentity {
    ProjectIdentity {
        id: "kioku-3f9a1c2e".into(),
        name: "記憶".into(),
        root: "/home/u/記憶".into(),
        remote: Some("github.com/u/kioku".into()),
    }
}

fn start_with(
    store: &Store,
    session: &str,
    lane: Option<&str>,
    source: &str,
) -> SessionStartResponse {
    store
        .start_session(&SessionStartRequest {
            session_id: session.into(),
            agent: "claude-code".into(),
            cwd: "/home/u/記憶".into(),
            source: source.into(),
            project: project(),
            lane: lane.map(str::to_string),
            machine: None,
        })
        .unwrap()
}

fn start(store: &Store, session: &str, lane: Option<&str>) -> SessionStartResponse {
    start_with(store, session, lane, "startup")
}

/// A prompt observation `ago` minutes in the past.
fn work(store: &Store, session: &str, ago: i64) {
    let ts = util::fmt_ts(util::now() - chrono::Duration::minutes(ago));
    store
        .add_observation(&NewObservation {
            event_id: None,
            session_id: session.into(),
            kind: ObservationKind::Prompt,
            ts: Some(ts),
            payload: json!({ "prompt": "索引を直して" }),
        })
        .unwrap();
}

fn write(store: &Store, session: &str, summary: &str) -> Handoff {
    store
        .write_handoff(&HandoffInput {
            project: project().id,
            session: Some(session.into()),
            summary: summary.into(),
            next_steps: vec!["テストを書く".into()],
            decisions: vec!["日本語で書く".into()],
            ..HandoffInput::default()
        })
        .unwrap()
}

fn row(store: &Store, id: &str) -> Handoff {
    db::get_handoff(&store.db.lock(), id).unwrap().unwrap()
}

fn s(lane: Option<&str>, name: &str) -> String {
    let prefix = if lane.is_some() { "feat" } else { "main" };
    format!("{prefix}-{name}")
}

#[test]
fn consumption_matrix_on_two_lanes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    for lane in [None, Some("feature/検索")] {
        // writer: an earlier session on this lane, finished long ago
        let writer = s(lane, "writer");
        start(&store, &writer, lane);
        work(&store, &writer, 120);
        let older = write(&store, &writer, "古い引き継ぎ");
        let newest = write(&store, &writer, "最新の引き継ぎ");
        store.finalize_session(&writer).unwrap();

        // 1. a genuinely new session accepts the newest; the older one is superseded
        let fresh = s(lane, "new");
        let resp = start(&store, &fresh, lane);
        let got = resp.pending_handoff.expect("accepted");
        assert_eq!(got.id, newest.id);
        assert_eq!(got.accepted_by.as_deref(), Some(fresh.as_str()));
        assert!(resp.reference_handoff.is_none());
        assert_eq!(
            row(&store, &older.id).accepted_by.as_deref(),
            Some(SUPERSEDED)
        );
        work(&store, &fresh, 1);

        // 2. compact / resume of that session: the same handoff again, nothing new accepted
        let later = write(&store, &writer, "後から届いた引き継ぎ");
        for source in ["compact", "resume", "startup"] {
            let again = start_with(&store, &fresh, lane, source);
            assert_eq!(again.pending_handoff.unwrap().id, newest.id, "{source}");
            assert!(again.reference_handoff.is_none(), "{source}");
            assert!(row(&store, &later.id).accepted_at.is_none(), "{source}");
        }

        // 3. a second session while `fresh` is active (observation < 30 min, open): the
        //    pending handoff is only a reference and stays pending
        let second = s(lane, "second");
        let resp = start(&store, &second, lane);
        assert!(resp.pending_handoff.is_none());
        assert_eq!(resp.reference_handoff.unwrap().id, later.id);
        assert_eq!(resp.reference_reason.as_deref(), Some(REFERENCE_CONCURRENT));
        assert!(row(&store, &later.id).accepted_by.is_none());

        // 4. a resume with an id that never accepted anything: reference only
        let cleared = s(lane, "cleared");
        let resp = start_with(&store, &cleared, lane, "resume");
        assert!(resp.pending_handoff.is_none());
        assert_eq!(resp.reference_handoff.unwrap().id, later.id);
        assert_eq!(resp.reference_reason.as_deref(), Some(REFERENCE_RESUMED));
        assert!(row(&store, &later.id).accepted_at.is_none());

        // 5. explicit accept ignores the busy lane
        let acc = store
            .pending_handoff_routed(&project().id, true, Some(&second), None)
            .unwrap();
        assert_eq!(acc.handoff.unwrap().id, later.id);
        assert_eq!(
            row(&store, &later.id).accepted_by.as_deref(),
            Some(second.as_str())
        );

        // 6. once the other sessions are finalized (or quiet for 30 min) a new one accepts
        for id in [&fresh, &second, &cleared] {
            store.finalize_session(id).unwrap();
        }
        let next = write(&store, &writer, "次の引き継ぎ");
        let idle = s(lane, "idle");
        let resp = start(&store, &idle, lane);
        assert_eq!(resp.pending_handoff.unwrap().id, next.id);
        work(&store, &idle, 45);
        let quiet = write(&store, &writer, "静かなレーン");
        let resp = start(&store, &s(lane, "after-quiet"), lane);
        assert_eq!(
            resp.pending_handoff.map(|h| h.id),
            Some(quiet.id.clone()),
            "an observation 45 minutes old does not make the lane busy"
        );
    }
    // the lanes never touched each other: every handoff of one lane names its own sessions
    let conn = store.db.lock();
    for h in db::lane_handoffs(&conn, &project().id, Some("feature/検索"), 20).unwrap() {
        if let Some(by) = h.accepted_by.as_deref().filter(|b| *b != SUPERSEDED) {
            assert!(by.starts_with("feat-"), "{by}");
        }
    }
}

/// Rule 2: the handoff a session wrote itself (its previous turn's Stop) is never accepted
/// by it, neither at start nor through an explicit accept; it waits for the next session.
#[test]
fn a_session_never_accepts_its_own_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    for lane in [None, Some("feature")] {
        let me = s(lane, "me");
        start(&store, &me, lane);
        work(&store, &me, 1);
        let mine = write(&store, &me, "自分の引き継ぎ");
        let resp = start_with(&store, &me, lane, "startup");
        assert!(resp.pending_handoff.is_none());
        assert!(
            resp.reference_handoff.is_none(),
            "own handoff is not even a reference: {:?}",
            resp.reference_handoff
        );
        let explicit = store
            .pending_handoff_routed(&project().id, true, Some(&me), None)
            .unwrap();
        assert!(explicit.handoff.is_none());
        assert!(row(&store, &mine.id).accepted_at.is_none());
        store.finalize_session(&me).unwrap();
        // the next session gets it
        let next = s(lane, "next");
        assert_eq!(
            start(&store, &next, lane).pending_handoff.unwrap().id,
            mine.id
        );
        store.finalize_session(&next).unwrap();
    }
}

#[test]
fn history_lists_the_lane_with_status() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "w", Some("feature"));
    let a = write(&store, "w", "一つ目の引き継ぎ");
    let b = write(&store, "w", "二つ目の引き継ぎ");
    store.finalize_session("w").unwrap();
    start(&store, "r", Some("feature"));
    store.finalize_session("r").unwrap();
    let c = write(&store, "w", "三つ目の引き継ぎ");
    // another lane is not part of the history
    start(&store, "m", None);
    write(&store, "m", "メインの引き継ぎ");

    let history = store
        .handoff_history(&project().id, None, Some("feature"), 10)
        .unwrap();
    let ids: Vec<&str> = history.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, [c.id.as_str(), b.id.as_str(), a.id.as_str()]);
    assert_eq!(history[0].status(), "pending");
    assert_eq!(history[1].status(), "accepted by r");
    assert_eq!(history[2].status(), SUPERSEDED);
    assert!(history[0].content_md.contains("三つ目の引き継ぎ"));
    // by session, capped, and never more than MAX_HISTORY
    assert_eq!(
        store
            .handoff_history(&project().id, Some("r"), None, 1)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .handoff_history(&project().id, None, None, 500)
            .unwrap()
            .len(),
        1
    );
}

/// Claude Code's `/clear` starts a new session id with `source = clear`: it is a new
/// session and takes the lane's pending handoff like any other (review of PR #11).
#[test]
fn clear_with_a_new_id_is_a_new_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    let lane = Some("feature/クリア");
    let writer = s(lane, "writer");
    start(&store, &writer, lane);
    let h = write(&store, &writer, "クリア前の引き継ぎ");
    store.finalize_session(&writer).unwrap();
    let cleared = s(lane, "after-clear");
    let resp = start_with(&store, &cleared, lane, "clear");
    assert_eq!(resp.pending_handoff.unwrap().id, h.id);
    assert_eq!(
        row(&store, &h.id).accepted_by.as_deref(),
        Some(cleared.as_str())
    );
    // The same id again with `clear` is a resume: it gets its accepted handoff back.
    let again = start_with(&store, &cleared, lane, "clear");
    assert_eq!(again.pending_handoff.unwrap().id, h.id);
}

// ------------------------------------------------------------------ SPEC-M3.5 §1

/// The lane of the audit finding (SPEC-M3.5): one long-lived session, nobody else.
const CRYPT_LANE: &str = "ShinichiroGoto/DBの完全暗号化";

fn write_decision(store: &Store, session: &str, summary: &str, decision: &str) -> Handoff {
    store
        .write_handoff(&HandoffInput {
            project: project().id,
            session: Some(session.into()),
            summary: summary.into(),
            next_steps: vec!["鍵の保管場所を決める".into()],
            decisions: vec![decision.into()],
            ..HandoffInput::default()
        })
        .unwrap()
}

fn edit(store: &Store, session: &str, path: &str) {
    store
        .add_observation(&NewObservation {
            event_id: None,
            session_id: session.into(),
            kind: ObservationKind::ToolUse,
            ts: None,
            payload: json!({"tool_name": "Edit", "tool_input": {"file_path": path}, "tool_response": {}}),
        })
        .unwrap();
}

fn pending_of(store: &Store, session: &str, source: HandoffSource) -> Vec<Handoff> {
    db::lane_handoffs(&store.db.lock(), &project().id, Some(CRYPT_LANE), 50)
        .unwrap()
        .into_iter()
        .filter(|h| h.session_id.as_deref() == Some(session) && h.source == source)
        .filter(|h| h.accepted_at.is_none())
        .collect()
}

/// SPEC-M3.5 §1: a session writing three agent handoffs on a lane leaves one pending and
/// two superseded; another session's pending handoff on the lane is untouched; `carry`
/// still carries a decision from a superseded handoff; `history` shows the statuses.
#[test]
fn a_sessions_newer_handoff_supersedes_its_own_older_pending_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    let lane = Some(CRYPT_LANE);
    // two sessions on the lane; the other one leaves a pending handoff while "long" works
    start(&store, "other", lane);
    start(&store, "long", lane);
    work(&store, "long", 1);
    work(&store, "other", 120);
    let others = write_decision(
        &store,
        "other",
        "別セッションの引き継ぎ",
        "鍵は OS のキーチェーンに置く",
    );
    store.finalize_session("other").unwrap();
    let first = write_decision(
        &store,
        "long",
        "暗号化の方針を決めた",
        "SQLCipher で DB を暗号化する",
    );
    let second = write_decision(
        &store,
        "long",
        "移行手順を書いた",
        "移行は起動時に一度だけ行う",
    );
    let third = write_decision(&store, "long", "テストを通した", "平文の DB は移行後に消す");

    assert_eq!(
        row(&store, &first.id).accepted_by.as_deref(),
        Some(SUPERSEDED)
    );
    assert_eq!(
        row(&store, &second.id).accepted_by.as_deref(),
        Some(SUPERSEDED)
    );
    assert!(row(&store, &third.id).accepted_at.is_none());
    assert!(
        row(&store, &others.id).accepted_at.is_none(),
        "another session's pending handoff is not superseded by this one"
    );
    assert_eq!(pending_of(&store, "long", HandoffSource::Agent).len(), 1);
    // the project lane is another lane: what happens there does not touch this one
    start(&store, "main-writer", None);
    let main = write_decision(&store, "main-writer", "メインの引き継ぎ", "main は安定版");
    write_decision(&store, "main-writer", "メインの続き", "main は安定版のまま");
    assert_eq!(
        row(&store, &main.id).accepted_by.as_deref(),
        Some(SUPERSEDED)
    );
    assert!(row(&store, &third.id).accepted_at.is_none());

    // history shows what they are
    let history = store
        .handoff_history(&project().id, None, lane, 10)
        .unwrap();
    let status: Vec<(String, String)> =
        history.iter().map(|h| (h.id.clone(), h.status())).collect();
    for (id, want) in [
        (&third.id, "pending"),
        (&second.id, SUPERSEDED),
        (&first.id, SUPERSEDED),
        (&others.id, "pending"),
    ] {
        assert!(
            status.contains(&(id.clone(), want.to_string())),
            "{want}: {status:?}"
        );
    }

    // carried decisions (SPEC-M3.0) still read the superseded handoffs: a new session on
    // another lane sees every decision, the first one's included
    let resp = start(&store, "reader", Some("feature/鍵の保管"));
    let decisions: Vec<&str> = resp.decisions.iter().map(|d| d.text.as_str()).collect();
    for d in [
        "SQLCipher で DB を暗号化する",
        "移行は起動時に一度だけ行う",
        "平文の DB は移行後に消す",
    ] {
        assert!(decisions.contains(&d), "{d} missing from {decisions:?}");
    }
    assert!(
        resp.decisions.iter().any(|d| d.handoff_id == first.id),
        "the carried item names the superseded handoff"
    );
}

/// SPEC-M3.5 §1 (cause in §5): a session's rules handoff is refreshed in place even when
/// it predates the session's newer agent handoff (the addendum case used to insert a new
/// row each time); a new row is issued only after the old one was accepted and the work
/// changed, and the session never has two pending rules handoffs on the lane.
#[test]
fn rules_handoff_is_refreshed_not_duplicated_on_a_long_lived_lane() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    let lane = Some(CRYPT_LANE);
    let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
    start(&store, "long", lane);
    work(&store, "long", 1);
    let rules = store.finalize_session("long").unwrap().handoff_id.unwrap();
    assert_eq!(row(&store, &rules).source, HandoffSource::Rules);

    for round in 0..3 {
        pause();
        let agent = write_decision(
            &store,
            "long",
            &format!("{round} 回目の作業"),
            "暗号鍵は 32 バイト",
        );
        // enough work after the agent handoff for an addendum
        for f in ["暗号.rs", "移行.rs", "鍵.rs"] {
            edit(&store, "long", &format!("/home/u/記憶/src/{f}"));
        }
        pause();
        let id = store.finalize_session("long").unwrap().handoff_id.unwrap();
        assert_eq!(
            id, rules,
            "round {round}: refreshed in place, not a new row"
        );
        let r = row(&store, &id);
        assert!(
            r.created_at > agent.created_at,
            "the addendum ranks above its agent handoff"
        );
        assert!(r.content_md.contains("暗号.rs"), "{}", r.content_md);
        let peek = store
            .pending_handoff_routed(&project().id, false, None, lane)
            .unwrap()
            .handoff
            .unwrap();
        assert_eq!(peek.id, rules, "round {round}");
        assert_eq!(pending_of(&store, "long", HandoffSource::Rules).len(), 1);
        assert_eq!(pending_of(&store, "long", HandoffSource::Agent).len(), 1);
    }

    // a plain re-finalize keeps created_at
    let before = row(&store, &rules);
    work(&store, "long", 0);
    store.finalize_session("long").unwrap();
    assert_eq!(row(&store, &rules).created_at, before.created_at);

    // accepted by the next session, then the long session keeps working: a new row
    let next = start(&store, "next", lane).pending_handoff.unwrap();
    assert_eq!(next.id, rules);
    store.finalize_session("next").unwrap();
    work(&store, "long", 0);
    for f in ["監査.rs", "ログ.rs", "設定.rs"] {
        edit(&store, "long", &format!("/home/u/記憶/src/{f}"));
    }
    pause();
    let fresh = store.finalize_session("long").unwrap().handoff_id.unwrap();
    assert_ne!(
        fresh, rules,
        "the accepted one is not rewritten under its reader"
    );
    assert_eq!(row(&store, &rules).accepted_by.as_deref(), Some("next"));
    let pending = pending_of(&store, "long", HandoffSource::Rules);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, fresh);
}

/// SPEC-M3.5 §1: an inserted handoff supersedes the writer session's older pending ones of
/// the same source on the lane — here rules rows left by an older server, which inserted
/// one per addendum — and a re-finalize then refreshes the one pending row.
#[test]
fn inserted_rules_handoff_supersedes_the_sessions_older_pending_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "long", Some(CRYPT_LANE));
    work(&store, "long", 1);
    let h = Handoff {
        id: util::new_id(),
        project_id: project().id,
        session_id: Some("long".into()),
        source: HandoffSource::Rules,
        content_md: "## 引き継ぎ（自動生成）\n古い行".into(),
        created_at: util::fmt_ts(util::now() - chrono::Duration::days(3)),
        accepted_at: None,
        accepted_by: None,
        agent: Some("claude-code".into()),
        updated_at: None,
        lane: Some(CRYPT_LANE.into()),
    };
    let legacy = [
        h.clone(),
        Handoff {
            id: util::new_id(),
            ..h.clone()
        },
    ];
    let now = util::now_ts();
    let fresh = Handoff {
        id: util::new_id(),
        created_at: now.clone(),
        ..h
    };
    {
        let conn = store.db.lock();
        for l in &legacy {
            db::insert_handoff(&conn, l, Some(0)).unwrap();
        }
        db::insert_handoff(&conn, &fresh, Some(1)).unwrap();
        assert_eq!(db::supersede_own_pending(&conn, &fresh, &now).unwrap(), 2);
    }
    for l in &legacy {
        assert_eq!(row(&store, &l.id).accepted_by.as_deref(), Some(SUPERSEDED));
    }
    assert!(row(&store, &fresh.id).accepted_at.is_none());
    let id = store.finalize_session("long").unwrap().handoff_id.unwrap();
    assert_eq!(id, fresh.id);
    assert_eq!(pending_of(&store, "long", HandoffSource::Rules).len(), 1);
}
