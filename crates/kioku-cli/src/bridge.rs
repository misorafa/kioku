//! `kioku mcp` (M2 §20): a local MCP server on stdin/stdout that serves the same six tools
//! as the server's `/mcp`, each backed by one REST request through [`ApiClient`] — so MCP
//! gets the same connection robustness as hooks (§19.2), and agent config files hold
//! neither the server URL nor the token.

use std::time::Duration;

use anyhow::Context;
use kioku_core::{ClientConfig, Hit, Page, PendingHandoff, StatusReport};
use kioku_server::mcp::{
    HANDOFF_PENDING_DESC, HANDOFF_WRITE_DESC, HandoffPendingParams, HandoffWriteParams,
    INSTRUCTIONS, QUERY_DESC, QueryParams, READ_DESC, ReadParams, STATUS_DESC, WRITE_PAGE_DESC,
    WritePageParams, WriteScope, format_hits, format_page, format_pending, format_status,
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde_json::{Value, json};

use crate::client::{ApiClient, HttpError};

/// Deadline of one tool call's REST request.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `kioku_query`'s default number of hits (the server's MCP default).
const QUERY_DEFAULT_LIMIT: usize = 8;

/// The stdio MCP handler: the server's six tools, over REST.
#[derive(Clone)]
pub struct KiokuBridge {
    client: ClientConfig,
    tool_router: ToolRouter<KiokuBridge>,
}

impl KiokuBridge {
    /// A bridge to the server in `client` (`[client] server_url` + token).
    pub fn new(client: ClientConfig) -> KiokuBridge {
        KiokuBridge {
            client,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs one REST call on the blocking pool; errors become the tool's error text.
    async fn call(
        &self,
        f: impl FnOnce(&ApiClient) -> anyhow::Result<Value> + Send + 'static,
    ) -> Result<Value, String> {
        let client = self.client.clone();
        let url = client.server_url.clone();
        tokio::task::spawn_blocking(move || {
            let api = ApiClient::new(&client, CALL_TIMEOUT)?;
            f(&api)
        })
        .await
        .map_err(|e| format!("kioku mcp: {e}"))?
        .map_err(|e| match e.downcast_ref::<HttpError>() {
            Some(h) => h.message.clone(),
            None => format!("kioku server {url} unreachable: {e:#}"),
        })
    }
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("unexpected response from the kioku server: {e}"))
}

#[tool_router]
impl KiokuBridge {
    /// `kioku_query` → `GET /search`.
    #[tool(description = QUERY_DESC)]
    async fn kioku_query(&self, Parameters(p): Parameters<QueryParams>) -> Result<String, String> {
        let mut query = vec![
            ("q", p.query),
            ("limit", p.limit.unwrap_or(QUERY_DEFAULT_LIMIT).to_string()),
        ];
        if let Some(project) = p.project {
            query.push(("project", project));
        }
        if let Some(scope) = p.scope {
            query.push(("scope", scope.as_str().to_string()));
        }
        let v = self.call(move |c| c.get(&["search"], &query)).await?;
        let hits: Vec<Hit> = decode(v.get("hits").cloned().unwrap_or(json!([])))?;
        Ok(format_hits(&hits))
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
            "title": p.title,
            "content": p.content,
            "project": p.project,
            "scope": p.scope.map(|s| match s {
                WriteScope::Project => "project",
                WriteScope::Global => "global",
            }),
            "tags": p.tags,
            "path": p.path,
        });
        let v = self.call(move |c| c.put(&["pages"], &body)).await?;
        Ok(format!(
            "wrote {}",
            v.get("path").and_then(Value::as_str).unwrap_or_default()
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
        let v = self
            .call(move |c| c.get(&["handoffs", "pending"], &query))
            .await?;
        // `reference_handoff` is additive (M2.4 §1.4); an older server sends `handoff` only.
        let routed: PendingHandoff = decode(v)?;
        Ok(format_pending(&routed))
    }

    /// `kioku_status` → `GET /status`.
    #[tool(description = STATUS_DESC)]
    async fn kioku_status(&self) -> Result<String, String> {
        let v = self.call(|c| c.get(&["status"], &[])).await?;
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

/// `kioku mcp`: serves the bridge on stdin/stdout until the agent closes it.
pub fn run(client: ClientConfig) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(async move {
        let service = KiokuBridge::new(client)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP stdio server")?;
        service.waiting().await.context("serving MCP on stdio")?;
        Ok(())
    })
}
