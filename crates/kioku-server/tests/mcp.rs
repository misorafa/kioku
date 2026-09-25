//! MCP over streamable HTTP, driven by rmcp's own client with the bearer header.

mod common;

use common::{PROJECT, TOKEN, spawn, start_body};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::json;

fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn mcp_tools_over_streamable_http() {
    let srv = spawn().await;
    // register the project and put something searchable in the wiki
    let (status, _) = srv
        .post("/api/v1/sessions/start", start_body("s-mcp"))
        .await;
    assert_eq!(status, 200);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.url("/mcp")).auth_header(TOKEN),
    );
    let client = ().serve(transport).await.expect("initialize");

    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.clone().unwrap().name, "kioku");
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(instructions.contains("kioku_query"));
    assert!(instructions.contains("kioku_handoff_write"));

    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "kioku_handoff_pending",
            "kioku_handoff_write",
            "kioku_query",
            "kioku_read",
            "kioku_status",
            "kioku_write_page",
        ]
    );
    for t in &tools {
        let desc = t.description.as_deref().unwrap_or_default();
        assert!(!desc.is_empty() && desc.is_char_boundary(0), "{}", t.name);
        assert!(desc.lines().count() >= 2, "{}: ja + en lines", t.name);
    }

    // kioku_status
    let status = client
        .call_tool(CallToolRequestParams::new("kioku_status"))
        .await
        .unwrap();
    assert_ne!(status.is_error, Some(true));
    let out = text(&status);
    assert!(out.contains("projects: 1"), "{out}");
    assert!(out.contains(&format!("project ids: {PROJECT}")), "{out}");
    assert!(out.contains(&srv.dir.path().display().to_string()), "{out}");

    // kioku_write_page with an unknown project lists the known ids
    let bad = client
        .call_tool(
            CallToolRequestParams::new("kioku_write_page").with_arguments(args(json!({
                "title": "x", "content": "y", "project": "wrong-project"
            }))),
        )
        .await
        .unwrap();
    assert_eq!(bad.is_error, Some(true));
    let out = text(&bad);
    assert!(
        out.contains("wrong-project") && out.contains(PROJECT),
        "{out}"
    );

    // kioku_write_page then kioku_query (Japanese) then kioku_read
    let wrote = client
        .call_tool(
            CallToolRequestParams::new("kioku_write_page").with_arguments(args(json!({
                "title": "引き継ぎの設計",
                "content": "引き継ぎ書を毎回作るのが手間なので自動化したい",
                "project": PROJECT,
                "tags": ["design"]
            }))),
        )
        .await
        .unwrap();
    assert_ne!(wrote.is_error, Some(true), "{}", text(&wrote));
    let out = text(&wrote);
    let path = out.strip_prefix("wrote ").expect(&out).to_string();
    assert!(path.starts_with(&format!("{PROJECT}/pages/")), "{path}");

    let query = client
        .call_tool(
            CallToolRequestParams::new("kioku_query").with_arguments(args(json!({
                "query": "引き継ぎ 手間",
                "project": PROJECT
            }))),
        )
        .await
        .unwrap();
    assert_ne!(query.is_error, Some(true));
    let out = text(&query);
    assert!(
        out.starts_with(&format!("1. {path} — 引き継ぎの設計 (")),
        "{out}"
    );
    assert!(out.contains("【"), "{out}");
    let none = client
        .call_tool(
            CallToolRequestParams::new("kioku_query")
                .with_arguments(args(json!({"query": "Postgres"}))),
        )
        .await
        .unwrap();
    assert_eq!(text(&none), "no hits");

    let read = client
        .call_tool(
            CallToolRequestParams::new("kioku_read").with_arguments(args(json!({"path": path}))),
        )
        .await
        .unwrap();
    let out = text(&read);
    assert!(out.contains("title: 引き継ぎの設計"), "{out}");
    assert!(out.contains("tags: design"), "{out}");
    assert!(out.ends_with("引き継ぎ書を毎回作るのが手間なので自動化したい"));

    // handoff write → pending (peek) → accept
    let wrote = client
        .call_tool(
            CallToolRequestParams::new("kioku_handoff_write").with_arguments(args(json!({
                "project": PROJECT,
                "summary": "MCP ツールを実装した",
                "next_steps": ["CLI を作る"],
                "decisions": ["rmcp 3.4 を使う"]
            }))),
        )
        .await
        .unwrap();
    assert_eq!(text(&wrote), format!("handoff recorded for {PROJECT}"));
    let peek = client
        .call_tool(
            CallToolRequestParams::new("kioku_handoff_pending")
                .with_arguments(args(json!({"project": PROJECT}))),
        )
        .await
        .unwrap();
    let out = text(&peek);
    assert!(out.contains("source: agent"), "{out}");
    assert!(out.contains("### 要約\nMCP ツールを実装した"), "{out}");
    let accept = client
        .call_tool(
            CallToolRequestParams::new("kioku_handoff_pending")
                .with_arguments(args(json!({"project": PROJECT, "accept": true}))),
        )
        .await
        .unwrap();
    assert!(text(&accept).contains("accepted: "));
    let after = client
        .call_tool(
            CallToolRequestParams::new("kioku_handoff_pending")
                .with_arguments(args(json!({"project": PROJECT}))),
        )
        .await
        .unwrap();
    assert_eq!(text(&after), "none");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mcp_rejects_missing_token() {
    let srv = spawn().await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.url("/mcp")),
    );
    assert!(().serve(transport).await.is_err());
}
