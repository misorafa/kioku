//! Tests of the index: SPEC-M1 §6.4 Japanese cases, snippets, rebuilds, and the SPEC-M3.1 §2
//! identifier field, re-ranking, filters, partial matches, user dictionary and index switch.

use super::*;

fn doc(path: &str, project: Option<&str>, title: &str, body: &str) -> IndexDoc {
    IndexDoc {
        path: path.to_string(),
        project_id: project.map(str::to_string),
        scope: if project.is_some() {
            "project"
        } else {
            "global"
        }
        .to_string(),
        kind: "page".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        tags: vec!["rust".to_string()],
        updated: Utc::now(),
        machine: None,
    }
}

fn fixture() -> (tempfile::TempDir, SearchIndex) {
    let dir = tempfile::tempdir().unwrap();
    let (idx, fresh) = SearchIndex::open(dir.path()).unwrap();
    assert!(fresh);
    idx.upsert_many(
        &[
            doc(
                "p1/pages/a.md",
                Some("p1"),
                "メモA",
                "引き継ぎ書を毎回作るのが手間なので自動化したい",
            ),
            doc(
                "p1/pages/b.md",
                Some("p1"),
                "メモB",
                "Flutterでコードチャートのアプリを作っている",
            ),
            doc(
                "_global/c.md",
                None,
                "メモC",
                "k3sクラスタにWireGuardで自宅サーバーを参加させた",
            ),
        ],
        false,
    )
    .unwrap();
    (dir, idx)
}

fn paths(idx: &SearchIndex, q: &str) -> Vec<String> {
    idx.search(q, &SearchScope::All, 10)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect()
}

#[test]
fn japanese_case_1_handoff_sentence() {
    let (_d, idx) = fixture();
    assert!(paths(&idx, "引き継ぎ").contains(&"p1/pages/a.md".to_string()));
    assert!(paths(&idx, "手間").contains(&"p1/pages/a.md".to_string()));
    // 「引継」: SPEC-M1 §6.4 left it open; SPEC-M3.1 §2's bigram fallback finds it as a
    // partial match (the kanji skeleton of 引き継ぎ書 is 引継書).
    let hits = idx.search("引継", &SearchScope::All, 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].path, "p1/pages/a.md");
    assert!(hits[0].partial);
}

#[test]
fn japanese_case_2_ascii_inside_japanese() {
    let (_d, idx) = fixture();
    let b = "p1/pages/b.md".to_string();
    assert!(paths(&idx, "Flutter").contains(&b));
    assert!(paths(&idx, "flutter").contains(&b));
    assert!(paths(&idx, "アプリ").contains(&b));
}

#[test]
fn japanese_case_3_compound_and_absent() {
    let (_d, idx) = fixture();
    let c = "_global/c.md".to_string();
    assert_eq!(paths(&idx, "自宅サーバー").first(), Some(&c));
    assert!(paths(&idx, "WireGuard").contains(&c));
    assert!(paths(&idx, "Postgres").is_empty());
}

#[test]
fn japanese_case_4_title_boost() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    idx.upsert_many(
        &[
            doc(
                "p/pages/body.md",
                Some("p"),
                "雑多なメモ",
                "今日は全文検索の設計について考えた。索引の構造も検討した。",
            ),
            doc(
                "p/pages/title.md",
                Some("p"),
                "全文検索",
                "今日は設計について考えた。索引の構造も検討した。",
            ),
        ],
        false,
    )
    .unwrap();
    let hits = idx.search("全文検索", &SearchScope::All, 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].path, "p/pages/title.md");
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn scope_filters_and_global_flag() {
    let (_d, idx) = fixture();
    idx.upsert(&doc(
        "p2/pages/x.md",
        Some("p2"),
        "他プロジェクト",
        "自宅サーバーの別メモ",
    ))
    .unwrap();
    let hits = idx
        .search("自宅サーバー", &SearchScope::Project("p1".into()), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "_global/c.md");
    assert!(hits[0].global);
    let hits = idx
        .search("自宅サーバー", &SearchScope::Global, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    let hits = idx.search("自宅サーバー", &SearchScope::All, 10).unwrap();
    assert_eq!(hits.len(), 2);
    let hits = idx
        .search("引き継ぎ", &SearchScope::Project("p2".into()), 10)
        .unwrap();
    assert!(hits.is_empty());
}

#[test]
fn snippet_is_plain_with_brackets() {
    let (_d, idx) = fixture();
    let hits = idx.search("手間", &SearchScope::All, 10).unwrap();
    assert!(hits[0].snippet.contains("【手間】"), "{}", hits[0].snippet);
    assert!(!hits[0].snippet.contains("<b>"));
    assert!(!hits[0].updated.is_empty());
}

/// Regression (SPEC-M2.8 §5): a deletion followed by a clearing rebuild that adds the
/// same path back must leave that document in the index.
#[test]
fn delete_then_rebuild_keeps_the_rebuilt_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(tmp.path()).unwrap();
    let a = doc("p/a.md", Some("p"), "検索", "日本語の本文");
    idx.upsert(&a).unwrap();
    idx.delete_paths(&["p/a.md".to_string()]).unwrap();
    assert_eq!(idx.num_docs(), 0);
    idx.upsert_many(std::slice::from_ref(&a), true).unwrap();
    assert_eq!(idx.num_docs(), 1);
    assert_eq!(idx.search("日本語", &SearchScope::All, 3).unwrap().len(), 1);
    // The startup sweep followed by the session page migration's rebuild.
    let (b, c, s) = (
        doc("p/b.md", Some("p"), "b", "b"),
        doc("p/c.md", Some("p"), "c", "c"),
        doc("p/STATE.md", Some("p"), "s", "s"),
    );
    idx.upsert_many(&[a.clone(), b.clone(), s.clone()], false)
        .unwrap();
    idx.upsert_many(std::slice::from_ref(&c), false).unwrap();
    idx.delete_paths(&["p/a.md".to_string(), "p/b.md".to_string()])
        .unwrap();
    idx.upsert_many(&[s.clone(), a.clone()], true).unwrap();
    assert_eq!(idx.num_docs(), 2);
    // The same on a freshly opened index (a restart).
    idx.upsert_many(&[a.clone(), b.clone(), s.clone()], true)
        .unwrap();
    drop(idx);
    let (idx, _) = SearchIndex::open(tmp.path()).unwrap();
    idx.upsert_many(std::slice::from_ref(&c), false).unwrap();
    idx.delete_paths(&["p/a.md".to_string(), "p/b.md".to_string()])
        .unwrap();
    idx.upsert_many(&[s, a], true).unwrap();
    assert_eq!(idx.num_docs(), 2);
}

/// Regression (Windows CI, "Access is denied", os error 5): a commit renames the new
/// `meta.json` over the old one, which Windows refuses while anything holds it open. Reopen
/// → delete → clearing write → rebuild, plus the switch away from an older index, with a
/// searcher kept alive and other threads searching and writing meanwhile.
#[test]
fn reopen_delete_rebuild_with_a_live_searcher() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, current) = (tmp.path().join("old"), tmp.path().join("new"));
    let a = doc(
        "p/a.md",
        Some("p"),
        "引き継ぎ",
        "日本語の本文と自宅サーバー",
    );
    let s = doc("p/STATE.md", Some("p"), "状態", "現在の状態");
    SearchIndex::open(&legacy).unwrap().0.upsert(&a).unwrap();
    for round in 0..4 {
        let (idx, _) =
            SearchIndex::open_with(&current, std::slice::from_ref(&legacy), None).unwrap();
        let idx = Arc::new(idx);
        assert_eq!(idx.serving_legacy(), round == 0);
        // Holds the segments it saw (mmapped) until the end of the round.
        let searcher = idx.inner().reader.searcher();
        let seen = searcher.num_docs();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (idx, stop) = (idx.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    idx.search("日本語", &SearchScope::All, 5).unwrap();
                }
            })
        };
        if round > 0 {
            let writer = {
                let idx = idx.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        let path = format!("p/w{i}.md");
                        idx.upsert(&doc(&path, Some("p"), "並行", "同時に書く"))
                            .unwrap();
                    }
                })
            };
            idx.delete_paths(&["p/a.md".to_string()]).unwrap();
            idx.upsert_many(std::slice::from_ref(&a), true).unwrap();
            writer.join().unwrap();
        }
        // Round 0 builds the new index and switches to it; later rounds clear and refill.
        idx.rebuild(&[a.clone(), s.clone()]).unwrap();
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert!(!idx.serving_legacy());
        assert_eq!(idx.num_docs(), 2);
        let hits = idx.search("日本語", &SearchScope::All, 5).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, "p/a.md");
        // The old snapshot still answers from the segments it holds.
        assert_eq!(searcher.num_docs(), seen);
        assert_eq!(
            searcher
                .search(&AllQuery, &tantivy::collector::Count)
                .unwrap() as u64,
            seen
        );
        drop(searcher);
        // Once nothing maps it, the older directory goes (on Windows too).
        idx.remove_legacy_dirs();
        assert!(!legacy.exists(), "round {round}");
    }
}

/// Nothing in the background opens `meta.json`: the reader reloads only after kioku's own
/// commits (one made elsewhere stays unseen), so no poll can collide with a commit's rename.
#[test]
fn reader_reloads_only_after_its_own_commits() {
    let tmp = tempfile::tempdir().unwrap();
    SearchIndex::open(tmp.path())
        .unwrap()
        .0
        .upsert(&doc("p/a.md", Some("p"), "検索", "日本語の本文"))
        .unwrap();
    let (idx, _) = SearchIndex::open(tmp.path()).unwrap();
    assert_eq!(idx.num_docs(), 1);
    let raw = Index::open_in_dir(tmp.path()).unwrap();
    let mut w: IndexWriter = raw.writer_with_num_threads(1, WRITER_HEAP).unwrap();
    w.delete_all_documents().unwrap();
    w.commit().unwrap();
    w.wait_merging_threads().unwrap();
    // Longer than tantivy's 500 ms meta.json poll.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert_eq!(idx.num_docs(), 1, "no background reload");
    assert_eq!(idx.search("日本語", &SearchScope::All, 3).unwrap().len(), 1);
}

#[test]
fn upsert_replaces_and_syntax_fallback() {
    let (_d, idx) = fixture();
    assert_eq!(idx.num_docs(), 3);
    idx.upsert(&doc(
        "p1/pages/a.md",
        Some("p1"),
        "メモA",
        "内容を差し替えた",
    ))
    .unwrap();
    assert_eq!(idx.num_docs(), 3);
    assert!(paths(&idx, "手間").is_empty());
    // explicit syntax: phrase + field
    assert_eq!(paths(&idx, "\"自宅サーバー\""), vec!["_global/c.md"]);
    assert_eq!(paths(&idx, "title:メモB"), vec!["p1/pages/b.md"]);
    // broken syntax falls back to terms instead of erroring
    assert!(paths(&idx, "WireGuard AND (").contains(&"_global/c.md".to_string()));
    // tags match verbatim
    assert_eq!(paths(&idx, "rust").len(), 3);
    idx.upsert_many(&[], true).unwrap();
    assert_eq!(idx.num_docs(), 0);
}

#[test]
fn snippet_length_is_counted_in_chars() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    let body = format!(
        "{}引き継ぎの自動化について{}",
        "日本語の長い前置き。".repeat(30),
        "後続の説明文。".repeat(60)
    );
    idx.upsert(&doc("p/pages/long.md", Some("p"), "長文", &body))
        .unwrap();
    let hits = idx.search("自動化", &SearchScope::All, 10).unwrap();
    let n = hits[0].snippet.chars().count();
    assert!(n <= SNIPPET_MAX, "{n}");
    assert!(
        n > 150,
        "a 200-char snippet, not 200 bytes (~66 chars): {n}"
    );
    assert!(
        hits[0].snippet.contains("【自動化】"),
        "{}",
        hits[0].snippet
    );
}

#[test]
fn nfkc_folds_full_width_and_half_width() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    idx.upsert(&doc(
        "p/pages/w.md",
        Some("p"),
        "全角メモ",
        "Ｆｌｕｔｔｅｒ ｱﾌﾟﾘ を作った",
    ))
    .unwrap();
    for q in ["flutter", "Flutter", "アプリ", "ｱﾌﾟﾘ", "Ｆｌｕｔｔｅｒ"] {
        assert_eq!(paths(&idx, q), vec!["p/pages/w.md"], "{q}");
    }
    let hits = idx.search("アプリ", &SearchScope::All, 10).unwrap();
    assert!(hits[0].snippet.contains("【ｱﾌﾟﾘ】"), "{}", hits[0].snippet);
}

#[test]
fn tokenizer_segments_japanese() {
    let toks = tokenize("自宅サーバーをWireGuardで").unwrap();
    assert!(toks.contains(&"自宅".to_string()), "{toks:?}");
    assert!(toks.contains(&"wireguard".to_string()), "{toks:?}");
}

/// The review of 2026-11-20 (SPEC-M3.6 §3): a page about the comparison next to pages that
/// share one incidental token each (`X` ↔ `x86_64`, 性 ↔ 脆弱性, 投稿 alone).
fn noise_fixture() -> (tempfile::TempDir, SearchIndex) {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    idx.upsert_many(
        &[
            doc(
                "p/compete.md",
                Some("p"),
                "claude-mem との競合比較",
                "競合の claude-mem と比較した。優位性は日本語検索と自己ホスト。宣伝として X に投稿する。",
            ),
            doc(
                "p/x86.md",
                Some("p"),
                "x86_64 向けビルド",
                "x86_64 の Linux バイナリを musl でビルドする。",
            ),
            doc(
                "p/vuln.md",
                Some("p"),
                "依存クレートの脆弱性",
                "cargo audit で脆弱性を確認した。",
            ),
            doc(
                "p/blog.md",
                Some("p"),
                "ブログ",
                "週末にブログへ投稿した。",
            ),
            doc(
                "p/flutter.md",
                Some("p"),
                "Flutter のアプリ",
                "Flutter で画面を作った。",
            ),
        ],
        false,
    )
    .unwrap();
    (dir, idx)
}

#[test]
fn one_letter_tokens_are_noise_unless_alone() {
    for t in ["x", "1", "を", "-"] {
        assert!(is_noise_token(t), "{t}");
    }
    for t in ["x86", "性", "競合", "ab"] {
        assert!(!is_noise_token(t), "{t}");
    }
    let (_d, idx) = noise_fixture();
    // `X` alone still searches `x`
    let alone = paths(&idx, "X");
    assert!(alone.contains(&"p/compete.md".to_string()), "{alone:?}");
}

#[test]
fn observed_query_needs_two_tokens() {
    let (_d, idx) = noise_fixture();
    let hits = idx
        .search(
            "競合 比較 claude-mem 優位性 宣伝 X 投稿",
            &SearchScope::All,
            8,
        )
        .unwrap();
    let got: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    // the comparison page (the only two-token hit) first; one-token hits only after it
    assert_eq!(got[0], "p/compete.md", "{got:?}");
    // `X` is noise next to the other words: the x86_64 page is not found at all
    assert!(!got.contains(&"p/x86.md"), "{got:?}");
    assert!(!got.contains(&"p/flutter.md"), "{got:?}");
    assert!(hits.iter().all(|h| !h.partial), "{got:?}");
}

/// The review of M3.6: a page matching one token must not be lost because another page
/// matches two — it comes after it (「winget トークン 再発行」).
#[test]
fn one_token_hits_follow_two_token_hits() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    idx.upsert_many(
        &[
            doc(
                "p/reissue.md",
                Some("p"),
                "認証",
                "API のトークンを再発行する手順。",
            ),
            doc(
                "p/winget.md",
                Some("p"),
                "公開",
                "WINGET_TOKEN を作り直す。",
            ),
            doc("p/other.md", Some("p"), "夕食", "牛乳と卵を買う。"),
        ],
        false,
    )
    .unwrap();
    let hits = idx
        .search("winget トークン 再発行", &SearchScope::All, 5)
        .unwrap();
    let got: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(got, ["p/reissue.md", "p/winget.md"]);
    assert!(hits.iter().all(|h| !h.partial));
}

/// When two-token hits already fill `limit`, no one-token hit is appended.
#[test]
fn a_full_first_tier_gets_no_one_token_hits() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, _) = SearchIndex::open(dir.path()).unwrap();
    idx.upsert_many(
        &[
            doc("p/a.md", Some("p"), "検索の改善", "索引と検索を速くした。"),
            doc(
                "p/b.md",
                Some("p"),
                "索引の再構築",
                "索引を作り直して検索を確かめた。",
            ),
            doc("p/c.md", Some("p"), "日本語", "形態素解析の辞書を足した。"),
        ],
        false,
    )
    .unwrap();
    let q = "検索 索引 辞書";
    let mut two: Vec<String> = idx
        .search(q, &SearchScope::All, 2)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect();
    two.sort();
    assert_eq!(two, ["p/a.md", "p/b.md"]);
    // with room, the one-token page follows
    let all = paths(&idx, q);
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all[2], "p/c.md");
}

#[test]
fn long_query_with_one_shared_token_falls_back_to_any_one() {
    let (_d, idx) = noise_fixture();
    let hits = idx
        .search("Flutter 移行 検討 費用 見積もり", &SearchScope::All, 5)
        .unwrap();
    let got: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(got, ["p/flutter.md"]);
    assert!(!hits[0].partial, "the any-one retry is not a partial match");
    // a two-token query is not subject to coverage: either token is enough
    let mut two = paths(&idx, "投稿 ビルド");
    two.sort();
    assert_eq!(two, ["p/blog.md", "p/compete.md", "p/x86.md"]);
}
