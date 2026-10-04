//! `kioku mcp` (M2 §20): a local MCP server on stdin/stdout that serves the same six tools
//! as the server's `/mcp`, each backed by one REST request through [`ApiClient`] — so MCP
//! gets the same connection robustness as hooks (§19.2), and agent config files hold
//! neither the server URL nor the token. The bridge lives as long as the agent session, so it
//! re-reads config.toml before every call: a token rotated (or a server re-pointed) by
//! `kioku rotate-token`, `setup` or `invite` mid-session must not leave it sending a stale
//! token and failing every call with 401 until the agent restarts.
//!
//! Desktop apps (Claude.app, Codex.app) run no hooks, so the bridge also does the hooks'
//! two machine-level chores (SPEC-M3.4 §1): it follows the server's version — once per
//! process, after the first successful tool call, off the call's path, through
//! [`crate::auto_update::after_session_start`] — and records each successful call in
//! `state/last-hook.json` under `mcp`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use kioku_core::{ClientConfig, Config, Hit, Page, PathSession, PendingHandoff, StatusReport};
use kioku_server::mcp::{
    HANDOFF_PENDING_DESC, HANDOFF_WRITE_DESC, HandoffPendingParams, HandoffWriteParams,
    INSTRUCTIONS, QUERY_DESC, QueryParams, QueryRequest, QueryResult, READ_DESC, ReadParams,
    STATUS_DESC, WRITE_PAGE_DESC, WritePageParams, WriteScope, format_page, format_pending,
    format_query, format_status,
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde_json::{Value, json};

use crate::client::{ApiClient, HttpError};
use crate::event::HookEnv;

/// Deadline of one tool call's REST request.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `kioku_query`'s default number of hits (the server's MCP default).
const QUERY_DEFAULT_LIMIT: usize = 8;

/// Tool error when the server predates `path_prefix` (SPEC-M3.1 §3).
pub const PATH_PREFIX_UNSUPPORTED: &str = "this kioku server predates path_prefix and cannot list sessions by path; update the server (`kioku update` on it) / サーバーが古く path_prefix に対応していません（サーバーで kioku update を実行）";

/// What a bridge started by an app (`kioku mcp`) keeps for following the server's version
/// and for its liveness entry (SPEC-M3.4 §1).
struct Follow {
    /// Environment, home directory and data directory of this machine's kioku state.
    env: HookEnv,
    /// Set once the version check has started: it runs at most once per process.
    started: AtomicBool,
    /// The update notice (auto off / winget / brew / not writable) not yet shown; it is
    /// appended to the next `kioku_query` / `kioku_handoff_pending` result.
    notice: parking_lot::Mutex<Option<String>>,
    /// Serializes this process's writes of `last-hook.json` (they share one temp name).
    liveness: parking_lot::Mutex<()>,
}

/// The stdio MCP handler: the server's six tools, over REST.
#[derive(Clone)]
pub struct KiokuBridge {
    /// The `[client]` section in use when config is not re-read (or a re-read fails).
    client: ClientConfig,
    /// Environment to re-read config.toml with before each call (`KIOKU_*` overrides still
    /// apply); `None` = always use `client`.
    env: Option<Arc<HashMap<String, String>>>,
    /// Version following and liveness; `None` for a bridge with a fixed config (tests,
    /// embedding), which never touches the machine's state or starts an updater.
    follow: Option<Arc<Follow>>,
    tool_router: ToolRouter<KiokuBridge>,
}

impl KiokuBridge {
    /// A bridge to the server in `client` (`[client] server_url` + token), fixed for its life.
    pub fn new(client: ClientConfig) -> KiokuBridge {
        KiokuBridge {
            client,
            env: None,
            follow: None,
            tool_router: Self::tool_router(),
        }
    }

    /// A bridge that loads config with `env` now and re-reads it before every call.
    pub fn from_env(env: HashMap<String, String>) -> anyhow::Result<KiokuBridge> {
        let client = Config::load_with_env(&env)?.client;
        let follow = Follow {
            env: HookEnv {
                vars: env.clone(),
                home: kioku_core::util::home_dir_opt(),
                cwd: None,
            },
            started: AtomicBool::new(false),
            notice: parking_lot::Mutex::new(None),
            liveness: parking_lot::Mutex::new(()),
        };
        Ok(KiokuBridge {
            env: Some(Arc::new(env)),
            follow: Some(Arc::new(follow)),
            ..KiokuBridge::new(client)
        })
    }

    /// The `[client]` config for the next call: config.toml as it is now, else the last copy.
    fn current_client(&self) -> ClientConfig {
        let Some(env) = &self.env else {
            return self.client.clone();
        };
        match Config::load_with_env(env) {
            Ok(cfg) => cfg.client,
            Err(e) => {
                tracing::warn!(
                    "kioku mcp: re-reading config failed, using the startup copy: {e:#}"
                );
                self.client.clone()
            }
        }
    }

    /// Runs one REST call on the blocking pool; errors become the tool's error text.
    async fn call(
        &self,
        f: impl FnOnce(&ApiClient) -> anyhow::Result<Value> + Send + 'static,
    ) -> Result<Value, String> {
        self.call_hinted(f, false).await
    }

    /// [`KiokuBridge::call`]; `is_status` = the response is `GET /status`, whose `version`
    /// spares the version check its own request.
    async fn call_hinted(
        &self,
        f: impl FnOnce(&ApiClient) -> anyhow::Result<Value> + Send + 'static,
        is_status: bool,
    ) -> Result<Value, String> {
        let result = self.request(f).await;
        if let Ok(v) = &result {
            let version = is_status
                .then(|| v.get("version").and_then(Value::as_str))
                .flatten()
                .map(str::to_string);
            self.after_success(version);
        }
        result
    }

    /// After a successful call (SPEC-M3.4 §1), both on a spawned task so the call's result
    /// is never held up: the liveness entry, and — the first time in this process — the
    /// version check.
    fn after_success(&self, status_version: Option<String>) {
        let Some(follow) = self.follow.clone() else {
            return;
        };
        let bridge = self.clone();
        let first = !follow.started.swap(true, Ordering::SeqCst);
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                bridge.record_liveness();
                if first {
                    bridge.follow_server(status_version);
                }
            })
            .await;
        });
    }

    /// The machine's config as it is now (`None` for a fixed bridge or a broken file).
    fn current_config(&self) -> Option<Config> {
        Config::load_with_env(self.env.as_deref()?).ok()
    }

    /// `state/last-hook.json` ← `mcp` / `tool_call` (SPEC-M3.2 §2, SPEC-M3.4 §1).
    fn record_liveness(&self) {
        let (Some(follow), Some(cfg)) = (&self.follow, self.current_config()) else {
            return;
        };
        if let Some(path) = crate::liveness::liveness_path(&cfg, &follow.env) {
            let _one_writer = follow.liveness.lock();
            crate::liveness::record_named(
                &path,
                crate::liveness::MCP_ENTRY,
                crate::liveness::MCP_EVENT,
                true,
                &kioku_core::util::now_ts(),
            );
        }
    }

    /// The SessionStart hook's client update decision, for a bridge (blocking): the server
    /// version from `status_version` or a one-off `GET /status`, then
    /// [`crate::auto_update::after_session_start`], and a detached `kioku update
    /// --background` when one is due or a notice for the next query result. When the server
    /// cannot be asked, the check is left for the next successful call.
    fn follow_server(&self, status_version: Option<String>) {
        let Some(follow) = &self.follow else {
            return;
        };
        let version = status_version.filter(|v| !v.is_empty()).or_else(|| {
            let client = self.current_client();
            ApiClient::new(&client, CALL_TIMEOUT)
                .and_then(|api| api.get(&["status"], &[]))
                .ok()
                .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
                .filter(|v| !v.is_empty())
        });
        let (Some(version), Some(cfg)) = (version, self.current_config()) else {
            follow.started.store(false, Ordering::SeqCst);
            return;
        };
        let (notice, spawn) =
            crate::auto_update::after_session_start(&cfg, &follow.env, Some(&version));
        if let Some(line) = notice {
            *follow.notice.lock() = Some(line);
        }
        if let Some(tag) = spawn
            && let Err(err) = crate::auto_update::spawn_background(&tag)
        {
            let root = crate::hook::client_state_root(&cfg, &follow.env);
            crate::auto_update::log_update(root.as_deref(), &format!("kioku: {err:#}"));
        }
    }

    /// `out` with the pending update notice appended, once (SPEC-M3.4 §1).
    fn with_notice(&self, mut out: String) -> String {
        if let Some(line) = self.follow.as_ref().and_then(|f| f.notice.lock().take()) {
            out.push_str(&format!("\n\nkioku: {line}"));
        }
        out
    }

    /// One REST call on the blocking pool with the config as it is now.
    async fn request(
        &self,
        f: impl FnOnce(&ApiClient) -> anyhow::Result<Value> + Send + 'static,
    ) -> Result<Value, String> {
        let bridge = self.clone();
        tokio::task::spawn_blocking(move || {
            let client = bridge.current_client();
            let url = client.server_url.clone();
            ApiClient::new(&client, CALL_TIMEOUT)
                .and_then(|api| f(&api))
                .map_err(|e| match e.downcast_ref::<HttpError>() {
                    Some(h) => h.message.clone(),
                    None => format!("kioku server {url} unreachable: {e:#}"),
                })
        })
        .await
        .map_err(|e| format!("kioku mcp: {e}"))?
    }
}

/// A server older than SPEC-M3.4 ignores `slug`: say so when the written path is not
/// `…/<slug>.md`.
pub fn slug_ignored_note(path: &str, slug: Option<&str>) -> Option<String> {
    let slug = slug?;
    let expected = format!("/{slug}.md");
    (!path.ends_with(&expected)).then(|| {
        "kioku: this kioku server predates slug and named the page from its title; update the server (`kioku update` on it) / サーバーが古く slug に未対応のため、タイトルから名前を付けました".to_string()
    })
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("unexpected response from the kioku server: {e}"))
}

#[tool_router]
impl KiokuBridge {
    /// `kioku_query` → `GET /search`.
    #[tool(description = QUERY_DESC)]
    async fn kioku_query(&self, Parameters(p): Parameters<QueryParams>) -> Result<String, String> {
        let kinds = p.kinds.unwrap_or_default();
        // Validated here too, so a bad value reads the same as from the server's `/mcp`.
        let request = QueryRequest::new(p.query, p.since.as_deref(), &kinds, p.path_prefix)
            .map_err(|e| e.to_string())?;
        let mut query = vec![
            ("q", request.query.clone()),
            ("limit", p.limit.unwrap_or(QUERY_DEFAULT_LIMIT).to_string()),
        ];
        if let Some(project) = p.project {
            query.push(("project", project));
        }
        if let Some(scope) = p.scope {
            query.push(("scope", scope.as_str().to_string()));
        }
        if let Some(since) = p.since.filter(|s| !s.trim().is_empty()) {
            query.push(("since", since));
        }
        if !request.options.kinds.is_empty() {
            query.push(("kinds", request.options.kinds.join(",")));
        }
        if let Some(prefix) = &request.path_prefix {
            query.push(("path_prefix", prefix.clone()));
        }
        let v = self.call(move |c| c.get(&["search"], &query)).await?;
        let hits: Vec<Hit> = decode(v.get("hits").cloned().unwrap_or(json!([])))?;
        let mut result = QueryResult {
            hits: (!request.query.trim().is_empty()).then_some(hits),
            sessions: None,
        };
        if let Some(prefix) = request.path_prefix {
            match v.get("sessions") {
                Some(sessions) => {
                    let sessions: Vec<PathSession> = decode(sessions.clone())?;
                    result.sessions = Some((prefix, sessions));
                }
                // A server older than SPEC-M3.1 ignores `path_prefix`.
                None => return Err(PATH_PREFIX_UNSUPPORTED.to_string()),
            }
        }
        Ok(self.with_notice(format_query(&result)))
    }

    /// `kioku_read` → `GET /pages/<path>`.
    #[tool(description = READ_DESC)]
    async fn kioku_read(&self, Parameters(p): Parameters<ReadParams>) -> Result<String, String> {
        let v = self
            .call(move |c| {
                let mut segments = vec!["pages"];
                segments.extend(p.path.split('/').filter(|s| !s.is_empty()));
                c.get(&segments, &[])
            })
            .await?;
        let page: Page = decode(v)?;
        Ok(format_page(&page))
    }

    /// `kioku_write_page` → `PUT /pages`.
    #[tool(description = WRITE_PAGE_DESC)]
    async fn kioku_write_page(
        &self,
        Parameters(p): Parameters<WritePageParams>,
    ) -> Result<String, String> {
        let body = json!({
            "expected_revision": p.expected_revision,
            "title": p.title,
            "content": p.content,
            "project": p.project,
            "scope": p.scope.map(|s| match s {
                WriteScope::Project => "project",
                WriteScope::Global => "global",
            }),
            "tags": p.tags,
            "path": p.path,
            "slug": p.slug,
        });
        let slug = p
            .slug
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        let v = self.call(move |c| c.put(&["pages"], &body)).await?;
        let path = v.get("path").and_then(Value::as_str).unwrap_or_default();
        Ok(slug_ignored_note(path, slug.as_deref()).map_or_else(
            || format!("wrote {path}"),
            |note| format!("wrote {path}\n{note}"),
        ))
    }

    /// `kioku_handoff_write` → `POST /handoffs`.
    #[tool(description = HANDOFF_WRITE_DESC)]
    async fn kioku_handoff_write(
        &self,
        Parameters(p): Parameters<HandoffWriteParams>,
    ) -> Result<String, String> {
        let project = p.project.clone();
        let body = json!({
            "project": p.project,
            "session": p.session.filter(|s| !s.trim().is_empty()),
            "summary": p.summary,
            "next_steps": p.next_steps,
            "open_questions": p.open_questions,
            "decisions": p.decisions,
            "verified": p.verified,
            "gotchas": p.gotchas,
        });
        let v = self.call(move |c| c.post(&["handoffs"], &body)).await?;
        // `project_id` is additive (M2 §20.1); an older server only returns `id`.
        let id = v
            .get("project_id")
            .and_then(Value::as_str)
            .map_or(project, str::to_string);
        Ok(format!("handoff recorded for {id}"))
    }

    /// `kioku_handoff_pending` → `GET /handoffs/pending`.
    #[tool(description = HANDOFF_PENDING_DESC)]
    async fn kioku_handoff_pending(
        &self,
        Parameters(p): Parameters<HandoffPendingParams>,
    ) -> Result<String, String> {
        let mut query = vec![("project", p.project), ("accept", p.accept.to_string())];
        if let Some(s) = p.session.filter(|s| !s.trim().is_empty()) {
            query.push(("session", s));
        }
        if let Some(l) = p.lane {
            query.push(("lane", l));
        }
        if let Some(n) = p.history.filter(|n| *n > 0) {
            query.push(("history", n.to_string()));
        }
        let v = self
            .call(move |c| c.get(&["handoffs", "pending"], &query))
            .await?;
        // `reference_handoff` is additive (M2.4 §1.4); an older server sends `handoff` only.
        let routed: PendingHandoff = decode(v)?;
        Ok(self.with_notice(format_pending(&routed)))
    }

    /// `kioku_status` → `GET /status`.
    #[tool(description = STATUS_DESC)]
    async fn kioku_status(&self) -> Result<String, String> {
        let v = self.call_hinted(|c| c.get(&["status"], &[]), true).await?;
        // `project_ids` is additive (M2 §20.1).
        let projects: Vec<String> = v
            .get("project_ids")
            .cloned()
            .map(decode)
            .transpose()?
            .unwrap_or_default();
        let status: StatusReport = decode(v)?;
        Ok(format_status(&status, &projects))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KiokuBridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kioku", kioku_core::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// `kioku mcp`: serves the bridge (re-reading config with `env`) on stdin/stdout until the
/// agent closes it.
pub fn run(env: HashMap<String, String>) -> anyhow::Result<()> {
    let bridge = KiokuBridge::from_env(env)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(async move {
        let service = bridge
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP stdio server")?;
        service.waiting().await.context("serving MCP on stdio")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &std::path::Path, token: &str) {
        std::fs::write(
            dir.join("config.toml"),
            format!("[client]\nserver_url = \"http://mini:7391\"\nauth_token = \"{token}\"\n"),
        )
        .unwrap();
    }

    #[test]
    fn picks_up_a_token_rotated_mid_session() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "old");
        let env = HashMap::from([(
            "KIOKU_DATA_DIR".to_string(),
            dir.path().display().to_string(),
        )]);
        let bridge = KiokuBridge::from_env(env).unwrap();
        assert_eq!(bridge.current_client().auth_token.as_deref(), Some("old"));

        write_config(dir.path(), "new");
        assert_eq!(bridge.current_client().auth_token.as_deref(), Some("new"));
    }

    #[test]
    fn a_fixed_bridge_never_re_reads() {
        let bridge = KiokuBridge::new(ClientConfig {
            auth_token: Some("fixed".into()),
            ..ClientConfig::default()
        });
        assert_eq!(bridge.current_client().auth_token.as_deref(), Some("fixed"));
    }

    #[test]
    fn falls_back_to_the_startup_config_when_the_file_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "old");
        let env = HashMap::from([(
            "KIOKU_DATA_DIR".to_string(),
            dir.path().display().to_string(),
        )]);
        let bridge = KiokuBridge::from_env(env).unwrap();

        std::fs::write(dir.path().join("config.toml"), "[client\n").unwrap();
        assert_eq!(bridge.current_client().auth_token.as_deref(), Some("old"));
    }

    /// SPEC-M3.4 §2: a server that ignored `slug` (it predates it) is called out.
    #[test]
    fn a_server_that_ignores_slug_is_called_out() {
        assert_eq!(
            slug_ignored_note("p/pages/write-test-2.md", Some("write-test-2")),
            None
        );
        assert_eq!(slug_ignored_note("p/pages/page-4565ee.md", None), None);
        let note = slug_ignored_note("p/pages/page-4565ee.md", Some("write-test-2")).unwrap();
        assert!(note.contains("predates slug") && note.contains("slug に未対応"));
    }
}
