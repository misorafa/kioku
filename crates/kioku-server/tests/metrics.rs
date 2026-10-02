//! SPEC-M3.2 §1: `GET /api/v1/metrics` (Prometheus text, bearer auth) and the counters
//! behind it. The request log line is tested in `request_log.rs` (its own binary: a
//! thread-local subscriber must not race other tests for tracing's callsite cache).

mod common;

use common::{TOKEN, spawn, start_body};
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::json;

const GAUGES: [&str; 15] = [
    "kioku_sessions_open",
    "kioku_sessions_total",
    "kioku_observations_total",
    "kioku_handoffs_pending",
    "kioku_index_docs",
    "kioku_outbox_queued",
    "kioku_db_bytes",
    "kioku_raw_bytes",
    "kioku_wiki_bytes",
    "kioku_backups_bytes",
    "kioku_last_backup_age_seconds",
    "kioku_last_prune_age_seconds",
    "kioku_last_observation_age_seconds",
    "kioku_update_last_check_age_seconds",
    "kioku_version_info",
];

const COUNTERS: [&str; 4] = [
    "kioku_http_requests_total",
    "kioku_http_request_seconds",
    "kioku_mcp_tool_calls_total",
    "kioku_git_commit_failures_total",
];

async fn metrics(srv: &common::TestServer) -> String {
    let resp = srv
        .http
        .get(srv.url("/api/v1/metrics"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ctype = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(ctype.starts_with("text/plain; version=0.0.4"), "{ctype}");
    resp.text().await.unwrap()
}

/// The value of the sample line starting with `prefix` (`name{labels}`).
fn sample(text: &str, prefix: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with(prefix) && l[prefix.len()..].starts_with(' '))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[tokio::test]
async fn metrics_need_the_token_and_list_every_family() {
    let srv = spawn().await;
    let anon = srv
        .http
        .get(srv.url("/api/v1/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let wrong = srv
        .http
        .get(srv.url("/api/v1/metrics"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let text = metrics(&srv).await;
    for name in GAUGES {
        assert!(
            text.contains(&format!("# TYPE {name} gauge\n")),
            "{name}\n{text}"
        );
    }
    for name in COUNTERS {
        assert!(text.contains(&format!("# TYPE {name} ")), "{name}\n{text}");
    }
    assert!(text.contains(&format!(
        "kioku_version_info{{version=\"{}\"}} 1\n",
        kioku_core::VERSION
    )));
    assert_eq!(sample(&text, "kioku_outbox_queued"), Some(0.0));
    assert_eq!(sample(&text, "kioku_last_backup_age_seconds"), Some(-1.0));
    // The refused scrapes were counted under their route.
    assert_eq!(
        sample(
            &text,
            "kioku_http_requests_total{route=\"/api/v1/metrics\",status=\"401\"}"
        ),
        Some(2.0)
    );
    assert!(!text.contains(TOKEN));
}

#[tokio::test]
async fn counters_increment_after_requests_and_tool_calls() {
    let srv = spawn().await;
    let before = metrics(&srv).await;
    let search = "kioku_http_requests_total{route=\"/api/v1/search\",status=\"200\"}";
    assert_eq!(sample(&before, search), None);

    let (status, _) = srv.post("/api/v1/sessions/start", start_body("s-m")).await;
    assert_eq!(status, 200);
    let (status, _) = srv
        .post(
            "/api/v1/observations",
            json!({"session_id": "s-m", "kind": "prompt", "payload": {"prompt": "検索の並び順を直す"}}),
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = srv.get("/api/v1/search?q=%E6%A4%9C%E7%B4%A2").await;
    assert_eq!(status, 200);
    // A page path is a route pattern, never a label value.
    let (status, _) = srv.get("/api/v1/pages/_global/存在しない.md").await;
    assert_eq!(status, 404);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(srv.url("/mcp")).auth_header(TOKEN),
    );
    let client = ().serve(transport).await.expect("initialize");
    let ok = client
        .call_tool(
            CallToolRequestParams::new("kioku_query")
                .with_arguments(json!({"query": "検索"}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(ok.is_error, Some(true));
    let failed = client
        .call_tool(
            CallToolRequestParams::new("kioku_read").with_arguments(
                json!({"path": "_global/ない.md"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(failed.is_error, Some(true));
    client.cancel().await.unwrap();

    let after = metrics(&srv).await;
    assert_eq!(sample(&after, search), Some(1.0), "{after}");
    assert_eq!(
        sample(
            &after,
            "kioku_http_requests_total{route=\"/api/v1/pages/{*path}\",status=\"404\"}"
        ),
        Some(1.0),
        "{after}"
    );
    assert!(!after.contains("存在しない"));
    assert!(
        sample(
            &after,
            "kioku_http_request_seconds_count{route=\"/api/v1/search\"}"
        ) >= Some(1.0)
    );
    assert!(
        sample(
            &after,
            "kioku_http_request_seconds_sum{route=\"/api/v1/search\"}"
        ) > Some(0.0)
    );
    assert!(
        after.contains("kioku_http_requests_total{route=\"/mcp\","),
        "{after}"
    );
    assert_eq!(
        sample(
            &after,
            "kioku_mcp_tool_calls_total{tool=\"kioku_query\",ok=\"true\"}"
        ),
        Some(1.0),
        "{after}"
    );
    assert_eq!(
        sample(
            &after,
            "kioku_mcp_tool_calls_total{tool=\"kioku_read\",ok=\"false\"}"
        ),
        Some(1.0),
        "{after}"
    );
    assert_eq!(sample(&after, "kioku_sessions_open"), Some(1.0));
    assert_eq!(sample(&after, "kioku_observations_total"), Some(1.0));
    let age = sample(&after, "kioku_last_observation_age_seconds").unwrap();
    assert!((0.0..60.0).contains(&age), "{age}");
}
