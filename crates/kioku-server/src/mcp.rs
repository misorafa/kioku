//! The MCP service (spec §10): six `kioku_*` tools over the `Store`, served through rmcp's
//! streamable HTTP transport at `/mcp`. The tool descriptions, parameter types and output
//! formatters are public so the `kioku mcp` stdio bridge (M2 §20) serves identical tools. Tool descriptions are what the model reads, so they
//! are written in Japanese with one English line each; outputs are plain text.

use std::sync::Arc;

use kioku_core::handoff::{REFERENCE_CONCURRENT, REFERENCE_RESUMED};
use kioku_core::strings::memory_note;
use kioku_core::{
    Error, Handoff, HandoffInput, Hit, Page, PageScope, PathSession, PendingHandoff, ProjectAlias,
    SearchOptions, SearchScope, Store, VERSION,
};
use kioku_core::{StatusReport, WritePageRequest};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;

use crate::shared::{DEFAULT_MCP_LIMIT, blocking, clamp_limit, resolve_scope, with_project_hint};

/// `instructions` returned on initialize.
pub const INSTRUCTIONS: &str = "kioku は、このユーザーのすべてのマシン・すべてのコーディングエージェントで共有される記憶（過去のセッション要約、STATE.md、保存済みページ、引き継ぎ）です。作業を始めるとき、特にコードベースを探索したり調査を繰り返したりする前に、まず kioku_query で関連する過去の記録・決定事項を検索し、見つかったページは kioku_read で読んでください（project には SessionStart で渡された id を使う）。後で役立つ知見や設計判断は kioku_write_page で残し、セッションを終える前（または作業の区切りやコンテキストが尽きる前）には必ず kioku_handoff_write（project と、SessionStart で渡された session の id を指定）で 要約 / 次にやること / 未解決の質問 / 決定事項 を記録してください。次のセッションはそれを自動で受け取ります。 kioku is shared memory across agents and machines: query it before exploring, and write a handoff with kioku_handoff_write before you stop.";

/// Tool description of `query` (shared with the `kioku mcp` bridge).
pub const QUERY_DESC: &str = "kioku の記憶（過去のセッション要約・各プロジェクトの STATE.md・保存済みページ）を全文検索する。日本語・英語どちらのクエリも使える（形態素解析済み。kioku_handoff_write や Store::open、src/index.rs のような識別子・パスは部分でも全体でも当たる）。コードを探索したり同じ調査を繰り返したりする前に、まずこれを呼ぶこと。project を渡すとそのプロジェクトとグローバルのページに絞られる。新しいもの・ページ（session より page、pinned 付き）が上に来る。since（YYYY-MM-DD）でそれ以降の更新に、kinds（page / session / state）で種類に絞れる。語として一致しないときは文字単位の部分一致で探し、結果に（部分一致）と付く。path_prefix（例: crates/kioku-core/src/store.rs）を渡すと、そのパス以下のファイルを編集したセッションを新しい順にタイトルと引き継ぎの要約つきで返す（誰がなぜ触ったか）。結果の path は kioku_read で全文を読める。\nSearch kioku's shared memory (past sessions, STATE.md, pages) before exploring; Japanese and English queries both work. Optional `since`, `kinds`; `path_prefix` lists the sessions that edited files under a path.";

/// Tool description of `read` (shared with the `kioku mcp` bridge).
pub const READ_DESC: &str = "kioku のページを path（kioku_query の結果に出る wiki 内の相対パス。例: <project_id>/STATE.md, <project_id>/sessions/2026-09-25-0c2f1a2b-3f9a1c2e4b5d.md, _global/<slug>.md）で読み、frontmatter の要約、本文、更新競合の検出に使う revision を返す。\nRead one kioku page by its wiki-relative path.";

/// Tool description of `write_page` (shared with the `kioku mcp` bridge).
pub const WRITE_PAGE_DESC: &str = "後で役に立つ知見・設計判断・手順・調査結果を Markdown ページとして kioku に保存する（検索対象になり、git に履歴が残る）。同じ title（または path）で書くと本文を置き換える。tags に pinned を付けたページは、そのプロジェクト（scope=global なら全プロジェクト）の SessionStart の <kioku> ブロックに毎回表示される（新しい順に 3 件まで、本文の先頭 400 字）— 常に守ってほしいルールや前提に使う。既存ページの更新前には kioku_read で読み、返された revision を expected_revision に渡す。競合（エラー）したら再読込して変更を統合してから書き直す。省略または空文字なら無条件に上書きする。scope=project（project を渡した場合の既定）はそのプロジェクト専用、scope=global はプロジェクトを横断する個人的なメモ。セッションの引き継ぎには使わず kioku_handoff_write を使うこと。\nSave durable knowledge as a searchable page; writing the same title/path replaces it. Tag it `pinned` to show it at every session start.";

/// Tool description of `handoff_write` (shared with the `kioku mcp` bridge).
pub const HANDOFF_WRITE_DESC: &str = "このセッションの引き継ぎを記録する。このプロジェクトで次に始まるセッション（別のエージェントや別マシンでも）の冒頭に自動で渡される。作業を終える前、区切りがついたとき、コンテキストが尽きそうなときに必ず呼ぶこと。project と session には SessionStart の <kioku> ブロックに書かれた project の id と session の id を渡すこと（session を省略すると、そのプロジェクトで最後に観測のあった開いているセッションに紐づく）。summary=何をしたか・今どういう状態か、next_steps=次の一手（ファイル名やコマンドまで具体的に）、open_questions=未解決の点、decisions=決めたこととその理由、verified=確認済みの事実（実際に試して確かめたこと）、gotchas=落とし穴・注意点（次のセッションが踏みそうな罠）。decisions / verified / open_questions / gotchas は次回以降のセッション開始時にも引き継がれて表示されるので、1 項目 1 行で簡潔に書くこと。\nRecord a handoff for the next session of this project; always call it before you stop. Pass `project` and `session` from the SessionStart <kioku> block. decisions, verified facts, open questions and gotchas are carried into later sessions too: one short line each.";

/// Tool description of `handoff_pending` (shared with the `kioku mcp` bridge).
pub const HANDOFF_PENDING_DESC: &str = "プロジェクトの未受領の引き継ぎ（最新のもの）を取得する。既定の accept=false では覗くだけで消費しない。accept=true にすると受領済みにして、同じレーンの古い未受領の引き継ぎを superseded（置き換え済み）にする。通常は SessionStart フックが自動で受領するが、同じブランチで別のセッションが作業中だった場合は参考表示だけで受領されないので、引き継ぐと決めたらここで accept=true を呼ぶ。自分のセッションが書いた引き継ぎは受領されない。引き継ぎはブランチごとのレーンに分かれる: session を渡すとそのセッションのレーン、lane（ブランチ名）を渡すとそのレーン、どちらも無ければ既定ブランチ（メインライン）のレーンを読む。自分のレーンに引き継ぎが無いときは、メインラインの引き継ぎが参考として返る（受領はされない）。<kioku> ブロックに引き継がれた決定事項だけでは足りないときは history=N（最大 20）で、そのレーンの直近 N 件の引き継ぎを状態（pending / accepted by … / superseded）つきで読める。\nPeek at (or accept) the pending handoff of a project; handoffs are routed per branch lane (session or lane; default = main line). `history: N` (≤ 20) returns the lane's last N handoffs with their status when the carried decisions are not enough.";

/// Tool description of `status` (shared with the `kioku mcp` bridge).
pub const STATUS_DESC: &str = "kioku サーバーの状態を返す: データディレクトリ、プロジェクト・ページ・セッション・観測・引き継ぎの件数、検索索引の文書数、登録済みプロジェクトの id 一覧。\nShow kioku server status, counts and known project ids.";

/// Search scope accepted by `kioku_query`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "lowercase")]
pub enum QueryScope {
    /// そのプロジェクト＋グローバル / this project plus global pages.
    Project,
    /// グローバルのみ / only global pages.
    Global,
    /// 全プロジェクト / everything.
    All,
}

impl QueryScope {
    /// Query-string / `resolve_scope` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryScope::Project => "project",
            QueryScope::Global => "global",
            QueryScope::All => "all",
        }
    }
}

/// Input of `kioku_query`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct QueryParams {
    /// 検索語（日本語・英語可。自然文でもよい。path_prefix だけなら空でよい）/ search terms,
    /// Japanese or English (may be empty with `path_prefix`).
    #[serde(default)]
    pub query: String,
    /// プロジェクト id（SessionStart で渡されたもの）/ project id from the SessionStart context.
    #[serde(default)]
    pub project: Option<String>,
    /// 検索範囲。省略時は project があれば project、なければ all / search scope; defaults to
    /// `project` when a project is given, else `all`.
    #[serde(default)]
    pub scope: Option<QueryScope>,
    /// 最大件数（既定 8）/ maximum number of hits (default 8).
    #[serde(default)]
    pub limit: Option<usize>,
    /// この日付（YYYY-MM-DD）以降に更新されたものだけ / only what was updated on or after
    /// this day.
    #[serde(default)]
    pub since: Option<String>,
    /// 種類で絞る: page / session / state / only these kinds.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// このパス以下のファイルを編集したセッションを新しい順に返す（例:
    /// crates/kioku-core/src/store.rs）。query は省略可 / list the sessions that edited files
    /// under this path, newest first (`query` may then be empty).
    #[serde(default)]
    pub path_prefix: Option<String>,
}

/// Input of `kioku_read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadParams {
    /// wiki 内の相対パス（kioku_query の結果の path）/ wiki-relative page path.
    pub path: String,
}

/// Page scope accepted by `kioku_write_page`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "lowercase")]
pub enum WriteScope {
    /// プロジェクト専用 / belongs to one project.
    Project,
    /// プロジェクト横断の個人メモ / personal, cross-project.
    Global,
}

/// Input of `kioku_write_page`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WritePageParams {
    /// kioku_read の revision。省略・空文字は無条件の上書き / revision from kioku_read; absent or empty = unconditional.
    #[serde(default)]
    pub expected_revision: Option<String>,
    /// ページのタイトル（日本語可）/ page title.
    pub title: String,
    /// 本文（Markdown）/ Markdown body.
    pub content: String,
    /// プロジェクト id（scope=project では必須）/ project id (required for scope=project).
    #[serde(default)]
    pub project: Option<String>,
    /// project または global。省略時は project があれば project / `project` or `global`.
    #[serde(default)]
    pub scope: Option<WriteScope>,
    /// タグ / tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// 明示的な相対パス（任意。project では <project_id>/pages/ 配下のみ）/ optional explicit
    /// path inside the scope.
    #[serde(default)]
    pub path: Option<String>,
}

/// Input of `kioku_handoff_write` (spec §7.5).
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandoffWriteParams {
    /// プロジェクト id（SessionStart で渡されたもの）/ project id from the SessionStart context.
    pub project: String,
    /// セッション id（SessionStart の `session:` 行。省略時はそのプロジェクトで最後に観測のあった
    /// 開いているセッション）/ session id from the SessionStart `session:` line; defaults to the
    /// open session of the project with the newest observation.
    #[serde(default)]
    pub session: Option<String>,
    /// 何をしたか・現在の状態の要約 / what was done and where things stand.
    pub summary: String,
    /// 次にやること（具体的に）/ concrete next steps.
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// 未解決の質問・懸念 / open questions.
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// 決定事項（理由も）/ decisions made, with reasons.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// 確認済みの事実（実際に確かめたこと）/ facts that were checked and hold.
    #[serde(default)]
    pub verified: Vec<String>,
    /// 落とし穴・注意点 / pitfalls the next session should know about.
    #[serde(default)]
    pub gotchas: Vec<String>,
}

/// Input of `kioku_handoff_pending`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandoffPendingParams {
    /// プロジェクト id / project id.
    pub project: String,
    /// true で受領済みにする（既定 false = 覗くだけ）/ mark as accepted (default: peek only).
    #[serde(default)]
    pub accept: bool,
    /// セッション id（任意。そのセッションのレーンを読み、accept 時の受領者になる）/ optional
    /// session id: reads its lane and is recorded as the acceptor.
    #[serde(default)]
    pub session: Option<String>,
    /// レーン＝ブランチ名（任意。省略時は session のレーン、無ければメインライン）/ optional lane
    /// (branch name); defaults to the session's lane, else the main line.
    #[serde(default)]
    pub lane: Option<String>,
    /// そのレーンの直近 N 件（最大 20）の引き継ぎを状態つきで返す / also return the lane's
    /// last N handoffs (≤ 20) with their status.
    #[serde(default)]
    pub history: Option<usize>,
}

/// The rmcp server handler exposing the six kioku tools.
#[derive(Clone)]
pub struct KiokuMcp {
    store: Arc<Store>,
    tool_router: ToolRouter<KiokuMcp>,
}

impl KiokuMcp {
    /// Creates the handler over a shared store.
    pub fn new(store: Arc<Store>) -> KiokuMcp {
        KiokuMcp {
            store,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl KiokuMcp {
    /// `kioku_query`: full-text search.
    #[tool(description = QUERY_DESC)]
    async fn kioku_query(&self, Parameters(p): Parameters<QueryParams>) -> Result<String, String> {
        let scope = resolve_scope(p.scope.map(QueryScope::as_str), p.project.as_deref())
            .map_err(err_text)?;
        let limit = clamp_limit(p.limit, DEFAULT_MCP_LIMIT);
        let request = QueryRequest::new(
            p.query,
            p.since.as_deref(),
            &p.kinds.unwrap_or_default(),
            p.path_prefix,
        )
        .map_err(err_text)?;
        let result = blocking(&self.store, move |s| request.run(s, &scope, limit))
            .await
            .map_err(err_text)?;
        Ok(format_query(&result))
    }

    /// `kioku_read`: one page.
    #[tool(description = READ_DESC)]
    async fn kioku_read(&self, Parameters(p): Parameters<ReadParams>) -> Result<String, String> {
        let page = blocking(&self.store, move |s| s.read_page(&p.path))
            .await
            .map_err(err_text)?;
        Ok(format_page(&page))
    }

    /// `kioku_write_page`: create or replace a page.
    #[tool(description = WRITE_PAGE_DESC)]
    async fn kioku_write_page(
        &self,
        Parameters(p): Parameters<WritePageParams>,
    ) -> Result<String, String> {
        let req = WritePageRequest {
            expected_revision: p.expected_revision,
            title: p.title,
            content: p.content,
            project: p.project,
            scope: p.scope.map(|s| match s {
                WriteScope::Project => PageScope::Project,
                WriteScope::Global => PageScope::Global,
            }),
            tags: p.tags,
            path: p.path,
        };
        let path = blocking(&self.store, move |s| {
            s.write_page(&req)
                .map_err(|e| with_project_hint(s, e, req.project.as_deref()))
        })
        .await
        .map_err(err_text)?;
        Ok(format!("wrote {path}"))
    }

    /// `kioku_handoff_write`: agent-written handoff.
    #[tool(description = HANDOFF_WRITE_DESC)]
    async fn kioku_handoff_write(
        &self,
        Parameters(p): Parameters<HandoffWriteParams>,
    ) -> Result<String, String> {
        let input = HandoffInput {
            project: p.project,
            session: p.session.filter(|s| !s.trim().is_empty()),
            summary: p.summary,
            next_steps: p.next_steps,
            open_questions: p.open_questions,
            decisions: p.decisions,
            gotchas: p.gotchas,
            verified: p.verified,
        };
        let handoff = blocking(&self.store, move |s| {
            s.write_handoff(&input)
                .map_err(|e| with_project_hint(s, e, Some(&input.project)))
        })
        .await
        .map_err(err_text)?;
        Ok(format!("handoff recorded for {}", handoff.project_id))
    }

    /// `kioku_handoff_pending`: peek at / accept the pending handoff.
    #[tool(description = HANDOFF_PENDING_DESC)]
    async fn kioku_handoff_pending(
        &self,
        Parameters(p): Parameters<HandoffPendingParams>,
    ) -> Result<String, String> {
        let session = p.session.filter(|s| !s.trim().is_empty());
        let history = p.history.unwrap_or(0);
        let routed = blocking(&self.store, move |s| {
            let mut routed = s.pending_handoff_routed(
                &p.project,
                p.accept,
                session.as_deref(),
                p.lane.as_deref(),
            )?;
            if history > 0 {
                routed.history =
                    s.handoff_history(&p.project, session.as_deref(), p.lane.as_deref(), history)?;
            }
            Ok(routed)
        })
        .await
        .map_err(err_text)?;
        Ok(format_pending(&routed))
    }

    /// `kioku_status`: counts and data dir.
    #[tool(description = STATUS_DESC)]
    async fn kioku_status(&self) -> Result<String, String> {
        let (status, projects) = blocking(&self.store, |s| {
            let status = s.status()?;
            let projects: Vec<String> = s.list_projects()?.into_iter().map(|p| p.id).collect();
            Ok((status, projects))
        })
        .await
        .map_err(err_text)?;
        Ok(format_status(&status, &projects))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KiokuMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kioku", VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// rmcp transport settings for `/mcp`. Host validation is off: the bearer token already
/// guards the endpoint, and a home server is reached by names we cannot know in advance.
pub fn transport_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default().disable_allowed_hosts()
}

/// The tower service mounted at `/mcp`.
pub fn service(
    store: Arc<Store>,
    config: StreamableHttpServerConfig,
) -> StreamableHttpService<KiokuMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(KiokuMcp::new(store.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Tool error text (the tool result is flagged `isError`).
fn err_text(err: Error) -> String {
    if let Error::Internal(e) = &err {
        tracing::error!(error = format!("{e:#}"), "mcp tool failed");
    }
    err.to_string()
}

/// A parsed `kioku_query` (SPEC-M3.1 §2–§3): full-text terms with filters, and/or a path
/// whose editing sessions are listed.
#[derive(Clone, Debug)]
pub struct QueryRequest {
    /// Search terms (may be empty when `path_prefix` is set).
    pub query: String,
    /// Filters of the full-text search.
    pub options: SearchOptions,
    /// List the sessions that edited files under this path.
    pub path_prefix: Option<String>,
}

/// What a `kioku_query` found.
#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    /// Full-text hits (`None` when no terms were given).
    pub hits: Option<Vec<Hit>>,
    /// `(prefix, sessions)` when `path_prefix` was given.
    pub sessions: Option<(String, Vec<PathSession>)>,
}

impl QueryRequest {
    /// Validates the parameters: terms or a path are required; `since` / `kinds` must parse.
    pub fn new(
        query: String,
        since: Option<&str>,
        kinds: &[String],
        path_prefix: Option<String>,
    ) -> Result<QueryRequest, Error> {
        let options = SearchOptions::parse(since, kinds).map_err(Error::invalid)?;
        let path_prefix = path_prefix.filter(|p| !p.trim().is_empty());
        if query.trim().is_empty() && path_prefix.is_none() {
            return Err(Error::invalid("query (or path_prefix) is required"));
        }
        Ok(QueryRequest {
            query,
            options,
            path_prefix,
        })
    }

    /// Runs it on the store (blocking).
    pub fn run(
        &self,
        store: &Store,
        scope: &SearchScope,
        limit: usize,
    ) -> Result<QueryResult, Error> {
        let sessions = match &self.path_prefix {
            Some(prefix) => {
                let project = match scope {
                    SearchScope::Project(id) => Some(id.as_str()),
                    _ => None,
                };
                Some((
                    prefix.clone(),
                    store.sessions_for_path(prefix, project, limit)?,
                ))
            }
            None => None,
        };
        let hits = if self.query.trim().is_empty() {
            None
        } else {
            Some(store.search_with(&self.query, scope, limit, &self.options)?)
        };
        Ok(QueryResult { hits, sessions })
    }
}

/// `kioku_query` output: the untrusted-memory note, the sessions of `path_prefix` (if
/// asked), then the hits (if terms were given).
pub fn format_query(r: &QueryResult) -> String {
    let mut out = memory_note();
    let mut parts = Vec::new();
    if let Some((prefix, sessions)) = &r.sessions {
        parts.push(path_sessions_body(prefix, sessions));
    }
    if let Some(hits) = &r.hits {
        parts.push(hits_body(hits));
    }
    out.push_str(&parts.join("\n\n"));
    out.trim_end().to_string()
}

/// The sessions that edited files under `prefix`, newest first (SPEC-M3.1 §3):
/// `N. <session page> — <title> (<date>, <agent>[@machine][, lane <lane>])`, then the files
/// and the handoff summary.
pub fn path_sessions_body(prefix: &str, sessions: &[PathSession]) -> String {
    let mut out = format!(
        "{prefix} を編集したセッション（新しい順） / sessions that edited {prefix}, newest first:\n"
    );
    if sessions.is_empty() {
        out.push_str("none");
        return out;
    }
    for (i, s) in sessions.iter().enumerate() {
        let who = match &s.machine {
            Some(m) => format!("{}@{m}", s.agent),
            None => s.agent.clone(),
        };
        let lane = s
            .lane
            .as_deref()
            .map(|l| format!(", lane {l}"))
            .unwrap_or_default();
        let title = if s.title.is_empty() { "-" } else { &s.title };
        out.push_str(&format!(
            "{}. {} — {title} ({}, {who}{lane})\n   files: {}\n",
            i + 1,
            s.path,
            s.date,
            s.files.join(", ")
        ));
        if let Some(summary) = &s.summary {
            out.push_str(&format!("   summary: {summary}\n"));
        }
    }
    out.trim_end().to_string()
}

/// `kioku_query` output: the untrusted-memory note, then numbered hits
/// `path — title (kind, YYYY-MM-DD[, @machine]) [global]` (SPEC-M3.0 §5, SPEC-M3.1 §2) +
/// indented snippet; a partial (character bigram) match is announced first.
pub fn format_hits(hits: &[Hit]) -> String {
    let mut out = memory_note();
    out.push_str(&hits_body(hits));
    out.trim_end().to_string()
}

/// [`format_hits`] without the memory note.
pub fn hits_body(hits: &[Hit]) -> String {
    let mut out = String::new();
    if hits.is_empty() {
        out.push_str("no hits");
        return out;
    }
    if hits.iter().any(|h| h.partial) {
        out.push_str(PARTIAL_MATCH_NOTE);
        out.push('\n');
    }
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {} ({}){}\n",
            i + 1,
            h.path,
            h.title,
            hit_meta(h),
            if h.global { " [global]" } else { "" }
        ));
        let snippet = h.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
        if !snippet.is_empty() {
            out.push_str(&format!("   {snippet}\n"));
        }
    }
    out.trim_end().to_string()
}

/// Line announcing that the hits only matched by characters (SPEC-M3.1 §2).
pub const PARTIAL_MATCH_NOTE: &str = "（部分一致）/ (partial match)";

/// `kind, YYYY-MM-DD[, @machine]` of a hit (SPEC-M3.0 §5, SPEC-M3.1 §2); just the kind when
/// the date is unknown.
pub fn hit_meta(h: &Hit) -> String {
    let date: String = h.updated.chars().take(10).collect();
    let mut meta = match (h.kind.is_empty(), date.len() == 10) {
        (false, true) => format!("{}, {date}", h.kind),
        (false, false) => h.kind.clone(),
        (true, true) => date,
        (true, false) => "-".to_string(),
    };
    if let Some(m) = h.machine.as_deref().filter(|m| !m.is_empty()) {
        meta.push_str(&format!(", @{m}"));
    }
    meta
}

/// `kioku_read` output: the untrusted-memory note, frontmatter summary, blank line, body.
pub fn format_page(page: &Page) -> String {
    let fm = &page.frontmatter;
    let mut out = memory_note();
    out.push_str(&format!("path: {}\ntitle: {}\n", page.path, fm.title));
    // An older server sends no revision: print none rather than an empty one to pass back.
    if !page.revision.is_empty() {
        out.push_str(&format!("revision: {}\n", page.revision));
    }
    out.push_str(&format!(
        "kind: {} / scope: {}",
        fm.kind.as_str(),
        fm.scope.as_str()
    ));
    if let Some(p) = &fm.project {
        out.push_str(&format!(" / project: {p}"));
    }
    out.push('\n');
    if !fm.tags.is_empty() {
        out.push_str(&format!("tags: {}\n", fm.tags.join(", ")));
    }
    out.push_str(&format!(
        "created: {} / updated: {}\n",
        fm.created, fm.updated
    ));
    if let Some(s) = &fm.session {
        out.push_str(&format!("session: {s}\n"));
    }
    if let Some(a) = &fm.agent {
        out.push_str(&format!("agent: {a}\n"));
    }
    out.push('\n');
    out.push_str(page.body.trim_end());
    out
}

/// `kioku_handoff_pending` output: one header line, blank line, the handoff Markdown.
pub fn format_handoff(h: &Handoff) -> String {
    let mut header = format!(
        "handoff {} (project: {}, source: {}, created: {}",
        h.id,
        h.project_id,
        h.source.as_str(),
        h.created_at
    );
    if let Some(agent) = &h.agent {
        header.push_str(&format!(", agent: {agent}"));
    }
    if let Some(at) = &h.accepted_at {
        header.push_str(&format!(", accepted: {at}"));
    }
    header.push(')');
    format!("{header}\n\n{}", h.content_md.trim_end())
}

/// Chars of each handoff in `kioku_handoff_pending(history)` output.
pub const HISTORY_ENTRY_MAX: usize = 1500;

/// `kioku_handoff_pending` output: the untrusted-memory note, then the handoff, else the
/// lane's or the main line's as a marked reference (M2.4 §1.4, SPEC-M3.1 §1), else `none`;
/// then the lane's history with each handoff's status when asked for.
pub fn format_pending(p: &PendingHandoff) -> String {
    let mut body = match (&p.handoff, &p.reference_handoff) {
        (Some(h), _) => format_handoff(h),
        (None, Some(r)) => {
            let label = match p.reference_reason.as_deref() {
                Some(REFERENCE_CONCURRENT) => {
                    "another session is active on this lane; the handoff was left pending / 同じブランチで別のセッションが作業中のため、引き継ぎは消費していません（受領するなら accept=true）"
                }
                Some(REFERENCE_RESUMED) => {
                    "pending handoff of this lane (for reference, not accepted) / このレーンの未受領の引き継ぎ（参考）"
                }
                _ => {
                    "no handoff on this lane. Main line handoff (for reference, not accepted) / メインの引き継ぎ（参考・未受領）"
                }
            };
            format!("{label}:\n\n{}", format_handoff(r))
        }
        (None, None) => "none".to_string(),
    };
    if !p.history.is_empty() {
        body.push_str(&format!(
            "\n\n## history, newest first / 履歴（新しい順、{} 件）",
            p.history.len()
        ));
        for h in &p.history {
            let text = kioku_core::util::truncate_chars(h.content_md.trim_end(), HISTORY_ENTRY_MAX);
            let lane = h
                .lane
                .as_deref()
                .map(|l| format!(", lane {l}"))
                .unwrap_or_default();
            body.push_str(&format!(
                "\n\n### {} — {} ({}, {}{lane})\n{text}",
                h.created_at,
                h.status(),
                h.source.as_str(),
                h.agent.as_deref().unwrap_or("-"),
            ));
        }
    }
    format!("{}{body}", memory_note())
}

/// `kioku_status` output.
pub fn format_status(s: &StatusReport, projects: &[String]) -> String {
    let mut out = format!(
        "kioku {VERSION}\ndata_dir: {}\nprojects: {}\npages: {}\nsessions: {}\nobservations: {}\nhandoffs: {}\nindex_docs: {}\ngit: {}",
        s.data_dir,
        s.projects,
        s.pages,
        s.sessions,
        s.observations,
        s.handoffs,
        s.index_docs,
        if s.git_enabled { "enabled" } else { "disabled" },
    );
    if !projects.is_empty() {
        out.push_str(&format!("\nproject ids: {}", projects.join(", ")));
    }
    if !s.aliases.is_empty() {
        out.push_str(&format!("\naliases: {}", format_aliases(&s.aliases)));
    }
    out
}

/// `alias → project, …` (M2.4 §2.2).
pub fn format_aliases(aliases: &[ProjectAlias]) -> String {
    aliases
        .iter()
        .map(|a| format!("{} → {}", a.alias, a.project_id))
        .collect::<Vec<_>>()
        .join(", ")
}
