//! Deterministic Japanese retrieval evaluation (SPEC-M3.1 §2): ≥ 50 pages and ≥ 50 queries
//! over identifiers, mixed Japanese/English, half-width katakana, user-dictionary synonyms,
//! old vs new duplicates, pinned pages, page vs session kinds and the bigram fallback. Run
//! with `cargo test -p kioku-core search_evaluation -- --nocapture` to see the numbers.

use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Corpus {
    now: String,
    user_dict: String,
    pages: Vec<EvalPage>,
    queries: Vec<EvalQuery>,
}
#[derive(Deserialize)]
struct EvalPage {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}
#[derive(Deserialize)]
struct EvalQuery {
    q: String,
    relevant: Vec<String>,
    /// The first hit must be relevant (queries next to a near-duplicate distractor).
    #[serde(default)]
    top: bool,
    /// Only the bigram fallback finds it: the hits must be flagged `partial`.
    #[serde(default)]
    partial: bool,
}

#[test]
fn search_evaluation_recall_at_3_and_mrr() {
    let corpus: Corpus = serde_json::from_str(include_str!("search-eval.json")).unwrap();
    assert!(corpus.pages.len() >= 50, "{} pages", corpus.pages.len());
    assert!(
        corpus.queries.len() >= 50,
        "{} queries",
        corpus.queries.len()
    );
    let tmp = tempfile::tempdir().unwrap();
    let dict = tmp.path().join("user.csv");
    std::fs::write(&dict, &corpus.user_dict).unwrap();
    let (index, _) = SearchIndex::open_with(&tmp.path().join("index"), &[], Some(&dict)).unwrap();
    let default_updated = crate::util::parse_ts("2026-09-30T00:00:00Z").unwrap();
    let docs: Vec<_> = corpus
        .pages
        .into_iter()
        .map(|p| IndexDoc {
            path: p.id,
            title: p.title,
            body: p.body,
            project_id: Some("eval".into()),
            scope: "project".into(),
            kind: p.kind.unwrap_or_else(|| "page".into()),
            tags: p.tags,
            updated: p
                .updated
                .as_deref()
                .and_then(crate::util::parse_ts)
                .unwrap_or(default_updated),
            machine: None,
        })
        .collect();
    index.upsert_many(&docs, true).unwrap();
    let opts = SearchOptions {
        now: crate::util::parse_ts(&corpus.now),
        ..SearchOptions::default()
    };
    let scope = SearchScope::Project("eval".into());
    let mut recall = 0.0;
    let mut mrr = 0.0;
    let mut failures = Vec::new();
    let mut slowest = std::time::Duration::ZERO;
    for query in &corpus.queries {
        let started = std::time::Instant::now();
        let hits = index.search_with(&query.q, &scope, 3, &opts).unwrap();
        slowest = slowest.max(started.elapsed());
        let relevant = hits
            .iter()
            .filter(|h| query.relevant.contains(&h.path))
            .count();
        recall += relevant as f64 / query.relevant.len().min(3) as f64;
        mrr += hits
            .iter()
            .position(|h| query.relevant.contains(&h.path))
            .map(|i| 1.0 / (i + 1) as f64)
            .unwrap_or(0.0);
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        if relevant == 0 {
            failures.push(format!("no relevant result for {}: {paths:?}", query.q));
        }
        if query.top
            && !hits
                .first()
                .is_some_and(|h| query.relevant.contains(&h.path))
        {
            failures.push(format!(
                "{} must rank a relevant page first: {paths:?}",
                query.q
            ));
        }
        if hits.iter().any(|h| h.partial) != query.partial {
            failures.push(format!(
                "{}: partial expected {}, got {paths:?}",
                query.q, query.partial
            ));
        }
    }
    recall /= corpus.queries.len() as f64;
    mrr /= corpus.queries.len() as f64;
    println!(
        "Japanese search evaluation: {} pages, {} queries, Recall@3={recall:.3}, MRR@3={mrr:.3}, slowest query {slowest:?}",
        docs.len(),
        corpus.queries.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(recall >= 0.90, "Recall@3={recall}");
    assert!(mrr >= 0.80, "MRR@3={mrr}");
    // Well under 50 ms on a laptop; generous so shared CI runners pass.
    assert!(
        slowest < std::time::Duration::from_millis(500),
        "slowest query took {slowest:?}"
    );
}
