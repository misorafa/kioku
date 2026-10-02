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
