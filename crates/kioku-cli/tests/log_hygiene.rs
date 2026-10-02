//! SPEC-M3.2 §1 regression: no log kioku writes ever contains the bearer token —
//! `serve.log` (with the per-request `kioku_http` lines on), `hook.log` (failed hooks,
//! including a refused token) and `update.log` (a failed release check). Alone in its test
//! binary because it installs the process-wide tracing subscriber that `kioku serve` uses.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kioku_cli::{Agent, HookEnv, HookEventKind, run_hook_with_env};
use kioku_core::{Config, Store};
use serde_json::json;

const TOKEN: &str = "log-hygiene-token-7f3a9c";

fn client_config(dir: &Path, base: &str, token: &str) -> Config {
    let env: HashMap<String, String> = [
        ("KIOKU_DATA_DIR", dir.to_str().unwrap()),
        ("KIOKU_SERVER_URL", base),
        ("KIOKU_AUTH_TOKEN", token),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Config::load_with_env(&env).unwrap()
}

#[test]
fn no_token_in_serve_hook_or_update_logs() {
    let server_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let serve_log = server_dir.path().join("logs").join("serve.log");
    std::fs::create_dir_all(serve_log.parent().unwrap()).unwrap();
    // What `kioku serve --log-file` installs, with `[server] request_log = true`.
    let writer = kioku_cli::logfile::RotatingFile::open(&serve_log).unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            kioku_cli::commands::default_log_filter(true),
        ))
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
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
    let base = format!("http://{}", rx.recv().unwrap());

    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".kioku.toml"),
        "project = \"log-proj\"\nname = \"ログ\"\n",
    )
    .unwrap();
    let env = HookEnv {
        vars: HashMap::new(),
        home: Some(client_dir.path().to_path_buf()),
        cwd: Some(project.path().to_path_buf()),
    };
    let payload = |event: HookEventKind, extra: serde_json::Value| {
        let mut v = json!({
            "session_id": "log-s1",
            "cwd": project.path().display().to_string(),
            "hook_event_name": event.claude_code_name(),
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v.to_string()
    };
    let good = client_config(client_dir.path(), &base, TOKEN);
    for (event, extra) in [
        (HookEventKind::SessionStart, json!({"source": "startup"})),
        (
            HookEventKind::UserPromptSubmit,
            json!({"prompt": "トークンを記録しない"}),
        ),
    ] {
        let out = run_hook_with_env(
            event,
            Agent::ClaudeCode,
            &payload(event, extra),
            &good,
            &env,
        );
        assert_eq!(out.exit_code, 0);
    }
    // A refused token and an unreachable server both end up in hook.log.
    let bad = client_config(client_dir.path(), &base, "wrong-token-for-the-log");
    run_hook_with_env(
        HookEventKind::SessionStart,
        Agent::ClaudeCode,
        &payload(HookEventKind::SessionStart, json!({"source": "startup"})),
        &bad,
        &env,
    );
    let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let down = client_config(
        client_dir.path(),
        &format!("http://127.0.0.1:{dead_port}"),
        TOKEN,
    );
    run_hook_with_env(
        HookEventKind::SessionStart,
        Agent::ClaudeCode,
        &payload(HookEventKind::SessionStart, json!({"source": "startup"})),
        &down,
        &env,
    );
    // Authenticated and refused API calls, the metrics endpoint and MCP, all request-logged.
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    for (path, token) in [
        ("/api/v1/status", Some(TOKEN)),
        ("/api/v1/metrics", Some(TOKEN)),
        ("/api/v1/status", Some("wrong")),
        ("/api/v1/search?q=%E8%A8%98%E6%86%B6", None),
    ] {
        let mut req = http.get(format!("{base}{path}"));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().unwrap();
    }
    http.post(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "log-test", "version": "0"}}}),
        )
        .send()
        .unwrap();
    // update.log: a failed release check against an unreachable mirror.
    let status = Arc::new(parking_lot::Mutex::new(
        kioku_server::UpdateStatus::default(),
    ));
    let check = kioku_cli::auto_update::ServerCheck {
        base: format!("http://127.0.0.1:{dead_port}/releases"),
        exe: client_dir.path().join("kioku"),
        current: "0.0.1".into(),
        auto: true,
        verify: kioku_cli::update::Verify::automatic(),
        state_dir: Some(client_dir.path().join("state")),
        how: "run `kioku update`".into(),
    };
    match kioku_cli::auto_update::server_check_once(&check, &status) {
        kioku_cli::auto_update::ServerOutcome::Failed(msg) => {
            kioku_cli::auto_update::log_update(
                Some(client_dir.path()),
                &format!("kioku: auto-update failed: {msg}"),
            );
        }
        other => panic!("expected a failed check, got {other:?}"),
    }

    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let serve = read(&serve_log);
    let hook = read(&client_dir.path().join("logs").join("hook.log"));
    let update = read(&client_dir.path().join("logs").join("update.log"));
    assert!(serve.contains("kioku_http"), "request log is on: {serve}");
    assert!(serve.contains("GET /api/v1/status 401"), "{serve}");
    assert!(serve.contains("POST /mcp "), "{serve}");
    assert_eq!(hook.lines().count(), 2, "{hook}");
    assert!(hook.contains("status=401"), "{hook}");
    assert!(update.contains("auto-update failed"), "{update}");
    for (name, text) in [
        ("serve.log", &serve),
        ("hook.log", &hook),
        ("update.log", &update),
    ] {
        assert!(!text.is_empty(), "{name} is empty");
        for secret in [TOKEN, "wrong-token-for-the-log", "Bearer"] {
            assert!(!text.contains(secret), "{name} contains {secret}: {text}");
        }
    }
}
