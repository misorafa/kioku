//! tantivy full-text index (spec §6.2–§6.3, SPEC-M3.1 §2): the `ja` analyzer (lindera,
//! embedded IPADIC, NFKC, optional user dictionary) over title and body, the `code`
//! identifier field, the `ja_bigram` zero-hit fallback, re-ranked filtered search, and the
//! switch from an index built with an older schema to a rebuilt one.

mod analysis;

pub use analysis::{
    JaTokenizer, STARTER_USER_DICT, UserWord, bigram_tokens, code_tokens, identifier_parts,
    identifier_runs, nfkc, parse_user_dict, query_bigrams,
};

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, Occur, Query, QueryParser, RangeQuery,
    TermQuery,
};
use tantivy::schema::{
    DateOptions, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DateTime as TantivyDateTime, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument,
    Term,
};

use crate::page::GLOBAL_DIR;
use analysis::{WS_TOKENIZER, ja_analyzer_with, load_ja_tokenizer, ws_analyzer};

/// Name of the Japanese tokenizer registered on the index.
pub const JA_TOKENIZER: &str = "ja";
/// Max snippet length in chars.
pub const SNIPPET_MAX: usize = 200;
/// Version of what the index contains / how it was analyzed. Bump it whenever documents
/// indexed by an older build would be searched wrongly; the server then rebuilds the index
/// after it starts listening (SPEC-M2.8 §5). 2 = NFKC normalization in the `ja` analyzer;
/// 3 = `code`, `ja_bigram` and `machine` fields (SPEC-M3.1 §2), built in its own directory.
pub const INDEX_SCHEMA_VERSION: u32 = 3;
/// Weight of the `code` field's clauses against the Japanese fields (SPEC-M3.1 §2).
pub const CODE_WEIGHT: f32 = 0.7;
/// Share of [`CODE_WEIGHT`] given to the parts of a compound identifier in the query.
pub const CODE_PART_FACTOR: f32 = 0.5;
/// BM25 hits fetched per requested hit before re-ranking.
pub const RERANK_POOL: usize = 3;
/// Recency halves every this many days...
pub const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;
/// ...but never drops below this factor.
pub const RECENCY_FLOOR: f32 = 0.25;
/// Factor of a page tagged `pinned`.
pub const PINNED_BOOST: f32 = 1.5;
/// Share of a query's bigrams a partial match must contain.
const BIGRAM_MATCH: f64 = 0.75;
const TITLE_BOOST: f32 = 2.0;
const WRITER_HEAP: usize = 50_000_000;
const WRITER_RETRIES: u32 = 40;
/// 2200-01-01T00:00:00Z: `since` is clamped to it (tantivy dates are i64 nanoseconds).
const MAX_SINCE_SECS: i64 = 7_258_118_400;
/// Page kinds `kinds` may name.
pub const KINDS: [&str; 3] = ["page", "session", "state"];

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

/// Filters and clock of a search (SPEC-M3.1 §2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// Only pages updated on or after this day (UTC).
    pub since: Option<NaiveDate>,
    /// Only these kinds (`page` / `session` / `state`); empty = all.
    pub kinds: Vec<String>,
    /// "Now" for the recency factor; `None` = the current time.
    pub now: Option<DateTime<Utc>>,
}

impl SearchOptions {
    /// Parses `since` (`YYYY-MM-DD`) and `kinds`; the error text names the bad value.
    pub fn parse(since: Option<&str>, kinds: &[String]) -> Result<SearchOptions, String> {
        let since = match since.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Some(
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map_err(|_| format!("since must be YYYY-MM-DD, got {s}"))?,
            ),
            None => None,
        };
        let mut out = Vec::new();
        for k in kinds.iter().flat_map(|k| k.split(',')) {
            let k = k.trim().to_ascii_lowercase();
            if k.is_empty() {
                continue;
            }
            if !KINDS.contains(&k.as_str()) {
                return Err(format!(
                    "unknown kind: {k} (expected page, session or state)"
                ));
            }
            if !out.contains(&k) {
                out.push(k);
            }
        }
        Ok(SearchOptions {
            since,
            kinds: out,
            now: None,
        })
    }
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
    /// Relevance: BM25 × recency × kind weight × pinned boost (SPEC-M3.1 §2).
    pub score: f32,
    /// RFC 3339 update time.
    pub updated: String,
    /// True when the page is global (under `_global/`).
    pub global: bool,
    /// Machine of a session page (SPEC-M3.0 §6), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// True when found by the character-bigram fallback (no word matched).
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Machine of a session page, if known.
    pub machine: Option<String>,
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
    /// Absent from an index built before schema 3.
    machine: Option<Field>,
    code: Option<Field>,
    bigram: Option<Field>,
}

fn build_schema() -> Schema {
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
    let pre_tokenized = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(WS_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    b.add_text_field("path", STRING | STORED);
    b.add_text_field("project_id", STRING);
    b.add_text_field("scope", STRING);
    b.add_text_field("kind", STRING | STORED);
    b.add_text_field("title", ja_text.clone());
    b.add_text_field("body", ja_text);
    b.add_text_field("tags", raw_text);
    b.add_date_field("updated", DateOptions::default().set_fast().set_stored());
    b.add_text_field("machine", STORED);
    // Identifiers count by presence: no term frequencies and no length normalization, so
    // a page dense with identifiers is not penalized against one repeating a common word.
    let identifiers = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(WS_TOKENIZER)
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    );
    b.add_text_field("code", identifiers);
    b.add_text_field("ja_bigram", pre_tokenized);
    b.build()
}

/// The fields of `schema` by name (an older schema lacks the optional ones).
fn fields_of(schema: &Schema) -> anyhow::Result<Fields> {
    let get = |name: &str| {
        schema
            .get_field(name)
            .with_context(|| format!("index has no {name} field"))
    };
    Ok(Fields {
        path: get("path")?,
        project_id: get("project_id")?,
        scope: get("scope")?,
        kind: get("kind")?,
        title: get("title")?,
        body: get("body")?,
        tags: get("tags")?,
        updated: get("updated")?,
        machine: schema.get_field("machine").ok(),
        code: schema.get_field("code").ok(),
        bigram: schema.get_field("ja_bigram").ok(),
    })
}

/// The `ja` analyzer without a user dictionary: NFKC normalization (lindera character
/// filter, so offsets still point into the original text) → lindera (embedded IPADIC,
/// `Mode::Normal`) → lowercase. NFKC folds full-width ASCII and half-width kana.
pub fn ja_analyzer() -> anyhow::Result<TextAnalyzer> {
    Ok(ja_analyzer_with(JaTokenizer::plain()?))
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

/// One opened tantivy index with its reader, lazily opened writer and analyzer.
struct Inner {
    index: Index,
    reader: IndexReader,
    writer: Mutex<Option<IndexWriter>>,
    fields: Fields,
    analyzer: TextAnalyzer,
    dir: PathBuf,
}

impl Inner {
    fn new(index: Index, dir: &Path, tokenizer: &JaTokenizer) -> anyhow::Result<Inner> {
        let fields = fields_of(&index.schema())?;
        let analyzer = ja_analyzer_with(tokenizer.clone());
        index.tokenizers().register(JA_TOKENIZER, analyzer.clone());
        index.tokenizers().register(WS_TOKENIZER, ws_analyzer());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("opening index reader")?;
        Ok(Inner {
            index,
            reader,
            writer: Mutex::new(None),
            fields,
            analyzer,
            dir: dir.to_path_buf(),
        })
    }

    /// True when the index was built with the current schema.
    fn current(&self) -> bool {
        self.fields.code.is_some() && self.fields.bigram.is_some()
    }

    fn with_writer<T>(
        &self,
        f: impl FnOnce(&mut IndexWriter) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut guard = self.writer.lock();
        if guard.is_none() {
            // The lock file may still be held for a moment by a child process that inherited
            // its handle across fork (git runs concurrently); retry briefly.
            let mut attempt = 0;
            let w = loop {
                match self.index.writer_with_num_threads(1, WRITER_HEAP) {
                    Err(tantivy::TantivyError::LockFailure(_, _)) if attempt < WRITER_RETRIES => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    other => {
                        break other
                            .context("opening index writer (is another kioku process writing?)")?;
                    }
                }
            };
            *guard = Some(w);
        }
        let writer = guard.as_mut().expect("writer initialized above");
        f(writer)
    }

    /// Adds/replaces `docs` in one commit; `clear` first deletes everything.
    fn write(&self, docs: &[IndexDoc], clear: bool) -> anyhow::Result<()> {
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
                w.add_document(tantivy_doc(&f, doc))?;
            }
            w.commit().context("committing index")?;
            Ok(())
        })?;
        self.reader.reload().context("reloading index reader")?;
        Ok(())
    }
}

fn tantivy_doc(f: &Fields, doc: &IndexDoc) -> TantivyDocument {
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
    if let (Some(field), Some(m)) = (f.machine, &doc.machine) {
        d.add_text(field, m);
    }
    let text = format!("{}\n{}", doc.title, doc.body);
    if let Some(field) = f.code {
        d.add_text(field, code_tokens(&text).join(" "));
    }
    if let Some(field) = f.bigram {
        d.add_text(field, bigram_tokens(&text).join(" "));
    }
    d
}

/// The search index. Usually the current-schema index in its own directory; right after
/// an upgrade, the older index it replaces (served until [`SearchIndex::rebuild`] has built
/// the new one and switched to it, SPEC-M2.8 §5).
pub struct SearchIndex {
    dir: PathBuf,
    legacy: Vec<PathBuf>,
    user_dict: Option<PathBuf>,
    inner: RwLock<Arc<Inner>>,
}

impl SearchIndex {
    /// Opens or creates the index in `dir` (no older index, no user dictionary); returns
    /// `(index, fresh)` where `fresh` means it was (re)created empty.
    pub fn open(dir: &Path) -> anyhow::Result<(SearchIndex, bool)> {
        SearchIndex::open_with(dir, &[], None)
    }

    /// Opens the index in `dir`. When `dir` holds none yet, the newest index in `legacy`
    /// (directories of older schema versions, newest first) is served instead until
    /// [`SearchIndex::rebuild`]; with neither, an empty index is created and `fresh` is true.
    /// `user_dict` (`dict/user.csv`) is layered onto the `ja` analyzer when it exists.
    pub fn open_with(
        dir: &Path,
        legacy: &[PathBuf],
        user_dict: Option<&Path>,
    ) -> anyhow::Result<(SearchIndex, bool)> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tokenizer = load_ja_tokenizer(user_dict, &dict_scratch(dir))?;
        let schema = build_schema();
        let mut fresh = !dir.join("meta.json").exists();
        let mut opened = None;
        if fresh {
            for old in legacy {
                if !old.join("meta.json").exists() {
                    continue;
                }
                match Index::open_in_dir(old)
                    .map_err(anyhow::Error::from)
                    .and_then(|i| Inner::new(i, old, &tokenizer))
                {
                    Ok(inner) => {
                        tracing::info!(dir = %old.display(), "serving the index of an older kioku until it is rebuilt");
                        opened = Some(inner);
                        fresh = false;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(dir = %old.display(), error = format!("{e:#}"), "older index unusable")
                    }
                }
            }
        }
        let inner = match opened {
            Some(inner) => inner,
            None => match open_or_create(dir, schema.clone()) {
                Ok(index) => Inner::new(index, dir, &tokenizer)?,
                Err(e) => {
                    tracing::warn!(error = %e, "index incompatible; recreating (run reindex)");
                    std::fs::remove_dir_all(dir).ok();
                    std::fs::create_dir_all(dir)?;
                    fresh = true;
                    Inner::new(open_or_create(dir, schema)?, dir, &tokenizer)?
                }
            },
        };
        Ok((
            SearchIndex {
                dir: dir.to_path_buf(),
                legacy: legacy.to_vec(),
                user_dict: user_dict.map(Path::to_path_buf),
                inner: RwLock::new(Arc::new(inner)),
            },
            fresh,
        ))
    }

    fn inner(&self) -> Arc<Inner> {
        self.inner.read().clone()
    }

    /// True while an index of an older schema is being served (until [`SearchIndex::rebuild`]).
    pub fn serving_legacy(&self) -> bool {
        let inner = self.inner();
        inner.dir != self.dir || !inner.current()
    }

    /// Deletes the directories of older indexes that are no longer served (best effort: on
    /// Windows a directory still mapped by a search in flight is left for the next start).
    pub fn remove_legacy_dirs(&self) {
        if self.serving_legacy() {
            return;
        }
        for old in &self.legacy {
            if old != &self.dir && old.exists() {
                match std::fs::remove_dir_all(old) {
                    Ok(()) => tracing::info!(dir = %old.display(), "removed the older index"),
                    Err(e) => {
                        tracing::debug!(dir = %old.display(), error = %e, "older index not removed yet")
                    }
                }
            }
        }
    }

    /// Removes the documents of `paths` in one commit.
    pub fn delete_paths(&self, paths: &[String]) -> anyhow::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let inner = self.inner();
        let f = inner.fields;
        inner.with_writer(|w| {
            for p in paths {
                w.delete_term(Term::from_field_text(f.path, p));
            }
            w.commit().context("committing index")?;
            Ok(())
        })?;
        inner.reader.reload().context("reloading index reader")?;
        Ok(())
    }

    /// Replaces (or adds) the document for `doc.path` and commits.
    pub fn upsert(&self, doc: &IndexDoc) -> anyhow::Result<()> {
        self.upsert_many(std::slice::from_ref(doc), false)
    }

    /// Adds/replaces many documents in one commit; `clear` first deletes everything. With no
    /// documents and no `clear` it does nothing.
    pub fn upsert_many(&self, docs: &[IndexDoc], clear: bool) -> anyhow::Result<()> {
        // Nothing to do: do not open the writer (it holds the index lock files).
        if docs.is_empty() && !clear {
            return Ok(());
        }
        self.inner().write(docs, clear)
    }

    /// Rebuilds the index from `docs` with the user dictionary re-read. While an older
    /// index is served, the new one is built in the current directory and then switched to
    /// (searches keep using the old one until then); otherwise it is cleared and refilled.
    pub fn rebuild(&self, docs: &[IndexDoc]) -> anyhow::Result<()> {
        let tokenizer = load_ja_tokenizer(self.user_dict.as_deref(), &dict_scratch(&self.dir))?;
        let old = self.inner();
        let new = if self.serving_legacy() {
            let dir = self.dir.clone();
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let index = match open_or_create(&dir, build_schema()) {
                Ok(i) => i,
                Err(_) => {
                    std::fs::remove_dir_all(&dir).ok();
                    std::fs::create_dir_all(&dir)?;
                    open_or_create(&dir, build_schema())?
                }
            };
            Inner::new(index, &dir, &tokenizer)?
        } else {
            // Same index, new analyzer (registered on the index, so the segments written
            // from now on use it). The writer moves over: dropping it and opening another
            // can race with a child process that briefly inherits the lock file handle.
            let inner = Inner::new(old.index.clone(), &old.dir, &tokenizer)?;
            *inner.writer.lock() = old.writer.lock().take();
            inner
        };
        new.write(docs, true)?;
        drop(old.writer.lock().take());
        *self.inner.write() = Arc::new(new);
        drop(old);
        self.remove_legacy_dirs();
        Ok(())
    }

    /// Number of indexed documents.
    pub fn num_docs(&self) -> u64 {
        self.inner().reader.searcher().num_docs()
    }

    /// Paths whose indexed content differs from wiki-derived documents (read-only).
    pub fn inconsistent_paths(&self, expected: &[IndexDoc]) -> anyhow::Result<Vec<String>> {
        let inner = self.inner();
        let fields = inner.fields;
        let mut wanted: std::collections::BTreeMap<_, _> =
            expected.iter().map(|d| (d.path.clone(), d)).collect();
        let searcher = inner.reader.searcher();
        let n = searcher.num_docs() as usize;
        let mut bad = Vec::new();
        if n > 0 {
            let hits = searcher.search(&AllQuery, &TopDocs::with_limit(n).order_by_score())?;
            for (_, addr) in hits {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let text = |field| {
                    doc.get_first(field)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                };
                let path = text(fields.path).to_string();
                let matches = wanted.remove(&path).is_some_and(|d| {
                    let tags: Vec<_> = doc
                        .get_all(fields.tags)
                        .filter_map(|v| v.as_str())
                        .collect();
                    text(fields.title) == d.title
                        && text(fields.body).trim_end_matches('\n') == d.body.trim_end_matches('\n')
                        && text(fields.kind) == d.kind
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

    /// Full-text search with default options (see [`SearchIndex::search_with`]).
    pub fn search(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        self.search_with(query, scope, limit, &SearchOptions::default())
    }

    /// Full-text search (spec §6.3, SPEC-M3.1 §2): OR semantics over the `ja` title (×2)
    /// and body, the `code` field (×0.7) and tags; scope / kind / `since` filters; the best
    /// `3 × limit` BM25 hits re-ranked by recency, kind and `pinned`; when nothing matches,
    /// a character-bigram partial match (hits flagged `partial`).
    pub fn search_with(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
        opts: &SearchOptions,
    ) -> anyhow::Result<Vec<Hit>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner();
        let f = inner.fields;
        let filter = filter_query(&f, scope, opts);
        let searcher = inner.reader.searcher();
        let pool = TopDocs::with_limit(limit.saturating_mul(RERANK_POOL)).order_by_score();
        let user_query = build_query(&inner, query)?;
        let mut top = match &user_query {
            Some(q) => searcher
                .search(&*with_filter(q.box_clone(), &filter), &pool)
                .context("running search")?,
            None => Vec::new(),
        };
        let mut partial = false;
        let grams = query_bigrams(query);
        if top.is_empty()
            && let Some(q) = bigram_query(&f, &grams)
        {
            top = searcher
                .search(&*with_filter(q, &filter), &pool)
                .context("running partial-match search")?;
            partial = !top.is_empty();
        }

        // Re-rank: BM25 × recency × kind weight × pinned; ties keep BM25 order.
        let now = opts.now.unwrap_or_else(Utc::now);
        let mut ranked = Vec::with_capacity(top.len());
        for (i, (score, addr)) in top.into_iter().enumerate() {
            let doc: TantivyDocument = searcher.doc(addr).context("loading hit")?;
            let kind = stored_text(&doc, f.kind);
            let pinned = doc
                .get_all(f.tags)
                .filter_map(|v| v.as_str())
                .any(|t| t.eq_ignore_ascii_case(crate::carry::PINNED_TAG));
            let updated = doc
                .get_first(f.updated)
                .and_then(|v| v.as_datetime())
                .and_then(|d| DateTime::<Utc>::from_timestamp(d.into_timestamp_secs(), 0));
            let factor = updated.map_or(1.0, |u| recency(u, now))
                * kind_weight(&kind)
                * if pinned { PINNED_BOOST } else { 1.0 };
            ranked.push((score * factor, i, doc, updated));
        }
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        ranked.truncate(limit);

        let snippets = match (&user_query, partial) {
            (Some(q), false) => {
                let mut g =
                    SnippetGenerator::create(&searcher, &**q, f.body).context("snippets")?;
                // tantivy counts bytes here; Japanese is 3 bytes/char. Ask for ~3× and cut.
                g.set_max_num_chars(SNIPPET_MAX * 3);
                Some(g)
            }
            _ => None,
        };
        let mut hits = Vec::with_capacity(ranked.len());
        for (score, _, doc, updated) in ranked {
            let path = stored_text(&doc, f.path);
            let body = stored_text(&doc, f.body);
            let snippet = match &snippets {
                Some(g) => {
                    let s = g.snippet(&body);
                    if s.highlighted().is_empty() {
                        crate::util::one_line(&body)
                    } else {
                        plain_snippet(s.fragment(), s.highlighted())
                    }
                }
                None => partial_snippet(&body, &grams),
            };
            hits.push(Hit {
                global: path.starts_with(&format!("{GLOBAL_DIR}/")),
                title: stored_text(&doc, f.title),
                kind: stored_text(&doc, f.kind),
                snippet: crate::util::truncate_chars(&snippet, SNIPPET_MAX),
                score,
                updated: updated.map(crate::util::fmt_ts).unwrap_or_default(),
                machine: f
                    .machine
                    .map(|m| stored_text(&doc, m))
                    .filter(|m| !m.is_empty()),
                partial,
                path,
            });
        }
        Ok(hits)
    }
}

/// `index/user-dict.lindera.csv`: the lindera CSV generated from `dict/user.csv`.
fn dict_scratch(dir: &Path) -> PathBuf {
    dir.with_file_name("user-dict.lindera.csv")
}

fn stored_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `0.5 ^ (age_days / 30)`, floored at [`RECENCY_FLOOR`] (a future date counts as now).
pub fn recency(updated: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let age_days = (now - updated).num_seconds().max(0) as f64 / 86_400.0;
    (0.5f64.powf(age_days / RECENCY_HALF_LIFE_DAYS) as f32).max(RECENCY_FLOOR)
}

/// page 1.0 / state 0.8 / session 0.6 (anything else 1.0).
pub fn kind_weight(kind: &str) -> f32 {
    match kind {
        "state" => 0.8,
        "session" => 0.6,
        _ => 1.0,
    }
}

fn term(field: Field, v: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, v),
        IndexRecordOption::Basic,
    ))
}

/// Scope, kinds and `since` as one non-scoring filter (`None` = no filter).
fn filter_query(f: &Fields, scope: &SearchScope, opts: &SearchOptions) -> Option<Box<dyn Query>> {
    let mut must: Vec<Box<dyn Query>> = Vec::new();
    match scope {
        SearchScope::All => {}
        SearchScope::Global => must.push(term(f.scope, "global")),
        SearchScope::Project(id) => must.push(Box::new(BooleanQuery::new(vec![
            (Occur::Should, term(f.project_id, id)),
            (Occur::Should, term(f.scope, "global")),
        ]))),
    }
    if !opts.kinds.is_empty() {
        must.push(Box::new(BooleanQuery::new(
            opts.kinds
                .iter()
                .map(|k| (Occur::Should, term(f.kind, k)))
                .collect(),
        )));
    }
    if let Some(day) = opts.since {
        let secs = day
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc().timestamp())
            .unwrap_or_default()
            // tantivy keeps dates as i64 nanoseconds: stay inside 1970..2200.
            .clamp(0, MAX_SINCE_SECS);
        must.push(Box::new(RangeQuery::new(
            Bound::Included(Term::from_field_date(
                f.updated,
                TantivyDateTime::from_timestamp_secs(secs),
            )),
            Bound::Unbounded,
        )));
    }
    match must.len() {
        0 => None,
        1 => must.pop(),
        _ => Some(Box::new(BooleanQuery::new(
            must.into_iter().map(|q| (Occur::Must, q)).collect(),
        ))),
    }
}

fn with_filter(query: Box<dyn Query>, filter: &Option<Box<dyn Query>>) -> Box<dyn Query> {
    match filter {
        None => query,
        Some(filter) => Box::new(BooleanQuery::new(vec![
            (Occur::Must, query),
            (
                Occur::Must,
                Box::new(ConstScoreQuery::new(filter.box_clone(), 0.0)),
            ),
        ])),
    }
}

/// Builds the user query: tantivy syntax when the query uses it, else a tokenized OR query.
fn build_query(inner: &Inner, query: &str) -> anyhow::Result<Option<Box<dyn Query>>> {
    if uses_query_syntax(query) {
        let f = inner.fields;
        let mut parser = QueryParser::for_index(&inner.index, vec![f.title, f.body]);
        parser.set_field_boost(f.title, TITLE_BOOST);
        match parser.parse_query(query) {
            Ok(q) => return Ok(Some(q)),
            Err(e) => tracing::debug!(error = %e, "query parse failed; falling back to terms"),
        }
    }
    term_query(inner, query)
}

/// Term-by-term OR over title (boosted), body, `code` (×0.7) and tags of the input.
fn term_query(inner: &Inner, query: &str) -> anyhow::Result<Option<Box<dyn Query>>> {
    let f = inner.fields;
    let mut analyzer = inner.analyzer.clone();
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
    // Parts of a compound identifier are matched through the `code` field instead.
    let parts = match f.code {
        Some(_) => identifier_parts(query),
        None => HashSet::new(),
    };
    let meaningful: Vec<String> = tokens
        .iter()
        .filter(|t| !is_noise_token(t))
        .cloned()
        .collect();
    let tokens: Vec<String> = if meaningful.is_empty() {
        tokens
    } else {
        meaningful
    };
    let tokens: Vec<String> = tokens.into_iter().filter(|t| !parts.contains(t)).collect();
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let freq_term = |field: Field, t: &str| -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, t),
            IndexRecordOption::WithFreqs,
        ))
    };
    for t in &tokens {
        clauses.push((
            Occur::Should,
            Box::new(BoostQuery::new(freq_term(f.title, t), TITLE_BOOST)),
        ));
        clauses.push((Occur::Should, freq_term(f.body, t)));
    }
    if let Some(code) = f.code {
        // A whole identifier (or a lone word) at CODE_WEIGHT, the parts of a compound one
        // at half that, so naming `kioku_handoff_write` beats merely using its words.
        let mut weights: Vec<(String, f32)> = Vec::new();
        for run in identifier_runs(query) {
            let compound = run.len() > 1;
            for (i, t) in run.into_iter().enumerate() {
                let w = if i == 0 || !compound {
                    CODE_WEIGHT
                } else {
                    CODE_WEIGHT * CODE_PART_FACTOR
                };
                match weights.iter_mut().find(|(seen, _)| *seen == t) {
                    Some((_, old)) => *old = old.max(w),
                    None => weights.push((t, w)),
                }
            }
        }
        for (t, w) in weights {
            clauses.push((Occur::Should, Box::new(BoostQuery::new(term(code, &t), w))));
        }
    }
    for word in query.split_whitespace() {
        clauses.push((Occur::Should, term(f.tags, word)));
    }
    if clauses.is_empty() {
        return Ok(None);
    }
    Ok(Some(Box::new(BooleanQuery::new(clauses))))
}

/// The zero-hit fallback: at least 75% of the query's character bigrams in `ja_bigram`.
fn bigram_query(f: &Fields, grams: &[String]) -> Option<Box<dyn Query>> {
    let field = f.bigram?;
    if grams.is_empty() {
        return None;
    }
    let clauses: Vec<(Occur, Box<dyn Query>)> = grams
        .iter()
        .map(|g| -> (Occur, Box<dyn Query>) {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, g),
                    IndexRecordOption::WithFreqs,
                )),
            )
        })
        .collect();
    let min = ((grams.len() as f64 * BIGRAM_MATCH).ceil() as usize).max(1);
    Some(Box::new(BooleanQuery::with_minimum_required_clauses(
        clauses, min,
    )))
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

/// A partial match's snippet: the body around the first query bigram found verbatim
/// (marked `【】`), else its beginning.
fn partial_snippet(body: &str, grams: &[String]) -> String {
    let lower = body.to_lowercase();
    // Lowercasing can change byte lengths; only use the position when it did not.
    let found = grams.iter().find_map(|g| {
        let at = if lower.len() == body.len() {
            lower.find(g.as_str())
        } else {
            body.find(g.as_str())
        }?;
        Some((at, g.len()))
    });
    let Some((at, len)) = found else {
        return crate::util::one_line(body);
    };
    let before: String = {
        let chars: Vec<char> = body[..at].chars().collect();
        chars[chars.len().saturating_sub(40)..].iter().collect()
    };
    let text = format!("{before}【{}】{}", &body[at..at + len], &body[at + len..]);
    crate::util::one_line(&text)
}

#[cfg(test)]
mod evaluation;
#[cfg(test)]
mod tests;
