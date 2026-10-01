//! Deterministic Japanese retrieval evaluation; run with `cargo test -p kioku-core search_evaluation -- --nocapture`.

use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Corpus {
    pages: Vec<EvalPage>,
    queries: Vec<EvalQuery>,
}
#[derive(Deserialize)]
struct EvalPage {
    id: String,
    title: String,
    body: String,
}
#[derive(Deserialize)]
struct EvalQuery {
    q: String,
    relevant: Vec<String>,
    /// The first hit must be relevant (queries next to a near-duplicate distractor).
    #[serde(default)]
    top: bool,
}

#[test]
fn search_evaluation_recall_at_3_and_mrr() {
    let corpus: Corpus = serde_json::from_str(include_str!("search-eval.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let (index, _) = SearchIndex::open(tmp.path()).unwrap();
    let docs: Vec<_> = corpus
        .pages
        .into_iter()
        .map(|p| IndexDoc {
            path: p.id,
            title: p.title,
            body: p.body,
            project_id: Some("eval".into()),
            scope: "project".into(),
            kind: "page".into(),
            tags: vec![],
            updated: crate::util::parse_ts("2026-09-30T00:00:00Z").unwrap(),
        })
        .collect();
    index.upsert_many(&docs, true).unwrap();
    let mut recall = 0.0;
    let mut mrr = 0.0;
    for query in &corpus.queries {
        let hits = index
            .search(&query.q, &SearchScope::Project("eval".into()), 3)
            .unwrap();
        let relevant = hits
            .iter()
            .filter(|h| query.relevant.contains(&h.path))
            .count();
        recall += relevant as f64 / query.relevant.len() as f64;
        mrr += hits
            .iter()
            .position(|h| query.relevant.contains(&h.path))
            .map(|i| 1.0 / (i + 1) as f64)
            .unwrap_or(0.0);
        assert!(relevant > 0, "no relevant result for {}: {hits:?}", query.q);
        if query.top {
            assert!(
                hits.first()
                    .is_some_and(|h| query.relevant.contains(&h.path)),
                "{} must rank a relevant page first: {hits:?}",
                query.q
            );
        }
    }
    recall /= corpus.queries.len() as f64;
    mrr /= corpus.queries.len() as f64;
    println!(
        "Japanese search evaluation: {} queries, Recall@3={recall:.3}, MRR@3={mrr:.3}",
        corpus.queries.len()
    );
    assert!(recall >= 0.90, "Recall@3={recall}");
    assert!(mrr >= 0.75, "MRR@3={mrr}");
}
