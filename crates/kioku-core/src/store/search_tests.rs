//! SPEC-M3.1 §2–§3 tests on the store: filtered search, the switch from a schema-2 index
//! (served until the rebuild), the user dictionary file and its staleness flag, and the
//! sessions that touched a path.

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

fn start(store: &Store, session: &str, machine: Option<&str>) {
    store
        .start_session(&SessionStartRequest {
            session_id: session.into(),
            agent: "claude-code".into(),
            cwd: "/home/u/記憶".into(),
            source: "startup".into(),
            project: project(),
            lane: None,
            machine: machine.map(str::to_string),
        })
        .unwrap();
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

fn edit(store: &Store, session: &str, prompt: &str, files: &[&str]) {
    observe(
        store,
        session,
        ObservationKind::Prompt,
        json!({ "prompt": prompt }),
    );
    for f in files {
        observe(
            store,
            session,
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit", "tool_input": {"file_path": format!("/home/u/記憶/{f}")}, "tool_response": {}}),
        );
    }
}

fn page(store: &Store, title: &str, content: &str, tags: &[&str]) -> String {
    store.register_project(&project()).unwrap();
    store
        .write_page(&WritePageRequest {
            title: title.into(),
            content: content.into(),
            project: Some(project().id),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn sessions_for_a_path_newest_first_with_titles_and_summaries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    start(&store, "s-old", Some("mini"));
    edit(
        &store,
        "s-old",
        "ストアの書き込みロックを直す",
        &["crates/kioku-core/src/store.rs", "README.md"],
    );
    store.finalize_session("s-old").unwrap();
    start(&store, "s-new", None);
    edit(
        &store,
        "s-new",
        "索引の再構築を速くする",
        &[
            "crates/kioku-core/src/store.rs",
            "crates/kioku-core/src/index.rs",
        ],
    );
    store
        .write_handoff(&HandoffInput {
            project: project().id,
            session: Some("s-new".into()),
            summary: "再構築を spawn_blocking に移した".into(),
            ..HandoffInput::default()
        })
        .unwrap();
    store.finalize_session("s-new").unwrap();
    start(&store, "s-other", None);
    edit(
        &store,
        "s-other",
        "ドキュメントだけ",
        &["docs/SPEC-M3.1.md"],
    );
    store.finalize_session("s-other").unwrap();

    let got = store
        .sessions_for_path("crates/kioku-core/src/store.rs", None, 10)
        .unwrap();
    let ids: Vec<&str> = got.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, ["s-new", "s-old"]);
    assert!(got[0].title.contains("索引の再構築"), "{:?}", got[0]);
    assert_eq!(
        got[0].summary.as_deref(),
        Some("再構築を spawn_blocking に移した")
    );
    assert_eq!(got[0].files, ["crates/kioku-core/src/store.rs"]);
    assert!(got[1].title.contains("書き込みロック"), "{:?}", got[1]);
    assert!(got[1].summary.is_some(), "the rules handoff's summary");
    assert_eq!(got[1].machine.as_deref(), Some("mini"));
    assert!(got[1].path.ends_with(".md"));

    // a directory prefix, a Windows-style prefix, a project filter, a limit
    let dir = store
        .sessions_for_path(".\\crates\\kioku-core", Some(&project().id), 10)
        .unwrap();
    assert_eq!(dir.len(), 2);
    assert_eq!(dir[0].files.len(), 2);
    assert_eq!(
        store
            .sessions_for_path("crates/kioku-core/", None, 1)
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .sessions_for_path("crates/kioku-cli", None, 10)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        store.sessions_for_path("  ", None, 10),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn since_and_kinds_filter_and_session_hits_carry_the_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
    page(
        &store,
        "検索の設計",
        "全文検索の設計メモ。lindera を使う。",
        &[],
    );
    start(&store, "s-1", Some("studio"));
    edit(
        &store,
        "s-1",
        "全文検索の設計を見直す",
        &["crates/kioku-core/src/index.rs"],
    );
    store.finalize_session("s-1").unwrap();
    let pid = SearchScope::Project(project().id);
    let all = store.search("全文検索", &pid, 10).unwrap();
    let kinds: Vec<&str> = all.iter().map(|h| h.kind.as_str()).collect();
    assert!(
        kinds.contains(&"page") && kinds.contains(&"session"),
        "{kinds:?}"
    );
    let session = all.iter().find(|h| h.kind == "session").unwrap();
    assert_eq!(session.machine.as_deref(), Some("studio"));

    let opts = SearchOptions::parse(None, &["session".into()]).unwrap();
    let only = store.search_with("全文検索", &pid, 10, &opts).unwrap();
    assert!(only.iter().all(|h| h.kind == "session") && !only.is_empty());
    let future = SearchOptions::parse(Some("2999-01-01"), &[]).unwrap();
    assert!(
        store
            .search_with("全文検索", &pid, 10, &future)
            .unwrap()
            .is_empty()
    );
    let past = SearchOptions::parse(Some("2000-01-01"), &[]).unwrap();
    assert_eq!(
        store
            .search_with("全文検索", &pid, 10, &past)
            .unwrap()
            .len(),
        all.len()
    );
    assert!(SearchOptions::parse(Some("昨日"), &[]).is_err());
    assert!(SearchOptions::parse(None, &["memo".into()]).is_err());
}

/// The index of schema 2 (`index/tantivy`, no `code` / `ja_bigram` fields), as v0.9 built it.
fn write_schema2_index(dir: &Path, docs: &[IndexDoc]) {
    use tantivy::schema::{
        DateOptions, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    };
    std::fs::create_dir_all(dir).unwrap();
    let mut b = Schema::builder();
    let ja = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("ja")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let raw = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored();
    let path = b.add_text_field("path", STRING | STORED);
    let project_id = b.add_text_field("project_id", STRING);
    let scope = b.add_text_field("scope", STRING);
    let kind = b.add_text_field("kind", STRING | STORED);
    let title = b.add_text_field("title", ja.clone());
    let body = b.add_text_field("body", ja);
    let tags = b.add_text_field("tags", raw);
    let updated = b.add_date_field("updated", DateOptions::default().set_fast().set_stored());
    let index = tantivy::Index::create_in_dir(dir, b.build()).unwrap();
    index
        .tokenizers()
        .register("ja", crate::index::ja_analyzer().unwrap());
    let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 50_000_000).unwrap();
    for d in docs {
        let mut doc = tantivy::TantivyDocument::default();
        doc.add_text(path, &d.path);
        if let Some(p) = &d.project_id {
            doc.add_text(project_id, p);
        }
        doc.add_text(scope, &d.scope);
        doc.add_text(kind, &d.kind);
        doc.add_text(title, &d.title);
        doc.add_text(body, &d.body);
        for t in &d.tags {
            doc.add_text(tags, t);
        }
        doc.add_date(
            updated,
            tantivy::DateTime::from_timestamp_secs(d.updated.timestamp()),
        );
        w.add_document(doc).unwrap();
    }
    w.commit().unwrap();
}

/// SPEC-M3.1 §2 + SPEC-M2.8 §5: after an upgrade the schema-2 index keeps answering
/// searches until the rebuild (what the server runs after listening) switches to the
/// schema-3 one in its own directory; the old directory is then removed.
#[test]
fn schema2_index_is_served_until_the_rebuild_switches_over() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::for_data_dir(tmp.path());
    let dirs = DataDir::new(tmp.path());
    let docs = {
        let store = Store::open(cfg.clone()).unwrap();
        page(
            &store,
            "書き込みロック",
            "write_lock は Store::open で作る。src/index.rs の索引も同じロックの下で書く。",
            &[],
        );
        let wiki = dirs.wiki();
        list_wiki_pages(&wiki)
            .unwrap()
            .into_iter()
            .map(|rel| {
                let text = std::fs::read_to_string(wiki.join(&rel)).unwrap();
                page_records(&Page::parse(&rel, &text).unwrap(), &text).1
            })
            .collect::<Vec<_>>()
    };
    // what v0.9 left behind
    std::fs::remove_dir_all(dirs.index_dir()).unwrap();
    write_schema2_index(&dirs.index_dir_for(2), &docs);
    std::fs::write(dirs.index_version_file(), "2\n").unwrap();

    let store = Store::open(cfg.clone()).unwrap();
    assert!(store.index_outdated());
    assert!(store.index.serving_legacy());
    let hits = store
        .search("書き込みロック", &SearchScope::All, 5)
        .unwrap();
    assert_eq!(hits.len(), 1, "the old index still answers");
    assert!(dirs.index_dir_for(2).join("meta.json").exists());

    assert_eq!(store.reindex_if_outdated().unwrap(), Some(1));
    assert!(!store.index_outdated());
    assert!(!store.index.serving_legacy());
    assert_eq!(store.index_version(), INDEX_SCHEMA_VERSION);
    assert!(dirs.index_dir().join("meta.json").exists());
    assert!(!dirs.index_dir_for(2).exists(), "old index removed");
    // the schema-3 fields work: identifiers by their parts
    for q in ["write_lock", "lock", "Store::open", "index.rs"] {
        let hits = store.search(q, &SearchScope::All, 5).unwrap();
        assert_eq!(hits.len(), 1, "{q}");
    }
    drop(store);
    // a restart opens the new index directly
    let store = Store::open(cfg).unwrap();
    assert!(!store.index_outdated());
    assert_eq!(store.search("索引", &SearchScope::All, 5).unwrap().len(), 1);
}

#[test]
fn user_dictionary_is_used_and_doctor_sees_it_change() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::for_data_dir(tmp.path());
    let dirs = DataDir::new(tmp.path());
    std::fs::create_dir_all(dirs.user_dict_file().parent().unwrap()).unwrap();
    std::fs::write(
        dirs.user_dict_file(),
        "# テスト辞書\n記憶装置,-10000,名詞,キオクソウチ,kioku\n",
    )
    .unwrap();
    let store = Store::open(cfg.clone()).unwrap();
    page(&store, "製品名", "kioku はエージェントの共有メモリ", &[]);
    page(&store, "別のメモ", "記憶装置の話", &[]);
    // the synonym brings both pages for either word
    for q in ["記憶装置", "kioku"] {
        assert_eq!(
            store.search(q, &SearchScope::All, 5).unwrap().len(),
            2,
            "{q}"
        );
    }
    assert!(!store.status().unwrap().user_dict_stale);
    // editing the file makes the index stale until a reindex
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(dirs.user_dict_file(), "記憶装置,-10000,名詞,キオクソウチ\n").unwrap();
    assert!(store.status().unwrap().user_dict_stale);
    store.reindex().unwrap();
    assert!(!store.status().unwrap().user_dict_stale);
    assert_eq!(
        store.search("kioku", &SearchScope::All, 5).unwrap().len(),
        1
    );
}
