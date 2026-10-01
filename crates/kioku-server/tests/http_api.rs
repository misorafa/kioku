//! Integration tests for the §9 HTTP API over a real socket.

mod common;

use common::{PROJECT, TOKEN, spawn, start_body};
use reqwest::Method;
use serde_json::json;

#[tokio::test]
async fn health_is_public() {
    let srv = spawn().await;
    let resp = srv
        .http
        .get(srv.url("/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // SPEC-M2.7 §11: the version is only told to authenticated callers.
    assert!(body.get("version").is_none(), "{body}");
}

#[tokio::test]
async fn everything_else_requires_the_token() {
    let srv = spawn().await;
    let routes = [
        (Method::POST, "/api/v1/sessions/start"),
        (Method::GET, "/api/v1/sessions/abc"),
        (Method::GET, "/api/v1/sessions/abc/context"),
        (Method::POST, "/api/v1/sessions/abc/finalize"),
        (Method::POST, "/api/v1/observations"),
        (
            Method::GET,
            "/api/v1/search?q=%E5%BC%95%E3%81%8D%E7%B6%99%E3%81%8E",
        ),
        (Method::GET, "/api/v1/pages/_global/x.md"),
        (Method::PUT, "/api/v1/pages"),
        (Method::GET, "/api/v1/handoffs/pending?project=p"),
        (Method::POST, "/api/v1/handoffs"),
        (Method::GET, "/api/v1/status"),
        (Method::POST, "/api/v1/reindex"),
        (Method::POST, "/api/v1/prune"),
        (Method::POST, "/api/v1/forget"),
        (Method::POST, "/api/v1/projects/merge"),
        (Method::POST, "/api/v1/invites"),
        (Method::POST, "/mcp"),
        (Method::GET, "/mcp"),
        (Method::GET, "/api/v1/nonexistent"),
    ];
    for (method, path) in routes {
        for auth in [None, Some("Bearer wrong-token"), Some("Basic dGVzdA==")] {
            let mut req = srv
                .http
                .request(method.clone(), srv.url(path))
                .header("content-type", "application/json")
                .body("{}");
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), 401, "{method} {path} auth={auth:?}");
            let body: serde_json::Value = resp.json().await.unwrap();
            assert!(body["error"].as_str().unwrap().contains("unauthorized"));
        }
    }
    // with the token the same route works
    let (status, _) = srv.get("/api/v1/status").await;
    assert_eq!(status, 200);
    let resp = srv
        .http
        .get(srv.url("/api/v1/status"))
        .header("authorization", format!("bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn session_lifecycle_round_trip() {
    let srv = spawn().await;
    let sid = "0c2f1a2b-1111-2222-3333-444455556666";

    let (status, start) = srv.post("/api/v1/sessions/start", start_body(sid)).await;
    assert_eq!(status, 200, "{start}");
    assert_eq!(start["project_id"], PROJECT);
    assert!(start["pending_handoff"].is_null());
    assert!(start["state_excerpt"].is_null());
    assert_eq!(start["recent_sessions"], json!([]));
    // SPEC-M2.5 §3.2: clients follow the server's version.
    assert_eq!(start["server_version"], env!("CARGO_PKG_VERSION"));

    let observations = [
        json!({"session_id": sid, "kind": "prompt", "payload": {"prompt": "引き継ぎ書の自動生成を実装して"}}),
        json!({"session_id": sid, "kind": "tool_use", "ts": "2026-09-25T02:15:00Z", "payload": {
            "tool_name": "Edit",
            "tool_input": {"file_path": "/home/u/kioku/src/handoff.rs"},
            "tool_response": {}
        }}),
        json!({"session_id": sid, "kind": "tool_use", "payload": {
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test -p kioku-core"},
            "tool_response": {"stdout": "ok"}
        }}),
    ];
    for (i, obs) in observations.into_iter().enumerate() {
        let (status, body) = srv.post("/api/v1/observations", obs).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["seq"], i as i64 + 1);
    }

    // unknown session → 404, bad kind → 400, malformed JSON → 400
    let (status, body) = srv
        .post(
            "/api/v1/observations",
            json!({"session_id": "nope", "kind": "prompt", "payload": {}}),
        )
        .await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("nope"));
    let (status, body) = srv
        .post(
            "/api/v1/observations",
            json!({"session_id": sid, "kind": "bogus", "payload": {}}),
        )
        .await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
    let resp = srv
        .http
        .post(srv.url("/api/v1/observations"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let (status, info) = srv.get(&format!("/api/v1/sessions/{sid}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        info,
        json!({
            "project_id": PROJECT,
            "status": "open",
            "counts": {"prompts": 1, "tool_uses": 2},
            "has_agent_handoff": false,
            "tool_uses_since_handoff": 2
        })
    );
    let (status, _) = srv.get("/api/v1/sessions/unknown-session").await;
    assert_eq!(status, 404);

    // finalize: with a body, then again without one (idempotent)
    let (status, fin) = srv
        .post(
            &format!("/api/v1/sessions/{sid}/finalize"),
            json!({"reason": "stop"}),
        )
        .await;
    assert_eq!(status, 200, "{fin}");
    assert_eq!(fin["substantive"], true);
    let page_path = fin["session_page"].as_str().unwrap().to_string();
    assert!(page_path.starts_with(&format!("{PROJECT}/sessions/")));
    assert!(page_path.ends_with(&format!("-{}.md", &kioku_core::util::sha256_hex(sid)[..12])));
    let handoff_id = fin["handoff_id"].as_str().unwrap().to_string();
    let resp = srv
        .http
        .post(srv.url(&format!("/api/v1/sessions/{sid}/finalize")))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let again: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(again, fin);
    let (status, _) = srv
        .post("/api/v1/sessions/unknown-session/finalize", json!({}))
        .await;
    assert_eq!(status, 404);

    // pending handoff: peek does not consume
    for _ in 0..2 {
        let (status, body) = srv
            .get(&format!("/api/v1/handoffs/pending?project={PROJECT}"))
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["handoff"]["id"], handoff_id.as_str());
        assert_eq!(body["handoff"]["source"], "rules");
        assert!(body["handoff"]["accepted_at"].is_null());
        assert!(
            body["handoff"]["content_md"]
                .as_str()
                .unwrap()
                .contains("最後の指示: 引き継ぎ書の自動生成を実装して")
        );
    }

    // the session page is searchable with a Japanese query
    let (status, body) = srv
        .get(&format!(
            "/api/v1/search?q={}&project={PROJECT}",
            enc("引き継ぎ書の自動生成")
        ))
        .await;
    assert_eq!(status, 200);
    let hits = body["hits"].as_array().unwrap();
    assert!(
        hits.iter().any(|h| h["path"] == page_path.as_str()),
        "{body}"
    );
    let hit = hits
        .iter()
        .find(|h| h["path"] == page_path.as_str())
        .unwrap();
    assert_eq!(hit["kind"], "session");
    assert_eq!(hit["global"], false);
    assert!(hit["score"].as_f64().unwrap() > 0.0);

    // the session page itself
    let (status, page) = srv.get(&format!("/api/v1/pages/{page_path}")).await;
    assert_eq!(status, 200);
    assert_eq!(page["path"], page_path.as_str());
    assert_eq!(page["frontmatter"]["kind"], "session");
    assert_eq!(page["frontmatter"]["session"], sid);
    assert!(page["body"].as_str().unwrap().contains("src/handoff.rs"));
    let (status, state) = srv.get(&format!("/api/v1/pages/{PROJECT}/STATE.md")).await;
    assert_eq!(status, 200);
    assert_eq!(state["frontmatter"]["kind"], "state");

    // the next session consumes the handoff and sees STATE + recent sessions
    let (status, next) = srv
        .post("/api/v1/sessions/start", start_body("next-session"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(next["pending_handoff"]["id"], handoff_id.as_str());
    assert_eq!(next["pending_handoff"]["accepted_by"], "next-session");
    assert!(
        next["state_excerpt"]
            .as_str()
            .unwrap()
            .starts_with("## 最新の引き継ぎ")
    );
    assert_eq!(next["recent_sessions"][0]["path"], page_path.as_str());
    let (_, body) = srv
        .get(&format!("/api/v1/handoffs/pending?project={PROJECT}"))
        .await;
    assert!(body["handoff"].is_null());

    // invalid start request → 400
    let (status, _) = srv
        .post("/api/v1/sessions/start", start_body("../escape"))
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn agent_handoff_write_and_accept() {
    let srv = spawn().await;
    srv.post("/api/v1/sessions/start", start_body("s-agent"))
        .await;
    let (status, body) = srv
        .post(
            "/api/v1/handoffs",
            json!({
                "project": PROJECT,
                "summary": "HTTP API を実装した",
                "next_steps": ["MCP のテストを書く"],
                "open_questions": [],
                "decisions": ["rmcp を使う"]
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let (_, info) = srv.get("/api/v1/sessions/s-agent").await;
    assert_eq!(info["has_agent_handoff"], true);
    assert_eq!(info["tool_uses_since_handoff"], 0);

    let (status, body) = srv
        .get(&format!(
            "/api/v1/handoffs/pending?project={PROJECT}&accept=true&session=s-next"
        ))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["handoff"]["id"], id.as_str());
    assert_eq!(body["handoff"]["source"], "agent");
    assert_eq!(body["handoff"]["accepted_by"], "s-next");
    assert!(
        body["handoff"]["content_md"]
            .as_str()
            .unwrap()
            .contains("### 要約\nHTTP API を実装した")
    );
    let (_, body) = srv
        .get(&format!("/api/v1/handoffs/pending?project={PROJECT}"))
        .await;
    assert!(body["handoff"].is_null());

    // unknown project → 404 listing the known ids; empty summary → 400
    let (status, body) = srv
        .post(
            "/api/v1/handoffs",
            json!({"project": "nope-00000000", "summary": "x"}),
        )
        .await;
    assert_eq!(status, 404);
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("nope-00000000") && msg.contains(PROJECT),
        "{msg}"
    );
    let (status, _) = srv
        .post(
            "/api/v1/handoffs",
            json!({"project": PROJECT, "summary": " "}),
        )
        .await;
    assert_eq!(status, 400);
    // missing required query parameter → 400
    let (status, _) = srv.get("/api/v1/handoffs/pending").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn pages_put_get_and_search() {
    let srv = spawn().await;
    srv.post("/api/v1/sessions/start", start_body("s-pages"))
        .await;

    let (status, body) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({
                "title": "自宅サーバーの構成",
                "content": "k3sクラスタにWireGuardで自宅サーバーを参加させた",
                "scope": "global",
                "tags": ["infra"]
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let global_path = body["path"].as_str().unwrap().to_string();
    assert!(global_path.starts_with("_global/page-"));

    let (status, body) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({
                "title": "Design Notes",
                "content": "引き継ぎ書を毎回作るのが手間なので自動化したい",
                "project": PROJECT
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["path"], format!("{PROJECT}/pages/design-notes.md"));

    let (status, page) = srv
        .get(&format!("/api/v1/pages/{PROJECT}/pages/design-notes.md"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(page["frontmatter"]["title"], "Design Notes");
    assert_eq!(page["frontmatter"]["scope"], "project");
    assert_eq!(page["frontmatter"]["project"], PROJECT);
    assert_eq!(
        page["body"],
        "引き継ぎ書を毎回作るのが手間なので自動化したい\n"
    );
    let (status, page) = srv.get(&format!("/api/v1/pages/{global_path}")).await;
    assert_eq!(status, 200);
    assert_eq!(page["frontmatter"]["tags"], json!(["infra"]));

    // replace keeps the path
    let (status, body) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({"title": "Design Notes", "content": "更新した本文", "project": PROJECT}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["path"], format!("{PROJECT}/pages/design-notes.md"));

    // Japanese search, scoped
    let (_, body) = srv
        .get(&format!("/api/v1/search?q={}", enc("自宅サーバー")))
        .await;
    assert_eq!(body["hits"][0]["path"], global_path.as_str());
    assert_eq!(body["hits"][0]["global"], true);
    let snippet = body["hits"][0]["snippet"].as_str().unwrap();
    assert!(
        snippet.contains('【') && snippet.contains("サーバー"),
        "{snippet}"
    );
    let (_, body) = srv
        .get(&format!(
            "/api/v1/search?q={}&scope=project&project={PROJECT}",
            enc("更新")
        ))
        .await;
    assert_eq!(
        body["hits"][0]["path"],
        format!("{PROJECT}/pages/design-notes.md")
    );
    let (_, body) = srv
        .get(&format!("/api/v1/search?q={}&scope=global", enc("更新")))
        .await;
    assert_eq!(body["hits"], json!([]));
    let (_, body) = srv.get("/api/v1/search?q=Postgres&limit=5").await;
    assert_eq!(body["hits"], json!([]));
    let (status, _) = srv.get("/api/v1/search?q=x&scope=project").await;
    assert_eq!(status, 400);
    let (status, _) = srv.get("/api/v1/search?q=x&scope=bogus").await;
    assert_eq!(status, 400);
    let (status, _) = srv.get("/api/v1/search").await;
    assert_eq!(status, 400);

    // errors
    let (status, body) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({"title": "x", "content": "y", "project": "unknown-proj"}),
        )
        .await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains(PROJECT));
    let (status, _) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({"title": "x", "content": "y", "path": "../escape.md"}),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({"title": "  ", "content": "y"}),
        )
        .await;
    assert_eq!(status, 400);
    let (status, body) = srv.get("/api/v1/pages/_global/missing.md").await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn reindex_and_status() {
    let srv = spawn().await;
    let (status, body) = srv.get("/api/v1/status").await;
    assert_eq!(status, 200);
    assert_eq!(body["pages"], 0);
    assert_eq!(body["index_docs"], 0);
    assert_eq!(
        body["data_dir"],
        srv.dir.path().display().to_string().as_str()
    );
    for key in ["projects", "sessions", "observations", "handoffs"] {
        assert_eq!(body[key], 0, "{key}");
    }

    // a hand-written page is invisible until reindex
    std::fs::create_dir_all(srv.dir.path().join("wiki/_global")).unwrap();
    std::fs::write(
        srv.dir.path().join("wiki/_global/manual.md"),
        "# 手書きメモ\nFlutterでコードチャートのアプリを作っている\n",
    )
    .unwrap();
    let (_, body) = srv
        .get(&format!("/api/v1/search?q={}", enc("アプリ")))
        .await;
    assert_eq!(body["hits"], json!([]));

    let (status, body) = srv.post("/api/v1/reindex", json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"docs": 1}));

    let (_, body) = srv
        .get(&format!("/api/v1/search?q={}", enc("アプリ")))
        .await;
    assert_eq!(body["hits"][0]["path"], "_global/manual.md");
    assert_eq!(body["hits"][0]["title"], "手書きメモ");
    let (_, body) = srv.get("/api/v1/status").await;
    assert_eq!(body["pages"], 1);
    assert_eq!(body["index_docs"], 1);
    assert_eq!(srv.store.status().unwrap().index_docs, 1);
}

#[tokio::test]
async fn status_reports_versions() {
    let srv = spawn().await;
    let (status, body) = srv.get("/api/v1/status").await;
    assert_eq!(status, 200);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["index_schema_version"], 2);
    assert_eq!(body["index_schema_expected"], 2);
    // SPEC-M2.5 §3.4: the server's auto-update state (defaults: auto on, nothing checked).
    assert_eq!(body["update"]["auto"], true);
    assert_eq!(body["update"]["managed"], false);
    assert!(body["update"]["latest_seen"].is_null(), "{body}");
    assert!(body["update"]["last_check"].is_null(), "{body}");
    assert!(body["update"]["last_error"].is_null(), "{body}");
    let file = srv.dir.path().join("index/schema-version");
    std::fs::remove_file(&file).unwrap();
    let (_, body) = srv.get("/api/v1/status").await;
    assert!(body["index_schema_version"].is_null(), "{body}");
    assert_eq!(body["index_schema_expected"], 2);
    std::fs::write(&file, "1\n").unwrap();
    let (_, body) = srv.get("/api/v1/status").await;
    assert_eq!(body["index_schema_version"], 1);
}

#[tokio::test]
async fn session_context_is_read_only() {
    let srv = spawn().await;
    let (status, body) = srv.get("/api/v1/sessions/unknown-session/context").await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("unknown-session"));

    // a Codex session whose apply_patch touched two files leaves a rules handoff
    let mut start = start_body("codex-0001");
    start["agent"] = json!("codex");
    let (status, _) = srv.post("/api/v1/sessions/start", start).await;
    assert_eq!(status, 200);
    for obs in [
        json!({"session_id": "codex-0001", "kind": "prompt", "payload": {"prompt": "認証のバグを直して"}}),
        json!({"session_id": "codex-0001", "kind": "tool_use", "payload": {
            "tool_name": "Edit", "native_tool": "apply_patch",
            "tool_input": {"file_paths": ["/home/u/kioku/src/auth.rs", "/home/u/kioku/src/login.rs"],
                           "patch": "*** Begin Patch\n*** Update File: src/auth.rs\n*** End Patch"},
            "tool_response": "Success. Updated the following files"
        }}),
    ] {
        let (status, body) = srv.post("/api/v1/observations", obs).await;
        assert_eq!(status, 200, "{body}");
    }
    let (_, fin) = srv
        .post("/api/v1/sessions/codex-0001/finalize", json!({}))
        .await;
    let handoff_id = fin["handoff_id"].as_str().unwrap().to_string();
    let page_path = fin["session_page"].as_str().unwrap().to_string();
    let (_, page) = srv.get(&format!("/api/v1/pages/{page_path}")).await;
    let page_body = page["body"].as_str().unwrap();
    assert!(page_body.contains("- src/auth.rs (1)"), "{page_body}");
    assert!(page_body.contains("- src/login.rs (1)"), "{page_body}");
    assert_eq!(page["frontmatter"]["agent"], "codex");

    // a Cursor session started implicitly (M2 §3.9) consumes it
    let mut start = start_body("cursor-0002");
    start["agent"] = json!("cursor");
    start["source"] = json!("implicit");
    let (status, started) = srv.post("/api/v1/sessions/start", start).await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["pending_handoff"]["id"], handoff_id.as_str());
    assert_eq!(srv.store.session("cursor-0002").unwrap().source, "implicit");

    // a newer handoff is written; context returns the accepted one and consumes nothing
    let (status, _) = srv
        .post(
            "/api/v1/handoffs",
            json!({"project": PROJECT, "session": "codex-0001", "summary": "追加の引き継ぎ"}),
        )
        .await;
    assert_eq!(status, 200);
    for _ in 0..2 {
        let (status, ctx) = srv.get("/api/v1/sessions/cursor-0002/context").await;
        assert_eq!(status, 200, "{ctx}");
        assert_eq!(ctx["project_id"], PROJECT);
        assert_eq!(ctx["pending_handoff"]["id"], handoff_id.as_str());
        assert_eq!(ctx["pending_handoff"]["accepted_by"], "cursor-0002");
        assert_eq!(ctx["state_excerpt"], started["state_excerpt"]);
        assert_eq!(ctx["recent_sessions"], started["recent_sessions"]);
        assert_eq!(ctx["recent_sessions"][0]["path"], page_path.as_str());
    }
    let (_, pending) = srv
        .get(&format!("/api/v1/handoffs/pending?project={PROJECT}"))
        .await;
    assert!(pending["handoff"]["accepted_at"].is_null(), "{pending}");
    assert_eq!(pending["handoff"]["source"], "agent");
    let (_, info) = srv.get("/api/v1/sessions/cursor-0002").await;
    assert_eq!(info["status"], "open");
    assert_eq!(info["counts"], json!({"prompts": 0, "tool_uses": 0}));

    // a session that accepted nothing gets pending_handoff: null
    let (status, ctx) = srv.get("/api/v1/sessions/codex-0001/context").await;
    assert_eq!(status, 200);
    assert!(ctx["pending_handoff"].is_null(), "{ctx}");
}

#[tokio::test]
async fn lanes_route_handoffs_over_http() {
    let srv = spawn().await;
    // an old client's payload (no `lane`) still works and gets no lane back
    let (status, body) = srv
        .post("/api/v1/sessions/start", start_body("main-1"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("lane").is_none(), "{body}");
    assert!(body.get("reference_handoff").is_none(), "{body}");

    let mut a = start_body("wt-a");
    a["lane"] = json!("feature/検索");
    let (status, body) = srv.post("/api/v1/sessions/start", a).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["lane"], "feature/検索");

    // the branch's handoff stays on its lane
    let (_, h) = srv
        .post(
            "/api/v1/handoffs",
            json!({"project": PROJECT, "session": "wt-a", "summary": "検索を直した"}),
        )
        .await;
    let (_, p) = srv
        .get(&format!("/api/v1/handoffs/pending?project={PROJECT}"))
        .await;
    assert!(p["handoff"].is_null(), "project lane is empty: {p}");
    let (_, p) = srv
        .get(&format!(
            "/api/v1/handoffs/pending?project={PROJECT}&lane={}",
            enc("feature/検索")
        ))
        .await;
    assert_eq!(p["handoff"]["id"], h["id"]);
    assert_eq!(p["handoff"]["lane"], "feature/検索");

    // a main-line handoff is only a reference for another branch, and is not consumed
    let (_, main) = srv
        .post(
            "/api/v1/handoffs",
            json!({"project": PROJECT, "session": "main-1", "summary": "メインの作業"}),
        )
        .await;
    let mut b = start_body("wt-b");
    b["lane"] = json!("task-b");
    let (_, body) = srv.post("/api/v1/sessions/start", b).await;
    assert!(body["pending_handoff"].is_null(), "{body}");
    assert_eq!(body["reference_handoff"]["id"], main["id"]);
    let (_, p) = srv
        .get(&format!(
            "/api/v1/handoffs/pending?project={PROJECT}&session=wt-b&accept=true"
        ))
        .await;
    assert!(p["handoff"].is_null(), "{p}");
    assert_eq!(p["reference_handoff"]["id"], main["id"]);
    let (_, ctx) = srv.get("/api/v1/sessions/wt-b/context").await;
    assert_eq!(ctx["lane"], "task-b");
    assert_eq!(ctx["reference_handoff"]["id"], main["id"]);
    let (_, body) = srv
        .post("/api/v1/sessions/start", start_body("main-2"))
        .await;
    assert_eq!(body["pending_handoff"]["id"], main["id"]);
}

#[tokio::test]
async fn aliases_and_merge_over_http() {
    let srv = spawn().await;
    let path_id = "notes-0a1b2c3d";
    let remote = "github.com/u/notes";
    let remote_id = kioku_core::project::id_from_remote("notes", remote);
    let start = |sid: &str, id: &str, remote: Option<&str>| {
        json!({
            "session_id": sid, "agent": "codex", "cwd": "/home/u/notes", "source": "startup",
            "project": {"id": id, "name": "notes", "root": "/home/u/notes", "remote": remote}
        })
    };
    let (status, _) = srv
        .post("/api/v1/sessions/start", start("n-1", path_id, None))
        .await;
    assert_eq!(status, 200);
    let (status, body) = srv
        .post(
            "/api/v1/sessions/start",
            start("n-2", &remote_id, Some(remote)),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["project_id"], path_id);
    let (_, st) = srv.get("/api/v1/status").await;
    assert_eq!(
        st["aliases"],
        json!([{"alias": remote_id, "project_id": path_id}])
    );
    assert_eq!(st["project_ids"], json!([path_id]));

    // writes and searches through the alias land in the canonical project
    let (status, body) = srv
        .send(
            Method::PUT,
            "/api/v1/pages",
            json!({"title": "議事録", "content": "エイリアスで保存した議事録", "project": remote_id}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["path"].as_str().unwrap().starts_with(path_id));
    let (_, hits) = srv
        .get(&format!(
            "/api/v1/search?q={}&project={remote_id}",
            enc("議事録")
        ))
        .await;
    assert_eq!(hits["hits"][0]["path"], body["path"]);

    // merge: another project folded into the canonical one
    let (status, _) = srv.post("/api/v1/sessions/start", start_body("k-1")).await;
    assert_eq!(status, 200);
    let merge = |dry: bool| json!({"from": PROJECT, "into": remote_id, "dry_run": dry});
    let (status, dry) = srv.post("/api/v1/projects/merge", merge(true)).await;
    assert_eq!(status, 200, "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["into"], path_id);
    assert_eq!(dry["sessions"], 1);
    let (_, st) = srv.get("/api/v1/status").await;
    assert_eq!(st["projects"], 2);
    let (status, done) = srv.post("/api/v1/projects/merge", merge(false)).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(srv.store.session("k-1").unwrap().project_id, path_id);
    let (_, st) = srv.get("/api/v1/status").await;
    assert_eq!(st["projects"], 1);
    let (_, again) = srv.post("/api/v1/projects/merge", merge(false)).await;
    assert_eq!(again["already_merged"], true);
    let (status, _) = srv
        .post(
            "/api/v1/projects/merge",
            json!({"from": "nope-00000000", "into": path_id}),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = srv
        .post(
            "/api/v1/projects/merge",
            json!({"from": path_id, "into": remote_id}),
        )
        .await;
    assert_eq!(status, 400);
}

/// Percent-encodes a query-string value.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// SPEC-M2.5 §3.4: `update` reflects what the update task recorded.
#[tokio::test]
async fn status_reports_the_shared_update_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        kioku_core::Store::open(kioku_core::Config::for_data_dir(dir.path())).unwrap(),
    );
    let update = kioku_server::UpdateStatus::shared_for(&store);
    let app = kioku_server::build_app_with_update(store, TOKEN.to_string(), update.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    {
        let mut u = update.lock();
        u.managed = true;
        u.latest_seen = Some("v9.9.9".into());
        u.last_check = Some("2026-09-30T00:00:00Z".into());
        u.last_error = Some("checksum mismatch".into());
    }
    let body: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{addr}/api/v1/status"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["update"],
        json!({"auto": true, "managed": true, "latest_seen": "v9.9.9",
               "last_check": "2026-09-30T00:00:00Z", "last_error": "checksum mismatch"})
    );
}

/// SPEC-M2.5 §3.1 step 3: a shutdown request stops `serve_with` (graceful, bounded).
#[tokio::test]
async fn serve_with_stops_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = kioku_core::Config::for_data_dir(dir.path());
    cfg.server.auth_token = Some(TOKEN.into());
    let store = std::sync::Arc::new(kioku_core::Store::open(cfg).unwrap());
    let (tx, rx) = tokio::sync::watch::channel(false);
    let opts = kioku_server::ServeOptions {
        update: kioku_server::UpdateStatus::shared_for(&store),
        shutdown: Some(rx),
    };
    let task = tokio::spawn(kioku_server::serve_with(store, "127.0.0.1".into(), 0, opts));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!task.is_finished());
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("serve_with returns after a shutdown request")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn durable_delivery_page_conflicts_and_backup_api() {
    let server = common::spawn().await;
    let (_, mut req) = server
        .post(
            "/api/v1/sessions/start",
            common::start_body("reliability-session"),
        )
        .await;
    assert_eq!(req["project_id"], common::PROJECT);
    let observation = json!({"session_id":"reliability-session", "event_id":"delivery-1", "kind":"prompt", "payload":{"prompt":"日本語の復元テスト"}});
    let (code, first) = server
        .post("/api/v1/observations", observation.clone())
        .await;
    assert_eq!(code, 200);
    let (_, retry) = server
        .post("/api/v1/observations", observation.clone())
        .await;
    assert_eq!(retry, first);
    let mut changed = observation;
    changed["payload"]["prompt"] = json!("別の内容");
    assert_eq!(server.post("/api/v1/observations", changed).await.0, 409);
    req = json!({"title":"共有ページ", "content":"初期版", "project":common::PROJECT, "expected_revision":""});
    let (_, page) = server
        .send(reqwest::Method::PUT, "/api/v1/pages", req.clone())
        .await;
    let path = page["path"].as_str().unwrap();
    let (_, read) = server.get(&format!("/api/v1/pages/{path}")).await;
    let revision = read["revision"].as_str().unwrap();
    assert_eq!(revision.len(), 64);
    req["expected_revision"] = json!(revision);
    req["content"] = json!("変更後");
    assert_eq!(
        server
            .send(reqwest::Method::PUT, "/api/v1/pages", req.clone())
            .await
            .0,
        200
    );
    assert_eq!(
        server
            .send(reqwest::Method::PUT, "/api/v1/pages", req)
            .await
            .0,
        409
    );
    assert_eq!(
        server.get("/api/v1/diagnostics").await.1["inconsistent_pages"],
        json!([])
    );
    let (code, backup) = server.post("/api/v1/backup", json!({})).await;
    assert_eq!(code, 200);
    // SPEC-M2.8 §4: format 2 (working tree + wiki.bundle).
    assert_eq!(backup["format"], 2);
    assert!(backup["files"]["db/kioku.sqlite"]["sha256"].is_string());
    assert!(server.get("/api/v1/diagnostics").await.1["last_backup"].is_string());
    // SPEC-M2.7 §12: a second backup within 60 s is refused.
    let (code, again) = server.post("/api/v1/backup", json!({})).await;
    assert_eq!(code, 409, "{again}");
    let unauth = server
        .http
        .post(server.url("/api/v1/backup"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
}

/// SPEC-M2.8 §5: an index built by an older kioku is rebuilt by `serve_with` after it starts
/// listening (observable through the index version), not when the store is opened.
#[tokio::test]
async fn serve_with_rebuilds_an_outdated_index_after_listening() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = kioku_core::Config::for_data_dir(dir.path());
    cfg.server.auth_token = Some(TOKEN.into());
    let store = kioku_core::Store::open(cfg.clone()).unwrap();
    store
        .write_page(&kioku_core::WritePageRequest {
            title: "古い索引".into(),
            content: "全角ＡＢＣの日本語検索".into(),
            ..Default::default()
        })
        .unwrap();
    drop(store);
    std::fs::write(dir.path().join("index/schema-version"), "1\n").unwrap();
    let store = std::sync::Arc::new(kioku_core::Store::open(cfg).unwrap());
    assert!(store.index_outdated());
    let (tx, rx) = tokio::sync::watch::channel(false);
    let opts = kioku_server::ServeOptions {
        update: kioku_server::UpdateStatus::shared_for(&store),
        shutdown: Some(rx),
    };
    let task = tokio::spawn(kioku_server::serve_with(
        store.clone(),
        "127.0.0.1".into(),
        0,
        opts,
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while store.index_outdated() {
        assert!(std::time::Instant::now() < deadline, "index not rebuilt");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        store.index_version(),
        kioku_core::index::INDEX_SCHEMA_VERSION
    );
    assert!(
        !store
            .search("日本語検索", &kioku_core::SearchScope::All, 3)
            .unwrap()
            .is_empty()
    );
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// SPEC-M2.8 §3: `POST /api/v1/prune` (dry run and real), `POST /api/v1/forget`, and the
/// storage sizes in `GET /api/v1/status`.
#[tokio::test]
async fn prune_forget_and_storage_over_http() {
    let srv = spawn().await;
    srv.post("/api/v1/sessions/start", start_body("s-forget"))
        .await;
    let (code, _) = srv
        .post(
            "/api/v1/observations",
            json!({"session_id": "s-forget", "kind": "prompt",
                   "payload": {"prompt": "消したい秘密の作業メモ"}}),
        )
        .await;
    assert_eq!(code, 200);
    let (_, fin) = srv
        .post("/api/v1/sessions/s-forget/finalize", json!({}))
        .await;
    let page = fin["session_page"].as_str().unwrap().to_string();
    let (_, hits) = srv.get("/api/v1/search?q=%E7%A7%98%E5%AF%86").await;
    assert!(
        hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"] == page)
    );

    let (code, dry) = srv.post("/api/v1/prune", json!({"dry_run": true})).await;
    assert_eq!(code, 200, "{dry}");
    assert_eq!(dry["dry_run"], true);
    let (code, real) = srv.post("/api/v1/prune", json!({})).await;
    assert_eq!(code, 200, "{real}");
    assert_eq!(real["dry_run"], false);

    let (code, body) = srv.post("/api/v1/forget", json!({})).await;
    assert_eq!(code, 400, "{body}");
    let (code, body) = srv
        .post(
            "/api/v1/forget",
            json!({"session": "s-forget", "dry_run": true}),
        )
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["pages"], json!([page.clone()]));
    let (code, body) = srv
        .post("/api/v1/forget", json!({"session": "s-forget"}))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["sessions"], json!(["s-forget"]));
    let (_, hits) = srv.get("/api/v1/search?q=%E7%A7%98%E5%AF%86").await;
    assert!(hits["hits"].as_array().unwrap().is_empty(), "{hits}");
    let (code, _) = srv.get("/api/v1/sessions/s-forget").await;
    assert_eq!(code, 404);
    let (code, _) = srv
        .post("/api/v1/forget", json!({"session": "s-forget"}))
        .await;
    assert_eq!(code, 404);

    let (_, status) = srv.get("/api/v1/status").await;
    let storage = &status["storage"];
    assert!(storage["db_bytes"].as_u64().unwrap() > 0, "{status}");
    assert!(storage["wiki_bytes"].as_u64().is_some());
    assert_eq!(storage["last_prune"], real["at"]);
}
