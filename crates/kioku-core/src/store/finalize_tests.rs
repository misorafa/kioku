//! SPEC-M2.8 §1–§2 tests: the incremental (cached) digest equals the from-scratch digest,
//! a warm finalize parses only the new observations, and a turn makes one commit (none
//! when nothing changed).

use super::*;
use serde_json::json;

fn project() -> ProjectIdentity {
    ProjectIdentity {
        id: "kensaku-3f9a1c2e".into(),
        name: "検索".into(),
        root: "/home/u/検索".into(),
        remote: None,
    }
}

fn start(store: &Store, session: &str) {
    store
        .start_session(&SessionStartRequest {
            session_id: session.into(),
            agent: "claude-code".into(),
            cwd: "/home/u/検索".into(),
            source: "startup".into(),
            project: project(),
            lane: None,
        })
        .unwrap();
}

/// The `i`-th observation of the fixture: prompts, edits, reads, commands (more than 30
/// distinct, with repeated commits), errors — Japanese prompts and paths throughout.
fn nth(session: &str, i: usize) -> NewObservation {
    let root = "/home/u/検索";
    let (kind, payload) = match i % 7 {
        0 => (
            ObservationKind::Prompt,
            json!({"prompt": format!("<system-reminder>注意</system-reminder>\n検索の改善 その{i}\n詳細")}),
        ),
        1 => (
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit",
                   "tool_input": {"file_path": format!("{root}/src/索引{}.rs", i % 5)},
                   "tool_response": {}}),
        ),
        2 => (
            ObservationKind::ToolUse,
            json!({"tool_name": "Read",
                   "tool_input": {"file_path": format!("{root}/docs/設計{}.md", i % 23)},
                   "tool_response": "本文 error は含むが失敗ではない"}),
        ),
        3 => (
            ObservationKind::ToolUse,
            json!({"tool_name": "Bash",
                   "tool_input": {"command": format!("cargo test -p 検索{}\n二行目", i % 41)},
                   "tool_response": {"stdout": "test result: ok", "exit_code": i % 3}}),
        ),
        4 => (
            ObservationKind::ToolUse,
            json!({"tool_name": "Bash",
                   "tool_input": {"command": format!("git commit -m \"feat: 検索 {}\"", i % 4)},
                   "tool_response": {"stderr": "error: 何か"}}),
        ),
        5 => (
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit",
                   "tool_input": {"file_paths": [format!("{root}/src/a.rs"), "src/日本語.rs"]},
                   "tool_response": {"is_error": i.is_multiple_of(2)}}),
        ),
        _ => (ObservationKind::Stop, json!({"stop_hook_active": false})),
    };
    NewObservation {
        event_id: None,
        session_id: session.into(),
        kind,
        ts: Some(format!(
            "2026-09-25T{:02}:{:02}:{:02}.000Z",
            2 + i / 3600,
            (i / 60) % 60,
            i % 60
        )),
        payload,
    }
}

/// The cached digest (and addendum) of a session as stored, with the agent handoff filled in.
fn cached(store: &Store, session: &str) -> (SessionDigest, Option<SessionDigest>, i64) {
    let conn = store.db.lock();
    let (json, seq) = db::digest_cache(&conn, session).unwrap().unwrap();
    let cache: DigestCache = serde_json::from_str(&json).unwrap();
    let mut full = cache.full;
    full.agent_handoff =
        db::newest_session_handoff(&conn, session, Some(HandoffSource::Agent), false).unwrap();
    (full, cache.delta.map(|d| d.digest), seq)
}

/// SPEC-M2.8 §1: finalizing a 300-observation session after every 10 observations keeps a
/// cached digest equal to the from-scratch digest at every step — including the agent
/// handoff, the counts, the timestamps, and the addendum digest after an agent handoff.
#[test]
fn incremental_digest_equals_from_scratch_at_every_step() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "step");
    for i in 0..300 {
        store.add_observation(&nth("step", i)).unwrap();
        if i == 95 || i == 211 {
            store
                .write_handoff(&HandoffInput {
                    project: project().id,
                    session: Some("step".into()),
                    summary: format!("途中経過 {i}"),
                    next_steps: vec!["索引を直す".into()],
                    open_questions: vec![],
                    decisions: vec![],
                })
                .unwrap();
        }
        if (i + 1) % 10 != 0 {
            continue;
        }
        store.finalize_session("step").unwrap();
        let (full, delta, seq) = cached(&store, "step");
        let conn = store.db.lock();
        let session = db::get_session(&conn, "step").unwrap().unwrap();
        let scratch = digest_for(&conn, &session).unwrap();
        assert_eq!(full, scratch, "after {} observations", i + 1);
        assert_eq!(seq, db::max_seq(&conn, "step").unwrap());
        assert_eq!(full.prompt_count + full.tool_use_count, {
            let c = db::session_counts(&conn, "step").unwrap();
            c.prompts + c.tool_uses
        });
        let stale = db::agent_handoff_mark(&conn, "step").unwrap().is_some()
            && db::tool_uses_since_handoff(&conn, "step").unwrap() >= HANDOFF_STALE_TOOL_USES;
        if stale {
            let mark = db::agent_handoff_mark(&conn, "step").unwrap();
            let after = db::list_observations_after(&conn, "step", mark.as_ref()).unwrap();
            let want = SessionDigest::from_observations(&after, Some("/home/u/検索"));
            assert_eq!(delta.as_ref(), Some(&want), "addendum after {}", i + 1);
        }
    }
    let (full, _, _) = cached(&store, "step");
    assert_eq!(full.prompts[0], "検索の改善 その0\n詳細");
    assert!(full.commands.len() == crate::digest::COMMANDS_MAX);
    assert_eq!(full.git_commits.len(), 4, "deduplicated");
    assert!(full.agent_handoff.is_some());
}

/// SPEC-M2.8 §1: with a warm cache, finalizing a 2,000-observation session parses only the
/// observations added since the last finalize.
#[test]
fn warm_finalize_of_a_long_session_parses_only_new_observations() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "long");
    {
        let mut conn = store.db.lock();
        let tx = conn.transaction().unwrap();
        for i in 0..2000 {
            let o = nth("long", i);
            db::insert_observation(
                &tx,
                "long",
                &project().id,
                o.kind,
                o.ts.as_deref().unwrap(),
                &o.payload.to_string(),
                "",
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    store.finalize_session("long").unwrap(); // cold: digests from scratch once
    for i in 2000..2005 {
        store.add_observation(&nth("long", i)).unwrap();
    }
    db::PARSED_OBSERVATIONS.with(|c| c.set(0));
    store.finalize_session("long").unwrap();
    let parsed = db::PARSED_OBSERVATIONS.with(|c| c.get());
    assert!(parsed <= 10, "parsed {parsed} observations");
    let (full, _, _) = cached(&store, "long");
    assert_eq!(full.tool_use_count + full.prompt_count, {
        let c = db::session_counts(&store.db.lock(), "long").unwrap();
        c.prompts + c.tool_uses
    });
}

fn git(wiki: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(wiki)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commits(wiki: &Path) -> u32 {
    git(wiki, &["rev-list", "--count", "HEAD"]).parse().unwrap()
}

/// SPEC-M2.8 §2: a turn makes one commit with the session page and STATE.md; finalizing
/// again without new observations makes none; an unchanged STATE.md is not touched.
#[test]
fn one_commit_per_turn_and_none_without_changes() {
    if !crate::git::git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    let wiki = tmp.path().join("wiki");
    start(&store, "turns");
    for i in 0..4 {
        store.add_observation(&nth("turns", i)).unwrap();
    }
    store.finalize_session("turns").unwrap();
    let after_first = commits(&wiki);
    let files = git(&wiki, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.lines().count(), 2, "{files}");
    assert!(
        files.contains("STATE.md") && files.contains("/sessions/"),
        "{files}"
    );

    // No new observations: no commit, twice.
    store.finalize_session("turns").unwrap();
    store.finalize_session("turns").unwrap();
    assert_eq!(commits(&wiki), after_first);

    // New observations: exactly one commit containing both files.
    store.add_observation(&nth("turns", 4)).unwrap(); // a git commit (new handoff text)
    store.finalize_session("turns").unwrap();
    assert_eq!(commits(&wiki), after_first + 1);
    let files = git(&wiki, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.lines().count(), 2, "{files}");
    assert!(
        files.contains("STATE.md") && files.contains("/sessions/"),
        "{files}"
    );

    // An agent handoff, then a turn that only reads: the session page changes, STATE.md
    // (agent handoff, titles, edited files) does not and is left alone.
    store
        .write_handoff(&HandoffInput {
            project: project().id,
            session: Some("turns".into()),
            summary: "検索の索引を直した".into(),
            next_steps: vec![],
            open_questions: vec![],
            decisions: vec![],
        })
        .unwrap();
    store.add_observation(&nth("turns", 7)).unwrap(); // a prompt (reopens the session)
    store.finalize_session("turns").unwrap();
    let state_file = wiki.join(format!("{}/STATE.md", project().id));
    let state_before = std::fs::read_to_string(&state_file).unwrap();
    let count = commits(&wiki);
    store.add_observation(&nth("turns", 9)).unwrap(); // a Read
    store.finalize_session("turns").unwrap();
    assert_eq!(commits(&wiki), count + 1);
    let files = git(&wiki, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(!files.contains("STATE.md"), "{files}");
    assert_eq!(std::fs::read_to_string(&state_file).unwrap(), state_before);
}
