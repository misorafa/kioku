//! SPEC-M3.2 §1: one `kioku_http` log line per request. Alone in this test binary: tracing
//! caches callsite interest process-wide, and a parallel test without a subscriber can make
//! the scoped subscriber below miss events.

mod common;

use std::sync::Arc;

use common::{TOKEN, spawn};
use parking_lot::Mutex;

/// Collects formatted log output in memory.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One `kioku_http` line per request (`method route status ms`), without the bearer token
/// or the query string. Single-threaded runtime: the server's events reach this thread's
/// subscriber.
#[tokio::test(flavor = "current_thread")]
async fn request_log_lines_have_the_documented_format() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("kioku_http=info"))
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let srv = spawn().await;
    let (status, _) = srv.get("/api/v1/status").await;
    assert_eq!(status, 200);
    let (status, _) = srv.get("/api/v1/search?q=%E8%A8%98%E6%86%B6").await;
    assert_eq!(status, 200);
    let anon = srv
        .http
        .get(srv.url("/api/v1/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let text = String::from_utf8(captured.0.lock().clone()).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("kioku_http")).collect();
    assert_eq!(lines.len(), 3, "captured: {text:?}");
    let re = regex_lite(&lines);
    assert_eq!(
        re,
        [
            "GET /api/v1/status 200",
            "GET /api/v1/search 200",
            "GET /api/v1/status 401"
        ]
    );
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!text.contains("Bearer"), "{text}");
    assert!(!text.contains("%E8%A8%98"), "no query strings: {text}");
}

/// `method route status` of each line, after checking it ends with `<n>ms`.
fn regex_lite(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let msg = l.split("kioku_http: ").nth(1).unwrap_or(l).trim();
            let parts: Vec<&str> = msg.split(' ').collect();
            assert_eq!(parts.len(), 4, "{msg}");
            let ms = parts[3].strip_suffix("ms").expect(msg);
            assert!(ms.parse::<u64>().is_ok(), "{msg}");
            parts[..3].join(" ")
        })
        .collect()
}
