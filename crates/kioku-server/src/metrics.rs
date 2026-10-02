//! Observability (SPEC-M3.2 §1): the counters this server keeps since it started, the
//! request-counting / request-log middleware, and the hand-written Prometheus text of
//! `GET /api/v1/metrics` (no metrics crate).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::Response,
};
use kioku_core::store::MetricsSnapshot;
use parking_lot::Mutex;

/// `target` of the request log lines (`RUST_LOG=kioku_http=info` turns them on).
pub const REQUEST_LOG_TARGET: &str = "kioku_http";

/// Counters since start, shared by the middleware, the MCP tools and the metrics handler.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Counters>,
}

#[derive(Debug, Default)]
struct Counters {
    /// `(route, status)` → requests.
    requests: BTreeMap<(String, u16), u64>,
    /// `route` → (seconds summed, requests).
    seconds: BTreeMap<String, (f64, u64)>,
    /// `(tool, ok)` → calls.
    tools: BTreeMap<(String, bool), u64>,
}

impl Metrics {
    /// Fresh counters.
    pub fn new() -> Arc<Metrics> {
        Arc::new(Metrics::default())
    }

    /// Counts one finished HTTP request.
    pub fn record_request(&self, route: &str, status: u16, seconds: f64) {
        let mut c = self.inner.lock();
        *c.requests.entry((route.to_string(), status)).or_default() += 1;
        let e = c.seconds.entry(route.to_string()).or_default();
        e.0 += seconds;
        e.1 += 1;
    }

    /// Counts one MCP tool call.
    pub fn record_tool(&self, tool: &str, ok: bool) {
        *self
            .inner
            .lock()
            .tools
            .entry((tool.to_string(), ok))
            .or_default() += 1;
    }
}

/// What [`render`] needs besides the store snapshot and the counters.
#[derive(Clone, Debug, Default)]
pub struct RenderInput {
    /// The server's version.
    pub version: String,
    /// RFC 3339 time of the last release check (`None`: never).
    pub update_last_check: Option<String>,
    /// Observations queued for resending on this machine: always 0 on the server (§1).
    pub outbox_queued: u64,
}

/// Seconds since an RFC 3339 time; `-1` when it never happened (or cannot be read).
fn age_seconds(ts: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> i64 {
    ts.and_then(kioku_core::util::parse_ts)
        .map(|t| (now - t).num_seconds().max(0))
        .unwrap_or(-1)
}

/// Escapes a Prometheus label value (`\`, `"`, newline).
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    family(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

/// The Prometheus text exposition (format 0.0.4) of every gauge and counter.
pub fn render(
    snap: &MetricsSnapshot,
    metrics: &Metrics,
    input: &RenderInput,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut out = String::new();
    let st = &snap.storage;
    gauge(
        &mut out,
        "kioku_sessions_open",
        "Sessions not finalized yet.",
        snap.sessions_open,
    );
    gauge(
        &mut out,
        "kioku_sessions_total",
        "Sessions recorded.",
        snap.sessions_total,
    );
    gauge(
        &mut out,
        "kioku_observations_total",
        "Observations recorded.",
        snap.observations_total,
    );
    gauge(
        &mut out,
        "kioku_handoffs_pending",
        "Handoffs not accepted yet.",
        snap.handoffs_pending,
    );
    gauge(
        &mut out,
        "kioku_index_docs",
        "Documents in the search index.",
        snap.index_docs,
    );
    gauge(
        &mut out,
        "kioku_outbox_queued",
        "Observations queued for resending (always 0 on the server; clients have no endpoint).",
        input.outbox_queued,
    );
    for (name, help, v) in [
        ("kioku_db_bytes", "Bytes under db/.", st.db_bytes),
        ("kioku_raw_bytes", "Bytes under raw/.", st.raw_bytes),
        ("kioku_wiki_bytes", "Bytes under wiki/.", st.wiki_bytes),
        (
            "kioku_backups_bytes",
            "Bytes under backups/.",
            st.backups_bytes,
        ),
    ] {
        gauge(&mut out, name, help, v);
    }
    for (name, what, ts) in [
        (
            "kioku_last_backup_age_seconds",
            "the last completed backup",
            snap.last_backup.as_deref(),
        ),
        (
            "kioku_last_prune_age_seconds",
            "the last retention run",
            st.last_prune.as_deref(),
        ),
        (
            "kioku_last_observation_age_seconds",
            "the last observation received",
            snap.last_observation.as_deref(),
        ),
        (
            "kioku_update_last_check_age_seconds",
            "the last release check",
            input.update_last_check.as_deref(),
        ),
    ] {
        gauge(
            &mut out,
            name,
            &format!("Seconds since {what}; -1 = never."),
            age_seconds(ts, now),
        );
    }
    family(
        &mut out,
        "kioku_version_info",
        "gauge",
        "The running kioku version.",
    );
    let _ = writeln!(
        out,
        "kioku_version_info{{version=\"{}\"}} 1",
        label(&input.version)
    );

    let c = metrics.inner.lock();
    family(
        &mut out,
        "kioku_http_requests_total",
        "counter",
        "HTTP requests since start, by route and status.",
    );
    for ((route, status), n) in &c.requests {
        let _ = writeln!(
            out,
            "kioku_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}",
            label(route)
        );
    }
    family(
        &mut out,
        "kioku_http_request_seconds",
        "summary",
        "Time spent answering HTTP requests since start, by route.",
    );
    for (route, (sum, count)) in &c.seconds {
        let route = label(route);
        let _ = writeln!(
            out,
            "kioku_http_request_seconds_sum{{route=\"{route}\"}} {sum:.6}"
        );
        let _ = writeln!(
            out,
            "kioku_http_request_seconds_count{{route=\"{route}\"}} {count}"
        );
    }
    family(
        &mut out,
        "kioku_mcp_tool_calls_total",
        "counter",
        "MCP tool calls since start, by tool and outcome.",
    );
    for ((tool, ok), n) in &c.tools {
        let _ = writeln!(
            out,
            "kioku_mcp_tool_calls_total{{tool=\"{}\",ok=\"{ok}\"}} {n}",
            label(tool)
        );
    }
    family(
        &mut out,
        "kioku_git_commit_failures_total",
        "counter",
        "Failed wiki git add/commit runs since start.",
    );
    let _ = writeln!(
        out,
        "kioku_git_commit_failures_total {}",
        snap.git_commit_failures
    );
    out
}

/// The route label of a request: the matched route pattern (`/api/v1/pages/{*path}`), `/mcp`
/// for the MCP endpoint, `unmatched` for anything no route took — never the raw path, so
/// page names and ids do not become label values.
pub fn route_label(matched: Option<&str>) -> String {
    match matched {
        Some(p) if p == "/mcp" || p.starts_with("/mcp/") => "/mcp".to_string(),
        Some(p) => p.to_string(),
        None => "unmatched".to_string(),
    }
}

/// One request log line: `method route status ms` (no headers, no query string).
pub fn request_line(method: &str, route: &str, status: u16, ms: u128) -> String {
    format!("{method} {route} {status} {ms}ms")
}

/// Middleware: counts every request (401s included) and logs one `kioku_http` line.
pub async fn track(State(metrics): State<Arc<Metrics>>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().as_str().to_string();
    let route = route_label(req.extensions().get::<MatchedPath>().map(|m| m.as_str()));
    let response = next.run(req).await;
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    metrics.record_request(&route, status, elapsed.as_secs_f64());
    tracing::info!(
        target: "kioku_http",
        "{}",
        request_line(&method, &route, status, elapsed.as_millis())
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        kioku_core::util::parse_ts(s).unwrap()
    }

    #[test]
    fn renders_every_family_and_escapes_labels() {
        let m = Metrics::new();
        m.record_request("/api/v1/status", 200, 0.25);
        m.record_request("/api/v1/status", 200, 0.25);
        m.record_request("/api/v1/status", 401, 0.001);
        m.record_tool("kioku_query", true);
        m.record_tool("kioku_query", false);
        let snap = MetricsSnapshot {
            sessions_open: 2,
            last_backup: Some("2026-10-01T00:00:00.000Z".into()),
            ..MetricsSnapshot::default()
        };
        let input = RenderInput {
            version: "0.9.\"1".into(),
            update_last_check: None,
            outbox_queued: 0,
        };
        let text = render(&snap, &m, &input, at("2026-10-02T00:00:00.000Z"));
        assert!(text.contains("kioku_sessions_open 2\n"));
        assert!(text.contains("kioku_last_backup_age_seconds 86400\n"));
        assert!(text.contains("kioku_update_last_check_age_seconds -1\n"));
        assert!(text.contains("kioku_version_info{version=\"0.9.\\\"1\"} 1\n"));
        assert!(
            text.contains("kioku_http_requests_total{route=\"/api/v1/status\",status=\"200\"} 2\n")
        );
        assert!(
            text.contains("kioku_http_requests_total{route=\"/api/v1/status\",status=\"401\"} 1\n")
        );
        assert!(text.contains("kioku_http_request_seconds_count{route=\"/api/v1/status\"} 3\n"));
        assert!(text.contains("kioku_mcp_tool_calls_total{tool=\"kioku_query\",ok=\"false\"} 1\n"));
        assert!(text.contains("kioku_git_commit_failures_total 0\n"));
        // Every sample line belongs to a declared family.
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            let base = name.trim_end_matches("_sum").trim_end_matches("_count");
            assert!(
                text.contains(&format!("# TYPE {name} "))
                    || text.contains(&format!("# TYPE {base} ")),
                "{line}"
            );
        }
    }

    #[test]
    fn route_labels_never_carry_raw_paths() {
        assert_eq!(
            route_label(Some("/api/v1/pages/{*path}")),
            "/api/v1/pages/{*path}"
        );
        assert_eq!(route_label(Some("/mcp")), "/mcp");
        assert_eq!(route_label(Some("/mcp/{*rest}")), "/mcp");
        assert_eq!(route_label(None), "unmatched");
    }

    #[test]
    fn request_line_format() {
        assert_eq!(
            request_line("GET", "/api/v1/search", 200, 12),
            "GET /api/v1/search 200 12ms"
        );
    }
}
