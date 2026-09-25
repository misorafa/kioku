//! Test harness: runs `build_app` on an ephemeral loopback port over a temp data dir.

#![allow(dead_code)]

use std::sync::Arc;

use kioku_core::{Config, Store};
use serde_json::{Value, json};

/// Bearer token used by every test server.
pub const TOKEN: &str = "test-token-123";
/// Project id used by the fixtures.
pub const PROJECT: &str = "kioku-3f9a1c2e";

/// A running server and the temp dir that backs it.
pub struct TestServer {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    /// The store behind the server.
    pub store: Arc<Store>,
    /// Keeps the data dir alive for the test's duration.
    pub dir: tempfile::TempDir,
    /// HTTP client that never goes through a proxy.
    pub http: reqwest::Client,
}

impl TestServer {
    /// Absolute URL for `path`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Authenticated GET returning (status, JSON body).
    pub async fn get(&self, path: &str) -> (u16, Value) {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        split(resp).await
    }

    /// Authenticated request with a JSON body returning (status, JSON body).
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .http
            .request(method, self.url(path))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        split(resp).await
    }

    /// Authenticated POST with a JSON body.
    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
}

async fn split(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

/// Starts a server on 127.0.0.1:0.
pub async fn spawn() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let app = kioku_server::build_app(store.clone(), TOKEN.to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer {
        base: format!("http://{addr}"),
        store,
        dir,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
    }
}

/// Body of `POST /api/v1/sessions/start` for the fixture project.
pub fn start_body(session: &str) -> Value {
    json!({
        "session_id": session,
        "agent": "claude-code",
        "cwd": "/home/u/kioku",
        "source": "startup",
        "project": {
            "id": PROJECT,
            "name": "kioku",
            "root": "/home/u/kioku",
            "remote": "github.com/u/kioku"
        }
    })
}
