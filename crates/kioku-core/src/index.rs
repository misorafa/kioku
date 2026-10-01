//! tantivy full-text index with the lindera `ja` tokenizer (spec §6.2) and search (spec §6.3).

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera_analysis::character_filter::unicode_normalize::{
    UnicodeNormalizeCharacterFilter, UnicodeNormalizeKind,
};
use lindera_tantivy::tokenizer::LinderaTokenizer;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, Occur, Query, QueryParser, TermQuery,
};
use tantivy::schema::{
    DateOptions, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, TextAnalyzer};
use tantivy::{
    DateTime as TantivyDateTime, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument,
    Term,
};

use crate::page::GLOBAL_DIR;

/// Name of the Japanese tokenizer registered on the index.
pub const JA_TOKENIZER: &str = "ja";
/// Max snippet length in chars.
pub const SNIPPET_MAX: usize = 200;
/// Version of what the index contains / how it was analyzed. Bump it whenever documents
/// indexed by an older build would be searched wrongly; `Store::open` then warns until
/// `kioku reindex` has been run. 2 = NFKC normalization in the `ja` analyzer.
pub const INDEX_SCHEMA_VERSION: u32 = 2;
const TITLE_BOOST: f32 = 2.0;
const WRITER_HEAP: usize = 50_000_000;

/// Which pages a search may return.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchScope {
    /// Pages of this project plus all global pages.
    Project(String),
    /// Only `_global` pages.
    Global,
    /// Everything.
    All,
}

/// One search result (spec §6.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    /// Wiki-relative path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// `session` | `page` | `state`.
    pub kind: String,
    /// Plain-text snippet with `【】` around matched terms.
    pub snippet: String,
    /// BM25 score.
    pub score: f32,
    /// RFC 3339 update time.
    pub updated: String,
    /// True when the page is global (under `_global/`).
    pub global: bool,
}

/// A page as fed to the index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDoc {
    /// Wiki-relative path (unique key).
    pub path: String,
    /// Owning project id, if any.
    pub project_id: Option<String>,
    /// `project` | `global`.
    pub scope: String,
    /// `session` | `page` | `state`.
    pub kind: String,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Tags (indexed verbatim).
    pub tags: Vec<String>,
    /// Last update time.
    pub updated: DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    project_id: Field,
    scope: Field,
    kind: Field,
    title: Field,
    body: Field,
    tags: Field,
    updated: Field,
}

/// The tantivy index plus a lazily opened writer (so read-only users never take the lock).
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<Option<IndexWriter>>,
    fields: Fields,
    analyzer: TextAnalyzer,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let ja_text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JA_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let raw_text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_stored();
    let fields = Fields {
        path: b.add_text_field("path", STRING | STORED),
        project_id: b.add_text_field("project_id", STRING),
        scope: b.add_text_field("scope", STRING),
        kind: b.add_text_field("kind", STRING | STORED),
        title: b.add_text_field("title", ja_text.clone()),
        body: b.add_text_field("body", ja_text),
        tags: b.add_text_field("tags", raw_text),
        updated: b.add_date_field("updated", DateOptions::default().set_fast().set_stored()),
    };
    (b.build(), fields)
}

/// The `ja` analyzer: NFKC normalization (lindera character filter, so offsets still point
/// into the original text) → lindera (embedded IPADIC, `Mode::Normal`) → `LowerCaser`.
/// NFKC folds full-width ASCII (`Ｆｌｕｔｔｅｒ`) and half-width kana (`ｱﾌﾟﾘ`).
pub fn ja_analyzer() -> anyhow::Result<TextAnalyzer> {
    static TOKENIZER: OnceLock<Result<LinderaTokenizer, String>> = OnceLock::new();
    let tokenizer = TOKENIZER
        .get_or_init(|| {
            let dictionary = load_dictionary("embedded://ipadic").map_err(|e| e.to_string())?;
            let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
            let mut tokenizer = LinderaTokenizer::from_segmenter(segmenter);
            tokenizer.append_character_filter(
                UnicodeNormalizeCharacterFilter::new(UnicodeNormalizeKind::NFKC).into(),
            );
            Ok(tokenizer)
        })
        .clone()
        .map_err(|e| anyhow!("loading IPADIC dictionary: {e}"))?;
    Ok(TextAnalyzer::builder(tokenizer).filter(LowerCaser).build())
}

/// Tokenizes text with the `ja` analyzer (lowercased tokens, in order).
pub fn tokenize(text: &str) -> anyhow::Result<Vec<String>> {
    let mut analyzer = ja_analyzer()?;
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    Ok(out)
}

impl SearchIndex {
    /// Opens or creates the index in `dir`; returns `(index, fresh)` where `fresh` means it
    /// was (re)created empty and should be rebuilt from the wiki.
    pub fn open(dir: &Path) -> anyhow::Result<(SearchIndex, bool)> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let (schema, fields) = build_schema();
        let fresh = !dir.join("meta.json").exists();
        let index = match open_or_create(dir, schema.clone()) {
            Ok(index) => index,
            Err(e) => {
                tracing::warn!(error = %e, "index incompatible; recreating (run reindex)");
                std::fs::remove_dir_all(dir).ok();
                std::fs::create_dir_all(dir)?;
                let index = open_or_create(dir, schema)?;
                return SearchIndex::from_index(index, fields).map(|i| (i, true));
            }
        };
        SearchIndex::from_index(index, fields).map(|i| (i, fresh))
    }

    fn from_index(index: Index, fields: Fields) -> anyhow::Result<SearchIndex> {
        let analyzer = ja_analyzer()?;
        index.tokenizers().register(JA_TOKENIZER, analyzer.clone());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("opening index reader")?;
        Ok(SearchIndex {
            index,
            reader,
            writer: Mutex::new(None),
            fields,
            analyzer,
        })
    }

    fn with_writer<T>(
        &self,
        f: impl FnOnce(&mut IndexWriter) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut guard = self.writer.lock();
        if guard.is_none() {
            let w = self
                .index
                .writer_with_num_threads(1, WRITER_HEAP)
                .context("opening index writer (is another kioku process writing?)")?;
            *guard = Some(w);
        }
        let writer = guard.as_mut().expect("writer initialized above");
        f(writer)
    }

    /// Removes the documents of `paths` in one commit.
    pub fn delete_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let f = self.fields;
        self.with_writer(|w| {
            for p in paths {
                w.delete_term(Term::from_field_text(f.path, p));
            }
            w.commit().context("committing index")?;
            Ok(())
        })?;
        self.reader.reload().context("reloading index reader")?;
        Ok(())
    }

    /// Replaces (or adds) the document for `doc.path` and commits.
    pub fn upsert(&self, doc: &IndexDoc) -> anyhow::Result<()> {
        self.upsert_many(std::slice::from_ref(doc), false)
    }

    /// Adds/replaces many documents in one commit; `clear` first deletes everything.
    pub fn upsert_many(&self, docs: &[IndexDoc], clear: bool) -> anyhow::Result<()> {
        let f = self.fields;
        self.with_writer(|w| {
            if clear {
                // Not `delete_all_documents`: on a reopened index it can let a delete from an
                // earlier commit hit documents re-added in this one (see the regression test
                // `delete_then_rebuild_keeps_the_rebuilt_document`). A delete query is an
                // ordinary, opstamp-ordered operation.
                w.delete_query(Box::new(AllQuery))?;
            }
            for doc in docs {
                w.delete_term(Term::from_field_text(f.path, &doc.path));
                let mut d = TantivyDocument::default();
                d.add_text(f.path, &doc.path);
                if let Some(p) = &doc.project_id {
                    d.add_text(f.project_id, p);
                }
                d.add_text(f.scope, &doc.scope);
                d.add_text(f.kind, &doc.kind);
                d.add_text(f.title, &doc.title);
                d.add_text(f.body, &doc.body);
                for t in &doc.tags {
                    d.add_text(f.tags, t);
                }
                d.add_date(
                    f.updated,
                    TantivyDateTime::from_timestamp_secs(doc.updated.timestamp()),
                );
                w.add_document(d)?;
            }
            w.commit().context("committing index")?;
            Ok(())
        })?;
        self.reader.reload().context("reloading index reader")?;
        Ok(())
    }

    /// Number of indexed documents.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Paths whose indexed content differs from wiki-derived documents (read-only).
    pub fn inconsistent_paths(&self, expected: &[IndexDoc]) -> anyhow::Result<Vec<String>> {
        let mut wanted: std::collections::BTreeMap<_, _> =
            expected.iter().map(|d| (d.path.clone(), d)).collect();
        let searcher = self.reader.searcher();
        let n = searcher.num_docs() as usize;
        let mut bad = Vec::new();
        if n > 0 {
            let hits = searcher.search(
                &tantivy::query::AllQuery,
                &TopDocs::with_limit(n).order_by_score(),
            )?;
            for (_, addr) in hits {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let text = |field| {
                    doc.get_first(field)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                };
                let path = text(self.fields.path).to_string();
                let matches = wanted.remove(&path).is_some_and(|d| {
                    let tags: Vec<_> = doc
                        .get_all(self.fields.tags)
                        .filter_map(|v| v.as_str())
                        .collect();
                    text(self.fields.title) == d.title
                        && text(self.fields.body).trim_end_matches('\n')
                            == d.body.trim_end_matches('\n')
                        && text(self.fields.kind) == d.kind
                        && tags == d.tags.iter().map(String::as_str).collect::<Vec<_>>()
                });
                if !matches {
                    bad.push(path);
                }
            }
        }
        bad.extend(wanted.into_keys());
        bad.sort();
        bad.dedup();
        Ok(bad)
    }

    /// Full-text search (spec §6.3): OR semantics, title boosted ×2, scope filter, snippets.
    pub fn search(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let Some(user_query) = self.build_query(query)? else {
            return Ok(Vec::new());
        };
        let f = self.fields;
        let term = |field: Field, v: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, v),
                IndexRecordOption::Basic,
            ))
        };
        let filter: Option<Box<dyn Query>> = match scope {
            SearchScope::All => None,
            SearchScope::Global => Some(term(f.scope, "global")),
            SearchScope::Project(id) => Some(Box::new(BooleanQuery::new(vec![
                (Occur::Should, term(f.project_id, id)),
                (Occur::Should, term(f.scope, "global")),
            ]))),
        };
        let full: Box<dyn Query> = match filter {
            None => user_query.box_clone(),
            Some(filter) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, user_query.box_clone()),
                (Occur::Must, Box::new(ConstScoreQuery::new(filter, 0.0))),
            ])),
        };

        let searcher = self.reader.searcher();
        let top = searcher
            .search(&full, &TopDocs::with_limit(limit).order_by_score())
            .context("running search")?;
        let mut snippets =
            SnippetGenerator::create(&searcher, &*user_query, f.body).context("snippets")?;
        // tantivy counts bytes here; Japanese is 3 bytes/char. Ask for ~3× and cut to chars.
        snippets.set_max_num_chars(SNIPPET_MAX * 3);

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).context("loading hit")?;
            let text = |field: Field| {
                doc.get_first(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let path = text(f.path);
            let body = text(f.body);
            let snippet = snippets.snippet(&body);
            let snippet = if snippet.highlighted().is_empty() {
                crate::util::one_line(&body)
            } else {
                plain_snippet(snippet.fragment(), snippet.highlighted())
            };
            let snippet = crate::util::truncate_chars(&snippet, SNIPPET_MAX);
            let updated = doc
                .get_first(f.updated)
                .and_then(|v| v.as_datetime())
                .and_then(|d| DateTime::<Utc>::from_timestamp(d.into_timestamp_secs(), 0))
                .map(crate::util::fmt_ts)
                .unwrap_or_default();
            hits.push(Hit {
                global: path.starts_with(&format!("{GLOBAL_DIR}/")),
                title: text(f.title),
                kind: text(f.kind),
                snippet,
                score,
                updated,
                path,
            });
        }
        Ok(hits)
    }

    /// Builds the user query: tantivy syntax when the query uses it, else a tokenized OR query.
    fn build_query(&self, query: &str) -> anyhow::Result<Option<Box<dyn Query>>> {
        if uses_query_syntax(query) {
            let mut parser =
                QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
            parser.set_field_boost(self.fields.title, TITLE_BOOST);
            match parser.parse_query(query) {
                Ok(q) => return Ok(Some(q)),
                Err(e) => tracing::debug!(error = %e, "query parse failed; falling back to terms"),
            }
        }
        self.term_query(query)
    }

    /// Term-by-term OR over title (boosted), body and tags of the tokenized input.
    fn term_query(&self, query: &str) -> anyhow::Result<Option<Box<dyn Query>>> {
        let f = self.fields;
        let mut analyzer = self.analyzer.clone();
        let mut tokens: Vec<String> = Vec::new();
        {
            let mut stream = analyzer.token_stream(query);
            while stream.advance() {
                let t = stream.token().text.clone();
                if !tokens.contains(&t) {
                    tokens.push(t);
                }
            }
        }
        let meaningful: Vec<String> = tokens
            .iter()
            .filter(|t| !is_noise_token(t))
            .cloned()
            .collect();
        let tokens = if meaningful.is_empty() {
            tokens
        } else {
            meaningful
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for t in &tokens {
            let title: Box<dyn Query> = Box::new(TermQuery::new(
                Term::from_field_text(f.title, t),
                IndexRecordOption::WithFreqs,
            ));
            clauses.push((Occur::Should, Box::new(BoostQuery::new(title, TITLE_BOOST))));
            clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.body, t),
                    IndexRecordOption::WithFreqs,
                )),
            ));
        }
        for word in query.split_whitespace() {
            clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.tags, word),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(BooleanQuery::new(clauses))))
    }
}

fn open_or_create(dir: &Path, schema: Schema) -> anyhow::Result<Index> {
    let mmap = MmapDirectory::open(dir).context("opening index directory")?;
    Index::open_or_create(mmap, schema).context("opening index")
}

/// True when the query uses tantivy syntax (quotes, `field:`, `+`/`-` prefixes, AND/OR/NOT, parens).
fn uses_query_syntax(q: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"["()]|(?:^|\s)(?:title|body|tags):|(?:^|\s)[+-]\S|(?:^|\s)(?:AND|OR|NOT)(?:\s|$)"#,
        )
        .expect("valid syntax regex")
    });
    re.is_match(q)
}

/// Single hiragana (particles like を/の/が) and pure punctuation add noise to OR queries.
fn is_noise_token(t: &str) -> bool {
    let mut chars = t.chars();
    let single_hiragana = matches!((chars.next(), chars.next()), (Some(c), None) if ('\u{3041}'..='\u{309f}').contains(&c));
    single_hiragana || t.chars().all(|c| !c.is_alphanumeric())
}

/// Renders a snippet as plain text with `【】` around highlighted ranges.
fn plain_snippet(fragment: &str, highlighted: &[std::ops::Range<usize>]) -> String {
    let mut ranges: Vec<std::ops::Range<usize>> = highlighted.to_vec();
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    let mut out = String::new();
    let mut pos = 0;
    for r in merged {
        if r.start < pos || r.end > fragment.len() {
            continue;
        }
        out.push_str(&fragment[pos..r.start]);
        out.push('【');
        out.push_str(&fragment[r.start..r.end]);
        out.push('】');
        pos = r.end;
    }
    out.push_str(&fragment[pos..]);
    crate::util::one_line(&out)
}

#[cfg(test)]
mod tests {
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
        // 「引継」: no requirement (spec §6.4) — must simply not error
        idx.search("引継", &SearchScope::All, 10).unwrap();
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
}

#[cfg(test)]
mod evaluation;
