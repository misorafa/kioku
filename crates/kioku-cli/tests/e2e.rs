//! End-to-end: the real server (`kioku_server::build_app`) on an ephemeral loopback port,
//! driven by the hook handlers as library functions with `KIOKU_SERVER_URL` / token overrides.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kioku_cli::{Agent, HookEventKind, HookOutcome, run_hook};
use kioku_core::{Config, Store};
use serde_json::{Value, json};

const TOKEN: &str = "e2e-token-0123456789";
const PROJECT: &str = "e2e-proj";

struct Server {
    base: String,
    _dir: tempfile::TempDir,
}

/// Runs the server on its own runtime thread (the hooks use a blocking client).
fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = kioku_server::build_app(store, TOKEN.to_string());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    let addr = rx.recv().unwrap();
    Server {
        base: format!("http://{addr}"),
        _dir: dir,
    }
}

fn client_config(client_dir: &Path, base: &str, token: &str, extra: &[(&str, &str)]) -> Config {
    let mut env: HashMap<String, String> = [
        ("KIOKU_DATA_DIR", client_dir.to_str().unwrap()),
        ("KIOKU_SERVER_URL", base),
        ("KIOKU_AUTH_TOKEN", token),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    let cfg = Config::load_with_env(&env).unwrap();
    assert_eq!(cfg.client.server_url, base);
    cfg
}

fn project_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".kioku.toml"),
        format!("project = \"{PROJECT}\"\nname = \"e2e\"\n"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

fn hook(cfg: &Config, event: HookEventKind, payload: Value) -> HookOutcome {
    run_hook(event, Agent::ClaudeCode, &payload.to_string(), cfg)
}

fn base_payload(sid: &str, cwd: &Path, event: HookEventKind) -> Value {
    json!({
        "session_id": sid,
        "transcript_path": format!("/tmp/{sid}.jsonl"),
        "cwd": cwd.display().to_string(),
        "hook_event_name": event.claude_code_name(),
    })
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
}

fn api_get(base: &str, path: &str) -> Value {
    http()
        .get(format!("{base}/api/v1/{path}"))
        .bearer_auth(TOKEN)
        .send()
        .unwrap()
        .json()
        .unwrap()
}

fn run_turn(cfg: &Config, sid: &str, cwd: &Path) {
    let prompt = hook(
        cfg,
        HookEventKind::UserPromptSubmit,
        with(
            base_payload(sid, cwd, HookEventKind::UserPromptSubmit),
            json!({"prompt": "引き継ぎの自動化を実装して"}),
        ),
    );
    assert_eq!(prompt, HookOutcome::ok());
    let tools = [
        json!({"tool_name": "Edit", "tool_input": {"file_path": cwd.join("src/lib.rs").display().to_string(), "old_string": "a", "new_string": "b"}, "tool_response": {"filePath": "src/lib.rs"}}),
        json!({"tool_name": "Bash", "tool_input": {"command": "cargo test"}, "tool_response": {"stdout": "ok", "stderr": "", "interrupted": false}}),
        json!({"tool_name": "Read", "tool_input": {"file_path": cwd.join("src/main.rs").display().to_string()}, "tool_response": {"type": "text"}}),
    ];
    for (i, t) in tools.into_iter().enumerate() {
        let t = with(t, json!({"tool_use_id": format!("toolu_{i}")}));
        let out = hook(
            cfg,
            HookEventKind::PostToolUse,
            with(base_payload(sid, cwd, HookEventKind::PostToolUse), t),
        );
        assert_eq!(out, HookOutcome::ok());
    }
}

#[test]
fn full_session_lifecycle_with_nudge_and_handoff() {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let cfg = client_config(client_dir.path(), &server.base, TOKEN, &[]);
    let sid = "e2e-session-1";

    // 1. SessionStart: project registered, no handoff yet.
    let out = hook(
        &cfg,
        HookEventKind::SessionStart,
        with(
            base_payload(sid, cwd, HookEventKind::SessionStart),
            json!({"source": "startup"}),
        ),
    );
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout
            .starts_with("<kioku>\nproject: e2e (id: e2e-proj)"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains(&format!("server: {}", server.base)));
    assert!(!out.stdout.contains("## 前回からの引き継ぎ"));
    assert!(out.stdout.ends_with("</kioku>\n"));

    // 2. One prompt and three tool uses.
    run_turn(&cfg, sid, cwd);
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["counts"]["prompts"], 1);
    assert_eq!(info["counts"]["tool_uses"], 3);

    // 3. Stop without an agent handoff → nudge (exit 2, stderr), no finalize.
    let stop_payload = with(
        base_payload(sid, cwd, HookEventKind::Stop),
        json!({"stop_hook_active": false}),
    );
    let out = hook(&cfg, HookEventKind::Stop, stop_payload.clone());
    assert_eq!(out.exit_code, 2);
    assert!(out.stdout.is_empty());
    assert!(
        out.stderr
            .contains("kioku_handoff_write（project=e2e-proj）"),
        "{}",
        out.stderr
    );
    assert_eq!(
        api_get(&server.base, &format!("sessions/{sid}"))["status"],
        "open"
    );

    // 4. The agent writes its handoff (what kioku_handoff_write does, via HTTP).
    let resp = http()
        .post(format!("{}/api/v1/handoffs", server.base))
        .bearer_auth(TOKEN)
        .json(&json!({
            "project": PROJECT,
            "session": sid,
            "summary": "日本語検索と引き継ぎの自動化を実装した",
            "next_steps": ["Stop フックのテストを増やす"],
            "open_questions": [],
            "decisions": ["handoff は単回消費"]
        }))
        .send()
        .unwrap();
    assert!(resp.status().is_success());

    // 5. Stop again (even with stop_hook_active=false) → finalize, silent exit 0.
    let out = hook(&cfg, HookEventKind::Stop, stop_payload);
    assert_eq!(out, HookOutcome::ok());
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["status"], "finalized");
    assert_eq!(info["has_agent_handoff"], true);

    // PreCompact and SessionEnd succeed silently too.
    let out = hook(
        &cfg,
        HookEventKind::PreCompact,
        with(
            base_payload(sid, cwd, HookEventKind::PreCompact),
            json!({"trigger": "manual"}),
        ),
    );
    assert_eq!(out, HookOutcome::ok());
    let out = hook(
        &cfg,
        HookEventKind::SessionEnd,
        with(
            base_payload(sid, cwd, HookEventKind::SessionEnd),
            json!({"reason": "other"}),
        ),
    );
    assert_eq!(out, HookOutcome::ok());

    // 6. The next session receives the handoff and a STATE excerpt on stdout.
    let out = hook(
        &cfg,
        HookEventKind::SessionStart,
        with(
            base_payload("e2e-session-2", cwd, HookEventKind::SessionStart),
            json!({"source": "startup"}),
        ),
    );
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("## 前回からの引き継ぎ"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("日本語検索と引き継ぎの自動化を実装した")
    );
    assert!(out.stdout.contains("Stop フックのテストを増やす"));
    assert!(out.stdout.contains("## 現在の状態（STATE.md 抜粋）"));
    assert!(out.stdout.chars().count() <= kioku_cli::SESSION_START_CAP);

    // The handoff was consumed: a third session gets none.
    let out = hook(
        &cfg,
        HookEventKind::SessionStart,
        with(
            base_payload("e2e-session-3", cwd, HookEventKind::SessionStart),
            json!({"source": "resume"}),
        ),
    );
    assert!(!out.stdout.contains("## 前回からの引き継ぎ"));

    // Nothing failed, so nothing was logged.
    assert!(!client_dir.path().join("logs/hook.log").exists());
}

#[test]
fn stop_without_nudge_finalizes_and_failures_are_logged() {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let cfg = client_config(
        client_dir.path(),
        &server.base,
        TOKEN,
        &[("KIOKU_STOP_NUDGE", "0")],
    );
    let sid = "e2e-nonudge";
    let out = hook(
        &cfg,
        HookEventKind::SessionStart,
        with(
            base_payload(sid, cwd, HookEventKind::SessionStart),
            json!({"source": "startup"}),
        ),
    );
    assert_eq!(out.exit_code, 0);
    run_turn(&cfg, sid, cwd);
    let out = hook(
        &cfg,
        HookEventKind::Stop,
        with(
            base_payload(sid, cwd, HookEventKind::Stop),
            json!({"stop_hook_active": false}),
        ),
    );
    assert_eq!(out, HookOutcome::ok(), "nudge disabled → finalize");
    assert_eq!(
        api_get(&server.base, &format!("sessions/{sid}"))["status"],
        "finalized"
    );
    // Rules handoff was produced for the next session.
    let pending = api_get(&server.base, &format!("handoffs/pending?project={PROJECT}"));
    assert_eq!(pending["handoff"]["source"], "rules");

    // Unknown session → 404 → silent, one log line.
    let out = hook(
        &cfg,
        HookEventKind::UserPromptSubmit,
        with(
            base_payload("no-such-session", cwd, HookEventKind::UserPromptSubmit),
            json!({"prompt": "x"}),
        ),
    );
    assert_eq!(out, HookOutcome::ok());
    // Wrong token → 401 → silent, one log line.
    let bad = client_config(client_dir.path(), &server.base, "wrong-token", &[]);
    let out = hook(
        &bad,
        HookEventKind::SessionStart,
        with(
            base_payload("e2e-x", cwd, HookEventKind::SessionStart),
            json!({"source": "startup"}),
        ),
    );
    assert_eq!(out, HookOutcome::ok());

    let log = std::fs::read_to_string(client_dir.path().join("logs/hook.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(
        lines[0].contains("user-prompt-submit session=no-such-session status=404"),
        "{log}"
    );
    assert!(
        lines[1].contains("session-start session=e2e-x status=401"),
        "{log}"
    );
}
