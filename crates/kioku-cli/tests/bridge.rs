//! `kioku mcp` (M2 §20): the stdio bridge serves exactly the server's `/mcp` tools, and every
//! tool works through REST against a real server — including over the actual binary's stdio.

use kioku_core::strings::memory_note;
use std::sync::Arc;

use kioku_cli::bridge::KiokuBridge;
use kioku_core::{ClientConfig, Config, SessionStartRequest, Store};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, Tool},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};

const TOKEN: &str = "bridge-token-0123456789";
const PROJECT: &str = "bridge-proj";

/// A real server over a temp data dir with `PROJECT` registered; returns its base URL.
async fn start_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let req: SessionStartRequest = serde_json::from_value(json!({
        "session_id": "s-bridge", "agent": "claude-code", "cwd": "/home/u/bridge",
        "source": "startup",
        "project": {"id": PROJECT, "name": "bridge", "root": "/home/u/bridge", "remote": null}
    }))
    .unwrap();
    store.start_session(&req).unwrap();
    let app = kioku_server::build_app(store, TOKEN.to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), dir)
}

fn client_cfg(base: &str) -> ClientConfig {
    ClientConfig {
        server_url: base.to_string(),
        auth_token: Some(TOKEN.to_string()),
        ..ClientConfig::default()
    }
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn by_name(mut tools: Vec<Tool>) -> Vec<(String, Option<String>, Value)> {
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
        .into_iter()
        .map(|t| {
            (
                t.name.to_string(),
                t.description.map(|d| d.to_string()),
                serde_json::to_value(&*t.input_schema).unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_serves_the_server_tools_over_rest() {
    let (base, _dir) = start_server().await;

    // The bridge, in-process over a duplex pipe.
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let svc = KiokuBridge::new(client_cfg(&base.clone()))
            .serve(tokio::io::split(ours))
            .await
            .unwrap();
        let _ = svc.waiting().await;
    });
    let bridge = ().serve(tokio::io::split(theirs)).await.expect("initialize");

    // The server's own /mcp.
    let (base, _dir2) = start_server().await;
    let http = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(TOKEN),
    );
    let direct = ().serve(http).await.expect("initialize /mcp");

    // Identical tools (names, descriptions, input schemas) and instructions.
    assert_eq!(
        by_name(bridge.list_all_tools().await.unwrap()),
        by_name(direct.list_all_tools().await.unwrap())
    );
    assert_eq!(
        bridge.peer_info().unwrap().instructions,
        direct.peer_info().unwrap().instructions
    );
    direct.cancel().await.unwrap();

    let call = |name: &'static str, a: Value| {
        let p = CallToolRequestParams::new(name);
        if a.is_null() {
            p
        } else {
            p.with_arguments(args(a))
        }
    };

    let status = bridge
        .call_tool(call("kioku_status", Value::Null))
        .await
        .unwrap();
    let out = text(&status);
    assert!(out.contains(&format!("project ids: {PROJECT}")), "{out}");

    let bad = bridge
        .call_tool(call(
            "kioku_write_page",
            json!({"title": "x", "content": "y", "project": "wrong-project"}),
        ))
        .await
        .unwrap();
    assert_eq!(bad.is_error, Some(true));
    assert!(text(&bad).contains("wrong-project") && text(&bad).contains(PROJECT));

    let wrote = bridge
        .call_tool(call(
            "kioku_write_page",
            json!({"title": "引き継ぎの設計", "content": "引き継ぎ書を毎回作るのが手間なので自動化したい",
                   "project": PROJECT, "tags": ["design"]}),
        ))
        .await
        .unwrap();
    let path = text(&wrote).strip_prefix("wrote ").unwrap().to_string();
    assert!(path.starts_with(&format!("{PROJECT}/pages/")), "{path}");

    let hits = bridge
        .call_tool(call(
            "kioku_query",
            json!({"query": "引き継ぎ 手間", "project": PROJECT}),
        ))
        .await
        .unwrap();
    assert!(
        text(&hits).starts_with(&format!("{}1. {path} — 引き継ぎの設計 (", memory_note())),
        "{}",
        text(&hits)
    );
    let none = bridge
        .call_tool(call("kioku_query", json!({"query": "Postgres"})))
        .await
        .unwrap();
    assert_eq!(text(&none), format!("{}no hits", memory_note()));

    let read = bridge
        .call_tool(call("kioku_read", json!({"path": path})))
        .await
        .unwrap();
    let out = text(&read);
    assert!(
        out.contains("title: 引き継ぎの設計") && out.contains("tags: design"),
        "{out}"
    );
    assert!(
        out.ends_with("引き継ぎ書を毎回作るのが手間なので自動化したい"),
        "{out}"
    );
    let revision = out
        .lines()
        .find_map(|line| line.strip_prefix("revision: "))
        .unwrap();
    let conditional = json!({"title":"引き継ぎの設計", "content":"日本語の変更", "project":PROJECT, "path":path, "expected_revision":revision});
    let updated = bridge
        .call_tool(call("kioku_write_page", conditional.clone()))
        .await
        .unwrap();
    assert_ne!(updated.is_error, Some(true));
    let stale = bridge
        .call_tool(call("kioku_write_page", conditional))
        .await
        .unwrap();
    assert_eq!(stale.is_error, Some(true));
    assert!(text(&stale).contains("conflict"));
    let missing = bridge
        .call_tool(call("kioku_read", json!({"path": "nope/none.md"})))
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true));

    let hw = bridge
        .call_tool(call(
            "kioku_handoff_write",
            json!({"project": PROJECT, "summary": "stdio ブリッジを実装した",
                   "next_steps": ["インストーラを切り替える"]}),
        ))
        .await
        .unwrap();
    assert_eq!(text(&hw), format!("handoff recorded for {PROJECT}"));
    let peek = bridge
        .call_tool(call("kioku_handoff_pending", json!({"project": PROJECT})))
        .await
        .unwrap();
    assert!(
        text(&peek).contains("### 要約\nstdio ブリッジを実装した"),
        "{}",
        text(&peek)
    );
    let accept = bridge
        .call_tool(call(
            "kioku_handoff_pending",
            json!({"project": PROJECT, "accept": true}),
        ))
        .await
        .unwrap();
    assert!(text(&accept).contains("accepted: "));
    let after = bridge
        .call_tool(call("kioku_handoff_pending", json!({"project": PROJECT})))
        .await
        .unwrap();
    assert_eq!(text(&after), format!("{}none", memory_note()));

    // lanes (M2.4 §1.4): a branch without its own handoff gets the main line's as a
    // reference, which stays pending
    bridge
        .call_tool(call(
            "kioku_handoff_write",
            json!({"project": PROJECT, "summary": "メインの引き継ぎ"}),
        ))
        .await
        .unwrap();
    let lane = bridge
        .call_tool(call(
            "kioku_handoff_pending",
            json!({"project": PROJECT, "lane": "feature/検索", "accept": true}),
        ))
        .await
        .unwrap();
    let out = text(&lane);
    assert!(
        out.starts_with(&format!("{}no handoff on this lane.", memory_note())),
        "{out}"
    );
    assert!(out.contains("メインの引き継ぎ"), "{out}");
    let main = bridge
        .call_tool(call("kioku_handoff_pending", json!({"project": PROJECT})))
        .await
        .unwrap();
    assert!(text(&main).contains("メインの引き継ぎ"));
    assert!(!text(&main).contains("accepted: "));

    // SPEC-M3.1: filters, path_prefix and history go through REST too
    let filtered = bridge
        .call_tool(call(
            "kioku_query",
            json!({"query": "引き継ぎ", "kinds": ["page"], "since": "2000-01-01", "project": PROJECT}),
        ))
        .await
        .unwrap();
    assert!(
        text(&filtered).contains("— 引き継ぎの設計 (page, "),
        "{}",
        text(&filtered)
    );
    let later = bridge
        .call_tool(call(
            "kioku_query",
            json!({"query": "引き継ぎ", "since": "2999-01-01"}),
        ))
        .await
        .unwrap();
    assert!(text(&later).ends_with("no hits"), "{}", text(&later));
    let bad = bridge
        .call_tool(call(
            "kioku_query",
            json!({"query": "引き継ぎ", "kinds": ["メモ"]}),
        ))
        .await
        .unwrap();
    assert_eq!(bad.is_error, Some(true));
    let by_path = bridge
        .call_tool(call(
            "kioku_query",
            json!({"path_prefix": "src/検索.rs", "project": PROJECT}),
        ))
        .await
        .unwrap();
    assert_eq!(
        text(&by_path),
        format!(
            "{}src/検索.rs を編集したセッション（新しい順） / sessions that edited src/検索.rs, newest first:\nnone",
            memory_note()
        )
    );
    let history = bridge
        .call_tool(call(
            "kioku_handoff_pending",
            json!({"project": PROJECT, "history": 3}),
        ))
        .await
        .unwrap();
    let out = text(&history);
    assert!(out.contains("## history, newest first"), "{out}");
    assert!(out.contains("stdio ブリッジを実装した"), "{out}");

    bridge.cancel().await.unwrap();
}

/// SPEC-M3.1 §3: a server that predates `path_prefix` answers a plain search; the bridge
/// says so instead of printing an empty session list.
#[tokio::test(flavor = "multi_thread")]
async fn path_prefix_against_an_older_server_is_a_clear_error() {
    let app = axum::Router::new().route(
        "/api/v1/search",
        axum::routing::get(|| async { axum::Json(json!({"hits": []})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let cfg = client_cfg(&format!("http://{addr}"));
    tokio::spawn(async move {
        let svc = KiokuBridge::new(cfg)
            .serve(tokio::io::split(ours))
            .await
            .unwrap();
        let _ = svc.waiting().await;
    });
    let bridge = ().serve(tokio::io::split(theirs)).await.expect("initialize");
    let r = bridge
        .call_tool(
            CallToolRequestParams::new("kioku_query")
                .with_arguments(args(json!({"path_prefix": "src/索引.rs"}))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(
        text(&r).contains("path_prefix に対応していません"),
        "{}",
        text(&r)
    );
    // a plain query still works against it
    let r = bridge
        .call_tool(
            CallToolRequestParams::new("kioku_query")
                .with_arguments(args(json!({"query": "索引"}))),
        )
        .await
        .unwrap();
    assert_eq!(text(&r), format!("{}no hits", memory_note()));
    bridge.cancel().await.unwrap();
}

/// An unreachable server is a tool error with the URL, not a crash of the bridge.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_server_is_a_tool_error() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let cfg = client_cfg(&url);
    tokio::spawn(async move {
        let svc = KiokuBridge::new(cfg)
            .serve(tokio::io::split(ours))
            .await
            .unwrap();
        let _ = svc.waiting().await;
    });
    let bridge = ().serve(tokio::io::split(theirs)).await.unwrap();
    let r = bridge
        .call_tool(CallToolRequestParams::new("kioku_status"))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(
        text(&r).contains(&format!("kioku server {url} unreachable")),
        "{}",
        text(&r)
    );
    bridge.cancel().await.unwrap();
}

/// The real binary: `kioku mcp` answers initialize and a tool call over its stdio.
#[test]
fn kioku_mcp_binary_over_stdio() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (base, _dir) = rt.block_on(start_server());
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .arg("mcp")
        .env("HOME", home.path())
        .env("KIOKU_DATA_DIR", home.path().join(".kioku"))
        .env("KIOKU_SERVER_URL", &base)
        .env("KIOKU_AUTH_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-03-26", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    );
    let init: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "kioku", "{init}");
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "kioku_status", "arguments": {}}}));
    let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    let out = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(out.contains(&format!("project ids: {PROJECT}")), "{reply}");
    drop(stdin);
    let _ = child.wait();
}
