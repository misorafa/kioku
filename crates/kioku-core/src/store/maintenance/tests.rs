//! SPEC-M2.8 §3 (retention, forget, sizes) and §5 (startup sweep, deferred reindex) tests.

use super::*;
use serde_json::json;

fn project() -> ProjectIdentity {
    ProjectIdentity {
        id: "kioku-3f9a1c2e".into(),
        name: "記憶".into(),
        root: "/home/u/記憶".into(),
        remote: None,
    }
}

fn start(store: &Store, session: &str) {
    store
        .start_session(&SessionStartRequest {
            machine: None,
            session_id: session.into(),
            agent: "claude-code".into(),
            cwd: "/home/u/記憶".into(),
            source: "startup".into(),
            project: project(),
            lane: None,
        })
        .unwrap();
}

/// A small session whose observations carry `ts` (an old one is a retention candidate).
fn session_at(store: &Store, id: &str, ts: &str, prompt: &str) {
    start(store, id);
    let obs = [
        (ObservationKind::Prompt, json!({"prompt": prompt})),
        (
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit", "tool_input": {"file_path": "/home/u/記憶/src/索引.rs",
                   "old_string": "古い長い本文".repeat(20), "new_string": "新しい"}, "tool_response": {}}),
        ),
        (
            ObservationKind::ToolUse,
            json!({"tool_name": "Bash", "tool_input": {"command": "git commit -m \"feat: 記憶の整理\"\nbody"},
                   "tool_response": {"stdout": "x".repeat(400), "exit_code": 1}}),
        ),
        // SPEC-M3.0 §3: a reply's stub keeps the text the digest reads
        (ObservationKind::Assistant, json!({"text": "記憶を整理しました。"})),
    ];
    for (kind, payload) in obs {
        store
            .add_observation(&NewObservation {
                event_id: None,
                session_id: id.into(),
                kind,
                ts: Some(ts.into()),
                payload,
            })
            .unwrap();
    }
    store.finalize_session(id).unwrap();
}

fn set_age(path: &Path, days: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn days_ago(days: i64) -> String {
    util::fmt_ts(util::now() - chrono::Duration::days(days))
}

fn payloads(store: &Store, session: &str) -> Vec<String> {
    let conn = store.db.lock();
    let mut stmt = conn
        .prepare("SELECT payload || text FROM observations WHERE session_id=?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([session], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// SPEC-M2.8 §3: a dry run changes nothing; a real run gzips / deletes / reduces exactly the
/// old ones; the digest of a reduced session equals its cached digest.
#[test]
fn prune_applies_retention_to_old_data_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_data_dir(tmp.path());
    cfg.retention.backups_keep = Some(1);
    let store = Store::open(cfg).unwrap();
    session_at(&store, "old", &days_ago(200), "古いセッションの指示");
    session_at(&store, "new", &days_ago(1), "新しいセッションの指示");
    let raw = |s: &str| store.dirs.raw_file(&project().id, s);
    let old_raw_text = std::fs::read_to_string(raw("old")).unwrap();
    set_age(&raw("old"), 100); // gzip
    let ancient = store.dirs.raw().join(project().id).join("ancient.jsonl");
    std::fs::write(&ancient, "{}\n").unwrap();
    set_age(&ancient, 200); // delete
    let logs = store.dirs.logs_dir();
    std::fs::write(logs.join("hook-dump.jsonl.1"), "dump\n").unwrap();
    set_age(&logs.join("hook-dump.jsonl.1"), 10);
    std::fs::write(logs.join("hook-dump.jsonl"), "dump\n").unwrap();
    let backups = tmp.path().join("backups");
    for (i, created) in ["2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z"]
        .iter()
        .enumerate()
    {
        let dir = backups.join(format!("snap{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let m = BackupManifest {
            format: 2,
            created: created.to_string(),
            path: dir.display().to_string(),
            files: Default::default(),
            counts: Default::default(),
            wiki_head: None,
        };
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
    }
    let before_digest = store.digest("old").unwrap();
    let before_old = payloads(&store, "old");
    let before_new = payloads(&store, "new");

    let dry = store.prune(true).unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.raw_gzipped.count, 1);
    assert_eq!(dry.raw_deleted.count, 1);
    assert_eq!(dry.sessions_reduced.count, 1);
    assert_eq!(dry.observations_reduced, 4);
    assert!(dry.sessions_reduced.bytes > 0);
    assert_eq!(dry.backups_removed.count, 1);
    assert_eq!(dry.hook_dumps_removed.count, 1);
    assert_eq!(
        payloads(&store, "old"),
        before_old,
        "dry run changes nothing"
    );
    assert!(raw("old").is_file() && ancient.is_file());
    assert!(backups.join("snap0").is_dir());
    assert!(logs.join("hook-dump.jsonl.1").is_file());
    assert_eq!(store.storage().unwrap().last_prune, None);

    let real = store.prune(false).unwrap();
    assert_eq!(
        (
            real.raw_gzipped.count,
            real.raw_deleted.count,
            real.sessions_reduced.count,
            real.observations_reduced,
            real.backups_removed.count,
            real.hook_dumps_removed.count
        ),
        (1, 1, 1, 4, 1, 1)
    );
    // raw: the old log is gzipped (same content), the ancient one gone, the new one kept
    assert!(!raw("old").exists());
    let mut gz = raw("old").into_os_string();
    gz.push(".gz");
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(std::fs::File::open(&gz).unwrap()),
        &mut text,
    )
    .unwrap();
    assert_eq!(text, old_raw_text);
    assert!(!ancient.exists());
    assert!(raw("new").is_file());
    // observations: the old session is stubs only, the new one untouched
    assert!(
        payloads(&store, "old")
            .iter()
            .all(|p| p.contains("\"stub\":true"))
    );
    assert!(
        !payloads(&store, "old")
            .iter()
            .any(|p| p.contains("古い長い本文"))
    );
    assert_eq!(payloads(&store, "new"), before_new);
    // the reduced session's digest: from scratch == cached == before
    let after_digest = store.digest("old").unwrap();
    assert_eq!(after_digest, before_digest);
    let cached = {
        let conn = store.db.lock();
        let session = db::get_session(&conn, "old").unwrap().unwrap();
        cached_digest(&conn, &session).unwrap()
    };
    assert_eq!(cached, after_digest);
    assert_eq!(after_digest.prompts, vec!["古いセッションの指示"]);
    assert_eq!(after_digest.git_commits, vec!["feat: 記憶の整理"]);
    assert_eq!(after_digest.errors, 1);
    assert_eq!(after_digest.last_reply.as_deref(), Some("記憶を整理しました。"));
    // backups and hook dumps
    assert!(!backups.join("snap0").exists() && backups.join("snap1").is_dir());
    assert!(!logs.join("hook-dump.jsonl.1").exists());
    assert!(logs.join("hook-dump.jsonl").is_file());
    assert_eq!(store.storage().unwrap().last_prune, Some(real.at.clone()));
    // a second run has nothing left to do
    let again = store.prune(false).unwrap();
    assert_eq!(again.sessions_reduced.count + again.raw_gzipped.count, 0);
    // a finalize of the reduced (still finalized) session keeps its page
    assert!(store.finalize_session("old").unwrap().substantive);
}

/// SPEC-M2.8 §3: `[retention] auto`/days of 0 turn a category off.
#[test]
fn zero_days_disable_a_category() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_data_dir(tmp.path());
    cfg.retention.raw_days = 0;
    cfg.retention.observations_days = 0;
    let store = Store::open(cfg).unwrap();
    session_at(&store, "old", &days_ago(400), "古い");
    set_age(&store.dirs.raw_file(&project().id, "old"), 400);
    let r = store.prune(false).unwrap();
    assert_eq!(r.raw_gzipped.count + r.raw_deleted.count, 0);
    assert_eq!(r.sessions_reduced.count, 0);
}

/// SPEC-M2.8 §3: forgetting a session removes its rows, raw log, page (with a git commit)
/// and the handoffs it authored; search no longer finds the page; STATE.md is rewritten.
#[test]
fn forget_session_removes_everything_of_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    session_at(&store, "keep", &days_ago(2), "残す作業の指示");
    session_at(&store, "drop", &days_ago(1), "忘れるべき秘密の作業");
    store
        .write_handoff(&HandoffInput {
            gotchas: Vec::new(),
            verified: Vec::new(),
            project: project().id,
            session: Some("drop".into()),
            summary: "忘れるべき引き継ぎ".into(),
            next_steps: vec![],
            open_questions: vec![],
            decisions: vec![],
        })
        .unwrap();
    store
        .add_observation(&NewObservation {
            event_id: Some("e1".into()),
            session_id: "drop".into(),
            kind: ObservationKind::Note,
            ts: None,
            payload: json!({"text": "メモ"}),
        })
        .unwrap();
    let page = session_page_path(&store.session("drop").unwrap());
    assert!(
        store
            .search("忘れるべき秘密", &SearchScope::All, 5)
            .unwrap()
            .iter()
            .any(|h| h.path == page)
    );
    let dry = store.forget_session("drop", true).unwrap();
    assert_eq!(dry.observations, 5);
    assert_eq!(dry.receipts, 1);
    assert_eq!(dry.handoffs, 2, "the rules and the agent handoff");
    assert_eq!(dry.pages, vec![page.clone()]);
    assert!(store.session("drop").is_ok(), "dry run");

    let report = store.forget_session("drop", false).unwrap();
    assert_eq!(report.sessions, vec!["drop"]);
    assert!(matches!(store.session("drop"), Err(Error::NotFound(_))));
    assert!(store.observations("drop").unwrap().is_empty());
    assert!(!tmp.path().join("wiki").join(&page).exists());
    assert!(!store.dirs.raw_file(&project().id, "drop").exists());
    let conn_counts = {
        let conn = store.db.lock();
        (
            conn.query_row(
                "SELECT COUNT(*) FROM handoffs WHERE session_id='drop'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
            conn.query_row(
                "SELECT COUNT(*) FROM observation_receipts WHERE session_id='drop'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
        )
    };
    assert_eq!(conn_counts, (0, 0));
    assert!(
        store
            .search("忘れるべき秘密", &SearchScope::All, 5)
            .unwrap()
            .is_empty()
    );
    let state = store
        .read_page(&format!("{}/STATE.md", project().id))
        .unwrap();
    assert!(state.body.contains("残す作業の指示"));
    assert!(!state.body.contains("忘れるべき"));
    if crate::git::git_available() {
        let log = crate::git::run_git(&tmp.path().join("wiki"), &["log", "--format=%s"]).unwrap();
        assert!(log.contains("kioku: forget session drop"), "{log}");
    }
    let cmds = purge_history_commands(&report.wiki_dir, &report.pages);
    assert!(
        cmds.iter()
            .any(|c| c.contains("filter-repo") && c.contains(&page))
    );
    assert!(matches!(
        store.forget_session("drop", false),
        Err(Error::NotFound(_))
    ));
}

/// SPEC-M2.8 §3: forgetting a project removes all of it, pages and aliases included.
#[test]
fn forget_project_removes_everything_of_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    session_at(&store, "a", &days_ago(1), "プロジェクト全体を消す");
    let page = store
        .write_page(&WritePageRequest {
            title: "設計メモ".into(),
            content: "消える設計の詳細".into(),
            project: Some(project().id),
            ..Default::default()
        })
        .unwrap();
    let global = store
        .write_page(&WritePageRequest {
            title: "全体メモ".into(),
            content: "残る全体の知識".into(),
            ..Default::default()
        })
        .unwrap();
    db::upsert_alias(&store.db.lock(), "old-alias", &project().id, &now_ts()).unwrap();
    let report = store.forget_project("old-alias", false).unwrap();
    assert_eq!(report.project, project().id);
    assert!(report.pages.contains(&page));
    assert!(report.pages.iter().any(|p| p.ends_with("STATE.md")));
    assert!(matches!(
        store.project(&project().id),
        Err(Error::NotFound(_))
    ));
    assert_eq!(store.resolve_project_id("old-alias").unwrap(), "old-alias");
    assert!(!tmp.path().join("wiki").join(project().id).exists());
    assert!(!store.dirs.raw().join(project().id).exists());
    assert!(
        store
            .search("消える設計", &SearchScope::All, 5)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.search("残る全体", &SearchScope::All, 5).unwrap()[0].path,
        global
    );
    let status = store.status().unwrap();
    assert_eq!(
        (status.sessions, status.observations, status.handoffs),
        (0, 0, 0)
    );
}

/// SPEC-M2.8 §3: status reports the sizes of each part of the data directory.
#[test]
fn status_reports_storage_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    session_at(&store, "s", &days_ago(0), "容量の確認");
    std::fs::create_dir_all(tmp.path().join("backups/x")).unwrap();
    std::fs::write(tmp.path().join("backups/x/f"), vec![0u8; 1000]).unwrap();
    set_age(&store.dirs.raw_file(&project().id, "s"), 3);
    let s = store.status().unwrap().storage.unwrap();
    assert!(s.db_bytes > 0 && s.raw_bytes > 0 && s.wiki_bytes > 0 && s.index_bytes > 0);
    assert_eq!(s.backups_bytes, 1000);
    let want = display_date(&days_ago(3));
    assert_eq!(s.oldest_raw.as_deref(), Some(want.as_str()));
    assert_eq!(s.last_prune, None);
    let json = serde_json::to_value(store.status().unwrap()).unwrap();
    assert!(json["storage"]["db_bytes"].as_u64().unwrap() > 0);
}

/// SPEC-M2.8 §5: the startup sweep removes `.tmp` leftovers and heals a page whose row
/// hash differs from its file.
#[test]
fn startup_sweep_removes_tmp_files_and_heals_drifted_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::for_data_dir(tmp.path());
    let store = Store::open(cfg.clone()).unwrap();
    let path = store
        .write_page(&WritePageRequest {
            title: "自己修復".into(),
            content: "古い本文".into(),
            ..Default::default()
        })
        .unwrap();
    drop(store);
    let wiki = tmp.path().join("wiki");
    let leftover = wiki.join("_global/.自己修復.md.0123abcd.tmp");
    std::fs::write(&leftover, "half written").unwrap();
    let text = std::fs::read_to_string(wiki.join(&path)).unwrap();
    std::fs::write(
        wiki.join(&path),
        text.replace("古い本文", "形態素解析で見つかる新しい本文"),
    )
    .unwrap();
    let store = Store::open(cfg).unwrap();
    assert!(!leftover.exists());
    let hits = store.search("新しい本文", &SearchScope::All, 3).unwrap();
    assert_eq!(hits[0].path, path);
    let report = store.reliability().unwrap();
    assert!(report.inconsistent_pages.is_empty(), "{report:?}");
    // a second sweep has nothing to do
    let again = store.startup_sweep().unwrap();
    assert_eq!(again, SweepReport::default());
}

/// SPEC-M2.8 §5: an outdated index is rebuilt by `reindex_if_outdated` (what the server
/// runs after it starts listening), not by `open`.
#[test]
fn outdated_index_is_rebuilt_on_request() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::for_data_dir(tmp.path());
    let store = Store::open(cfg.clone()).unwrap();
    store
        .write_page(&WritePageRequest {
            title: "索引".into(),
            content: "全角ＡＢＣの検索".into(),
            ..Default::default()
        })
        .unwrap();
    drop(store);
    std::fs::write(tmp.path().join("index/schema-version"), "1\n").unwrap();
    let store = Store::open(cfg).unwrap();
    assert!(store.index_outdated(), "open does not rebuild");
    assert_eq!(store.reindex_if_outdated().unwrap(), Some(1));
    assert_eq!(store.index_version(), INDEX_SCHEMA_VERSION);
    assert_eq!(store.reindex_if_outdated().unwrap(), None);
    assert!(
        !store
            .search("検索", &SearchScope::All, 3)
            .unwrap()
            .is_empty()
    );
}
