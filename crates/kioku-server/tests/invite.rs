//! SPEC-M2.3 §6 (server): invites, `POST /api/v1/join` (the `/i/<code>` scripts are gone,
//! SPEC-M2.7 §4),
//! expiry on an injected clock and the rate limit, over a real socket with connect info.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{Duration, Utc};
use kioku_core::{Config, Store};
use kioku_server::Invites;
use serde_json::{Value, json};

const TOKEN: &str = "invite-token-0123456789abcdef";

struct Srv {
    base: String,
    http: reqwest::Client,
    clock: Arc<AtomicI64>,
    _dir: tempfile::TempDir,
}

impl Srv {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn invite(&self, body: Value) -> (u16, Value) {
        let resp = self
            .http
            .post(self.url("/api/v1/invites"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap())
    }

    async fn join(&self, code: &str) -> (u16, Value) {
        let resp = self
            .http
            .post(self.url("/api/v1/join"))
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap())
    }

    async fn script(&self, path: &str, host: Option<&str>) -> (u16, String, String) {
        let mut req = self.http.get(self.url(path));
        if let Some(h) = host {
            req = req.header("host", h);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (status, ctype, resp.text().await.unwrap())
    }
}

async fn spawn() -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let clock = Arc::new(AtomicI64::new(0));
    let start = Utc::now();
    let c = clock.clone();
    let invites = Arc::new(Invites::with_clock(Arc::new(move || {
        start + Duration::seconds(c.load(Ordering::SeqCst))
    })));
    let app = kioku_server::build_app_with_invites(store, TOKEN.to_string(), invites);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Srv {
        base: format!("http://{addr}"),
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        clock,
        _dir: dir,
    }
}

#[tokio::test]
async fn creating_an_invite_needs_the_token() {
    let srv = spawn().await;
    for auth in [None, Some("Bearer wrong")] {
        let mut req = srv.http.post(srv.url("/api/v1/invites")).json(&json!({}));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        assert_eq!(req.send().await.unwrap().status(), 401, "{auth:?}");
    }
    let (status, body) = srv.invite(json!({})).await;
    assert_eq!(status, 200, "{body}");
    let code = body["code"].as_str().unwrap();
    assert_eq!(code.len(), 8);
    assert!(
        code.bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b)),
        "{code}"
    );
    assert_eq!(body["uses"], 1);
    assert!(body["expires_at"].as_str().unwrap().ends_with('Z'));
    // An empty body is fine too; caps apply.
    let resp = srv
        .http
        .post(srv.url("/api/v1/invites"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, capped) = srv.invite(json!({"ttl_minutes": 999, "uses": 999})).await;
    assert_eq!(capped["uses"], 20);
}

/// SPEC-M2.7 §4: the plain-HTTP installer routes are gone; the code only travels in the
/// join call, and the token only in its answer.
#[tokio::test]
async fn the_installer_routes_are_gone_and_join_hands_out_the_token() {
    let srv = spawn().await;
    let (_, body) = srv.invite(json!({})).await;
    let code = body["code"].as_str().unwrap().to_string();
    for path in [
        format!("/i/{code}"),
        format!("/i/{code}.ps1"),
        format!("/i/{code}.sh"),
        "/i/x".to_string(),
    ] {
        let (status, _, text) = srv.script(&path, Some("192.168.1.240:7391")).await;
        assert_eq!(status, 404, "{path}");
        assert!(!text.contains(TOKEN));
        assert!(!text.contains("KIOKU_JOIN"), "{path}: {text}");
    }
    // Fetching those did not use the invite up (lower case works too).
    let (status, joined) = srv.join(&code.to_lowercase()).await;
    assert_eq!(status, 200, "{joined}");
    assert_eq!(joined["token"], TOKEN);
    assert_eq!(joined["server_url"], srv.base);
}

#[tokio::test]
async fn bad_or_expired_codes_get_404_with_one_clear_sentence() {
    let srv = spawn().await;
    let (status, body) = srv.join("not-a-code").await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("kioku invite"));

    let (_, body) = srv.invite(json!({"ttl_minutes": 5})).await;
    let code = body["code"].as_str().unwrap().to_string();
    srv.clock.store(5 * 60, Ordering::SeqCst);
    assert_eq!(srv.join(&code).await.0, 404, "expired");
}

#[tokio::test]
async fn join_consumes_uses() {
    let srv = spawn().await;
    let (_, body) = srv.invite(json!({"uses": 2})).await;
    let code = body["code"].as_str().unwrap().to_string();
    assert_eq!(body["uses"], 2);
    for _ in 0..2 {
        let (status, j) = srv.join(&code).await;
        assert_eq!(status, 200);
        assert_eq!(j["token"], TOKEN);
    }
    let (status, j) = srv.join(&code).await;
    assert_eq!(status, 404, "{j}");
    // Malformed body.
    let resp = srv
        .http
        .post(srv.url("/api/v1/join"))
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn failed_lookups_are_rate_limited() {
    let srv = spawn().await;
    let (_, body) = srv.invite(json!({})).await;
    let code = body["code"].as_str().unwrap().to_string();
    for i in 0..10 {
        let bad = if i % 2 == 0 { "AAAAAAAA" } else { "ZZZZZZZZ" };
        assert_eq!(srv.join(bad).await.0, 404, "failure {i}");
    }
    assert_eq!(
        srv.join(&code).await.0,
        429,
        "blocked even for a valid code"
    );
    srv.clock.store(61, Ordering::SeqCst);
    assert_eq!(srv.join(&code).await.0, 200, "the block lasts 60 s");
}

#[tokio::test]
async fn other_routes_still_need_the_token() {
    let srv = spawn().await;
    for path in [
        "/api/v1/status",
        "/api/v1/search?q=%E5%BC%95%E3%81%8D%E7%B6%99%E3%81%8E",
    ] {
        let resp = srv.http.get(srv.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), 401, "{path}");
    }
    let resp = srv
        .http
        .post(srv.url("/mcp"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = srv
        .http
        .get(srv.url("/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // SPEC-M2.7 §11: health tells an anonymous caller nothing about the version.
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({"ok": true, "observation_dedup": true}));
}
