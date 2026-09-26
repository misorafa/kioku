//! End-to-end: the real server (`kioku_server::build_app`) on an ephemeral loopback port,
//! driven by the hook handlers as library functions with `KIOKU_SERVER_URL` / token overrides.
//! M1: the Claude Code lifecycle. M2 (§16.4): the same lifecycle for Codex, Cursor and
//! Gemini CLI with their own payloads and reply formats, implicit session start, Cursor
//! late context, the Cursor sniff and the Codex SessionEnd deadline.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kioku_cli::{Agent, HookEnv, HookEventKind, HookOutcome, run_hook, run_hook_with_env};
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
    assert!(
        out.stdout.contains("\nsession: e2e-session-1  ←"),
        "{}",
        out.stdout
    );
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
            .contains("kioku_handoff_write（project=e2e-proj, session=e2e-session-1）"),
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
    let out = hook(&cfg, HookEventKind::Stop, stop_payload.clone());
    assert_eq!(out, HookOutcome::ok());
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["status"], "finalized");
    assert_eq!(info["has_agent_handoff"], true);
    assert_eq!(info["tool_uses_since_handoff"], 0);

    // 5b. Another turn of work after the handoff: the nudge comes back (the handoff is
    // stale); ignoring it, the finalize appends a rules addendum for the delta.
    run_turn(&cfg, sid, cwd);
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["tool_uses_since_handoff"], 3);
    let out = hook(&cfg, HookEventKind::Stop, stop_payload.clone());
    assert_eq!(out.exit_code, 2, "stale handoff → nudge");
    let active = with(
        base_payload(sid, cwd, HookEventKind::Stop),
        json!({"stop_hook_active": true}),
    );
    assert_eq!(hook(&cfg, HookEventKind::Stop, active), HookOutcome::ok());
    let pending = api_get(&server.base, &format!("handoffs/pending?project={PROJECT}"));
    let md = pending["handoff"]["content_md"].as_str().unwrap();
    assert_eq!(pending["handoff"]["source"], "rules");
    assert!(
        md.contains("日本語検索と引き継ぎの自動化を実装した"),
        "{md}"
    );
    assert!(md.contains("## 引き継ぎ（自動生成・追記）"), "{md}");
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
    // STATE.md's "latest handoff" section would repeat the handoff: it is stripped.
    assert!(!out.stdout.contains("## 最新の引き継ぎ"), "{}", out.stdout);
    assert_eq!(
        out.stdout
            .matches("日本語検索と引き継ぎの自動化を実装した")
            .count(),
        1
    );
    assert!(out.stdout.contains("## 最近のセッション"));
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

    // Unknown session on a prompt → implicit start (M2 §3.9): the session is created with
    // source "implicit", the prompt recorded, and the <kioku> block (with the pending
    // handoff) printed — M1 dropped the prompt with a 404 log line instead.
    let out = hook(
        &cfg,
        HookEventKind::UserPromptSubmit,
        with(
            base_payload("no-such-session", cwd, HookEventKind::UserPromptSubmit),
            json!({"prompt": "x"}),
        ),
    );
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout
            .starts_with("<kioku>\nproject: e2e (id: e2e-proj)"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("## 前回からの引き継ぎ"),
        "{}",
        out.stdout
    );
    let info = api_get(&server.base, "sessions/no-such-session");
    assert_eq!(info["counts"]["prompts"], 1);
    // SessionEnd of an unknown session still 404s → silent, one log line.
    let out = hook(
        &cfg,
        HookEventKind::SessionEnd,
        with(
            base_payload("no-such-session-2", cwd, HookEventKind::SessionEnd),
            json!({"reason": "other"}),
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
        lines[0].contains("session-end session=no-such-session-2 status=404"),
        "{log}"
    );
    assert!(
        lines[1].contains("session-start session=e2e-x status=401"),
        "{log}"
    );
}

// ---------------------------------------------------------------------------------------
// M2: Codex, Cursor, Gemini CLI

/// A docs fixture payload re-pointed at this test's session id and project dir.
fn fixture_payload(agent: Agent, name: &str, sid: &str, cwd: &Path) -> Value {
    let path = format!(
        "{}/tests/fixtures/{}/{name}.docs.json",
        env!("CARGO_MANIFEST_DIR"),
        agent.as_str()
    );
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let c = cwd.display().to_string();
    if agent == Agent::Cursor {
        v["conversation_id"] = json!(sid);
        if v.get("session_id").is_some() {
            v["session_id"] = json!(sid);
        }
        v["workspace_roots"] = json!([c]);
        if v.get("cwd").is_some() {
            v["cwd"] = json!(c);
        }
    } else {
        v["session_id"] = json!(sid);
        v["cwd"] = json!(c);
    }
    v
}

/// Client environment: a temp home, no agent variables, no process cwd.
fn agent_env(home: &Path) -> HookEnv {
    HookEnv {
        vars: HashMap::new(),
        home: Some(home.to_path_buf()),
        cwd: None,
    }
}

fn run(
    cfg: &Config,
    env: &HookEnv,
    agent: Agent,
    event: HookEventKind,
    payload: &Value,
) -> HookOutcome {
    run_hook_with_env(event, agent, &payload.to_string(), cfg, env)
}

/// The context text an agent received, or None when the reply carries none.
fn context_of(agent: Agent, out: &HookOutcome) -> Option<String> {
    assert_eq!(out.exit_code, 0, "{out:?}");
    match agent {
        Agent::ClaudeCode | Agent::Codex => Some(out.stdout.clone()).filter(|s| !s.is_empty()),
        Agent::Cursor => {
            let v: Value = serde_json::from_str(&out.stdout).expect("cursor stdout is JSON");
            v.get("additional_context")
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        Agent::GeminiCli => {
            let v: Value = serde_json::from_str(&out.stdout).expect("gemini stdout is JSON");
            v.pointer("/hookSpecificOutput/additionalContext")
                .and_then(Value::as_str)
                .map(str::to_string)
        }
    }
}

/// Asserts the agent-specific Stop nudge shape; returns the nudge text.
fn nudge_of(agent: Agent, out: &HookOutcome) -> String {
    match agent {
        Agent::ClaudeCode | Agent::Codex => {
            assert_eq!(out.exit_code, 2, "{out:?}");
            assert!(out.stdout.is_empty());
            out.stderr.clone()
        }
        Agent::Cursor => {
            assert_eq!(out.exit_code, 0);
            assert!(out.stderr.is_empty());
            let v: Value = serde_json::from_str(&out.stdout).unwrap();
            assert_eq!(v.as_object().unwrap().len(), 1, "{v}");
            v["followup_message"].as_str().unwrap().to_string()
        }
        Agent::GeminiCli => {
            assert_eq!(out.exit_code, 0);
            assert!(out.stderr.is_empty());
            let v: Value = serde_json::from_str(&out.stdout).unwrap();
            assert_eq!(v["decision"], "deny", "{v}");
            v["reason"].as_str().unwrap().to_string()
        }
    }
}

/// The agent's "silent success" reply.
fn assert_silent(agent: Agent, event: HookEventKind, out: &HookOutcome) {
    let want = match agent {
        Agent::ClaudeCode | Agent::Codex => "",
        Agent::Cursor if event == HookEventKind::UserPromptSubmit => "{\"continue\":true}\n",
        Agent::Cursor | Agent::GeminiCli => "{}\n",
    };
    assert_eq!(out.stdout, want, "{agent:?} {event:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(out.exit_code, 0);
}

/// (fixture, payload overrides) of the prompt and the three tool uses per agent.
fn turn_fixtures(agent: Agent, cwd: &Path) -> (&'static str, Vec<(&'static str, Value)>) {
    let file = |p: &str| cwd.join(p).display().to_string();
    match agent {
        Agent::Codex => (
            "user_prompt_submit",
            vec![
                ("post_tool_use_bash", json!({})),
                ("post_tool_use_apply_patch", json!({})),
                (
                    "post_tool_use_bash",
                    json!({"tool_use_id": "call_Ef56", "tool_input": {"command": "git commit -m \"codex 対応\""}}),
                ),
            ],
        ),
        Agent::Cursor => (
            "before_submit_prompt",
            vec![
                ("after_file_edit", json!({"file_path": file("src/lib.rs")})),
                ("post_tool_use_shell", json!({})),
                (
                    "post_tool_use_read",
                    json!({"tool_input": {"target_file": file("src/main.rs")}}),
                ),
            ],
        ),
        Agent::GeminiCli => (
            "before_agent",
            vec![
                (
                    "after_tool_replace",
                    json!({"tool_input": {"file_path": file("src/lib.rs"), "old_string": "a", "new_string": "b"}}),
                ),
                ("after_tool_shell_error", json!({})),
                (
                    "after_tool_replace",
                    json!({"tool_name": "read_file", "tool_input": {"file_path": file("src/main.rs")}}),
                ),
            ],
        ),
        Agent::ClaudeCode => unreachable!(),
    }
}

fn stop_fixture(agent: Agent) -> &'static str {
    match agent {
        Agent::GeminiCli => "after_agent",
        _ => "stop",
    }
}

fn agent_lifecycle(agent: Agent) {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let cfg = client_config(client_dir.path(), &server.base, TOKEN, &[]);
    let env = agent_env(home.path());
    let label = agent.as_str();
    let sid = format!("{label}-session-1");
    let p = |name: &str, extra: Value| with(fixture_payload(agent, name, &sid, cwd), extra);

    // 1. session start → the <kioku> block in the agent's format; no handoff yet
    let out = run(
        &cfg,
        &env,
        agent,
        HookEventKind::SessionStart,
        &p("session_start", json!({})),
    );
    let block = context_of(agent, &out).expect("session start context");
    assert!(
        block.starts_with("<kioku>\nproject: e2e (id: e2e-proj)"),
        "{block}"
    );
    assert!(block.contains(&format!("session: {sid}  ←")), "{block}");
    assert!(!block.contains("## 前回からの引き継ぎ"));

    // 2. prompt + 3 tool uses
    let (prompt, tools) = turn_fixtures(agent, cwd);
    let out = run(
        &cfg,
        &env,
        agent,
        HookEventKind::UserPromptSubmit,
        &p(prompt, json!({})),
    );
    assert_silent(agent, HookEventKind::UserPromptSubmit, &out);
    for (i, (name, extra)) in tools.into_iter().enumerate() {
        let out = run(
            &cfg,
            &env,
            agent,
            HookEventKind::PostToolUse,
            &p(name, extra),
        );
        if agent == Agent::Cursor && i == 1 {
            // Cursor late context: the block again, via postToolUse additional_context — on
            // the first native postToolUse (the afterFileEdit before it cannot carry it)
            let late = context_of(agent, &out).expect("cursor late context");
            assert!(
                late.starts_with("<kioku>\nproject: e2e (id: e2e-proj)"),
                "{late}"
            );
            assert!(late.contains(&format!("session: {sid}  ←")));
        } else {
            assert_silent(agent, HookEventKind::PostToolUse, &out);
        }
    }
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["counts"]["prompts"], 1, "{info}");
    assert_eq!(info["counts"]["tool_uses"], 3, "{info}");
    if agent == Agent::Cursor {
        assert!(
            client_dir
                .path()
                .join("state/cursor-ctx")
                .join(&sid)
                .exists()
        );
    }

    // 3. stop → agent-specific nudge (generic text), session stays open
    let stop = p(stop_fixture(agent), json!({}));
    let out = run(&cfg, &env, agent, HookEventKind::Stop, &stop);
    let nudge = nudge_of(agent, &out);
    assert!(
        nudge.contains(&format!(
            "kioku_handoff_write（project=e2e-proj, session={sid}）"
        )),
        "{nudge}"
    );
    assert!(
        nudge.contains("記録済みなら、そのまま終了してください。"),
        "{nudge}"
    );
    assert!(!nudge.contains("stop_hook_active"));
    assert_eq!(
        api_get(&server.base, &format!("sessions/{sid}"))["status"],
        "open"
    );

    // 4. the agent writes its handoff (what kioku_handoff_write does)
    let summary = format!("{label} で引き継ぎの自動化を実装した");
    let resp = http()
        .post(format!("{}/api/v1/handoffs", server.base))
        .bearer_auth(TOKEN)
        .json(&json!({
            "project": PROJECT, "session": sid, "summary": summary,
            "next_steps": [format!("{label} の実ペイロードを取得する")],
            "open_questions": [], "decisions": []
        }))
        .send()
        .unwrap();
    assert!(resp.status().is_success());

    // 5. stop again → finalize, silent
    let out = run(&cfg, &env, agent, HookEventKind::Stop, &stop);
    assert_silent(agent, HookEventKind::Stop, &out);
    let info = api_get(&server.base, &format!("sessions/{sid}"));
    assert_eq!(info["status"], "finalized", "{info}");
    // the session page carries the agent label and the normalized digest
    let fin: Value = http()
        .post(format!("{}/api/v1/sessions/{sid}/finalize", server.base))
        .bearer_auth(TOKEN)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let page = api_get(
        &server.base,
        &format!("pages/{}", fin["session_page"].as_str().unwrap()),
    );
    assert_eq!(page["frontmatter"]["agent"], label, "{page}");
    let body = page["body"].as_str().unwrap();
    assert!(body.contains(&format!("エージェント: {label}")), "{body}");
    match agent {
        Agent::Codex => {
            assert!(body.contains("crates/kioku-cli/src/event.rs"), "{body}");
            assert!(body.contains("docs/notes/codex.md"), "{body}");
            assert!(body.contains("cargo test -p kioku-core"), "{body}");
        }
        Agent::Cursor => {
            assert!(body.contains("src/lib.rs"), "{body}");
            assert!(body.contains("cargo test -p kioku-cli"), "{body}");
        }
        Agent::GeminiCli => {
            assert!(body.contains("src/lib.rs"), "{body}");
            assert!(body.contains("cargo build"), "{body}");
        }
        Agent::ClaudeCode => {}
    }
    // session end is silent too
    let end = fixture_payload(agent, "session_end", &sid, cwd);
    assert_silent(
        agent,
        HookEventKind::SessionEnd,
        &run(&cfg, &env, agent, HookEventKind::SessionEnd, &end),
    );

    // 6. next session start shows the handoff
    let sid2 = format!("{label}-session-2");
    let start2 = fixture_payload(agent, "session_start", &sid2, cwd);
    let out = run(&cfg, &env, agent, HookEventKind::SessionStart, &start2);
    let block = context_of(agent, &out).expect("next session context");
    assert!(block.contains("## 前回からの引き継ぎ"), "{block}");
    assert!(block.contains(&summary), "{block}");
    assert!(block.contains(&format!("{label} の実ペイロードを取得する")));
    assert!(
        !client_dir.path().join("logs/hook.log").exists(),
        "nothing failed"
    );
}

#[test]
fn codex_lifecycle() {
    agent_lifecycle(Agent::Codex);
}

#[test]
fn cursor_lifecycle() {
    agent_lifecycle(Agent::Cursor);
}

#[test]
fn gemini_lifecycle() {
    agent_lifecycle(Agent::GeminiCli);
}

#[test]
fn implicit_session_start_per_agent() {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let cfg = client_config(client_dir.path(), &server.base, TOKEN, &[]);
    let env = agent_env(home.path());

    // Codex / Gemini: the prompt on an unknown session starts it and returns the block.
    for (agent, prompt) in [
        (Agent::Codex, "user_prompt_submit"),
        (Agent::GeminiCli, "before_agent"),
    ] {
        let sid = format!("implicit-{}", agent.as_str());
        let out = run(
            &cfg,
            &env,
            agent,
            HookEventKind::UserPromptSubmit,
            &fixture_payload(agent, prompt, &sid, cwd),
        );
        let block = context_of(agent, &out).expect("implicit start block");
        assert!(
            block.starts_with("<kioku>\nproject: e2e (id: e2e-proj)"),
            "{block}"
        );
        let info = api_get(&server.base, &format!("sessions/{sid}"));
        assert_eq!(
            info["counts"]["prompts"], 1,
            "retried after the start: {info}"
        );
    }
    // A tool use on an unknown session: started, recorded, silent.
    let sid = "implicit-codex-tool";
    let out = run(
        &cfg,
        &env,
        Agent::Codex,
        HookEventKind::PostToolUse,
        &fixture_payload(Agent::Codex, "post_tool_use_bash", sid, cwd),
    );
    assert_silent(Agent::Codex, HookEventKind::PostToolUse, &out);
    assert_eq!(
        api_get(&server.base, &format!("sessions/{sid}"))["counts"]["tool_uses"],
        1
    );

    // Cursor: the prompt cannot carry context → {"continue":true}; the next tool use
    // delivers the block through the late-context path.
    let sid = "implicit-cursor";
    let out = run(
        &cfg,
        &env,
        Agent::Cursor,
        HookEventKind::UserPromptSubmit,
        &fixture_payload(Agent::Cursor, "before_submit_prompt", sid, cwd),
    );
    assert_silent(Agent::Cursor, HookEventKind::UserPromptSubmit, &out);
    let out = run(
        &cfg,
        &env,
        Agent::Cursor,
        HookEventKind::PostToolUse,
        &fixture_payload(Agent::Cursor, "post_tool_use_shell", sid, cwd),
    );
    let block = context_of(Agent::Cursor, &out).expect("late context after implicit start");
    assert!(block.contains(&format!("session: {sid}  ←")));

    // Stop on an unknown session: started, nothing to finalize → silent.
    for agent in [Agent::Codex, Agent::Cursor, Agent::GeminiCli] {
        let sid = format!("implicit-stop-{}", agent.as_str());
        let out = run(
            &cfg,
            &env,
            agent,
            HookEventKind::Stop,
            &fixture_payload(agent, stop_fixture(agent), &sid, cwd),
        );
        assert_silent(agent, HookEventKind::Stop, &out);
        let info = api_get(&server.base, &format!("sessions/{sid}"));
        assert_eq!(info["status"], "open", "{info}");
        assert_eq!(info["counts"]["tool_uses"], 0);
    }
    assert!(!client_dir.path().join("logs/hook.log").exists());
}

#[test]
fn cursor_late_context_once_per_session_and_toggle() {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let mut cfg = client_config(client_dir.path(), &server.base, TOKEN, &[]);
    let env = agent_env(home.path());
    let a = Agent::Cursor;
    let tool = |sid: &str| fixture_payload(a, "post_tool_use_shell", sid, cwd);

    for sid in ["late-1", "late-2"] {
        run(
            &cfg,
            &env,
            a,
            HookEventKind::SessionStart,
            &fixture_payload(a, "session_start", sid, cwd),
        );
        // afterFileEdit / postToolUseFailure cannot carry additional_context: recorded, but
        // the once-per-session marker stays unconsumed for the next real postToolUse.
        for name in ["after_file_edit", "post_tool_use_failure"] {
            let out = run(
                &cfg,
                &env,
                a,
                HookEventKind::PostToolUse,
                &fixture_payload(a, name, sid, cwd),
            );
            assert_silent(a, HookEventKind::PostToolUse, &out);
            assert!(
                !client_dir
                    .path()
                    .join("state/cursor-ctx")
                    .join(sid)
                    .exists(),
                "{sid}: {name} consumed the marker"
            );
        }
        let first = run(&cfg, &env, a, HookEventKind::PostToolUse, &tool(sid));
        assert!(context_of(a, &first).is_some(), "{sid}: first tool use");
        for _ in 0..2 {
            let again = run(&cfg, &env, a, HookEventKind::PostToolUse, &tool(sid));
            assert_silent(a, HookEventKind::PostToolUse, &again);
        }
    }
    // Aborted stop: finalize without a nudge even though 3 tools ran.
    let stop = with(
        fixture_payload(a, "stop", "late-1", cwd),
        json!({"status": "aborted"}),
    );
    assert_silent(
        a,
        HookEventKind::Stop,
        &run(&cfg, &env, a, HookEventKind::Stop, &stop),
    );
    assert_eq!(
        api_get(&server.base, "sessions/late-1")["status"],
        "finalized"
    );
    // loop_count > 0 (a follow-up already triggered): no nudge either.
    let stop = fixture_payload(a, "stop_loop1", "late-2", cwd);
    assert_silent(
        a,
        HookEventKind::Stop,
        &run(&cfg, &env, a, HookEventKind::Stop, &stop),
    );

    // Disabled: no late context at all.
    cfg.client.cursor_late_context = false;
    run(
        &cfg,
        &env,
        a,
        HookEventKind::SessionStart,
        &fixture_payload(a, "session_start", "late-3", cwd),
    );
    let out = run(&cfg, &env, a, HookEventKind::PostToolUse, &tool("late-3"));
    assert_silent(a, HookEventKind::PostToolUse, &out);
    assert!(!client_dir.path().join("state/cursor-ctx/late-3").exists());
}

#[test]
fn cursor_sniff_in_claude_hooks() {
    let server = start_server();
    let client_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let cwd = proj.path();
    let cfg = client_config(client_dir.path(), &server.base, TOKEN, &[]);
    let env = agent_env(home.path());

    // Cursor running kioku's Claude Code hooks, no native Cursor hooks: handled as Cursor.
    let start = fixture_payload(Agent::Cursor, "session_start", "sniff-1", cwd);
    let out = run(
        &cfg,
        &env,
        Agent::ClaudeCode,
        HookEventKind::SessionStart,
        &start,
    );
    let block = context_of(Agent::Cursor, &out).expect("re-dispatched to the Cursor renderer");
    assert!(block.contains("session: sniff-1  ←"));
    // A Claude-shaped payload is Claude Code even with CURSOR_VERSION in the environment
    // (Claude Code started from Cursor's terminal inherits it): the env alone never decides.
    let mut cursor_env = env.clone();
    cursor_env
        .vars
        .insert("CURSOR_VERSION".into(), "3.1.0".into());
    let prompt = with(
        base_payload("sniff-1", cwd, HookEventKind::UserPromptSubmit),
        json!({"prompt": "Cursor から Claude 形式で"}),
    );
    let out = run(
        &cfg,
        &cursor_env,
        Agent::ClaudeCode,
        HookEventKind::UserPromptSubmit,
        &prompt,
    );
    assert_silent(Agent::ClaudeCode, HookEventKind::UserPromptSubmit, &out);
    assert_eq!(
        api_get(&server.base, "sessions/sniff-1")["counts"]["prompts"],
        1
    );
    let fin: Value = http()
        .post(format!("{}/api/v1/sessions/sniff-1/finalize", server.base))
        .bearer_auth(TOKEN)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let page = api_get(
        &server.base,
        &format!("pages/{}", fin["session_page"].as_str().unwrap()),
    );
    assert_eq!(page["frontmatter"]["agent"], "cursor");

    // Native Cursor hooks present (project level): the Claude-format invocation is Silent
    // and records nothing.
    std::fs::create_dir_all(cwd.join(".cursor")).unwrap();
    std::fs::write(
        cwd.join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"sessionStart":[{"command":"/opt/kioku hook session-start --agent cursor"}]}}"#,
    )
    .unwrap();
    let start = fixture_payload(Agent::Cursor, "session_start", "sniff-2", cwd);
    let out = run(
        &cfg,
        &env,
        Agent::ClaudeCode,
        HookEventKind::SessionStart,
        &start,
    );
    assert_eq!(out, HookOutcome::ok());
    let status = http()
        .get(format!("{}/api/v1/sessions/sniff-2", server.base))
        .bearer_auth(TOKEN)
        .send()
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 404);
    // A plain Claude Code payload is not affected by Cursor's files.
    let out = hook(
        &cfg,
        HookEventKind::SessionStart,
        with(
            base_payload("claude-1", cwd, HookEventKind::SessionStart),
            json!({"source": "startup"}),
        ),
    );
    assert!(out.stdout.starts_with("<kioku>"));
}

#[test]
fn codex_session_end_deadline_is_capped() {
    // A server that accepts connections and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    let client_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let proj = project_dir();
    let mut cfg = client_config(client_dir.path(), &format!("http://{addr}"), TOKEN, &[]);
    cfg.client.timeout_ms = 10_000;
    let env = agent_env(home.path());
    let end = fixture_payload(Agent::Codex, "session_end", "slow", proj.path());
    let t0 = std::time::Instant::now();
    let out = run(&cfg, &env, Agent::Codex, HookEventKind::SessionEnd, &end);
    let took = t0.elapsed();
    assert_eq!(out, HookOutcome::ok());
    assert!(took >= std::time::Duration::from_millis(2400), "{took:?}");
    assert!(
        took < std::time::Duration::from_millis(3000),
        "{took:?}: Codex kills SessionEnd at 3 s"
    );
    let log = std::fs::read_to_string(client_dir.path().join("logs/hook.log")).unwrap();
    assert!(log.contains("session-end session=slow error: "), "{log}");
}

/// A non-UTF-8 environment (variable value and name) must never panic the hook binary:
/// Gemini still gets JSON-only stdout and exit 0 (regression: `std::env::vars()` panicked).
#[cfg(unix)]
#[test]
fn hook_binary_survives_a_non_utf8_environment() {
    use std::ffi::OsStr;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};

    let server = start_server();
    let home = tempfile::tempdir().unwrap();
    let project = project_dir();
    let payload = fixture_payload(
        Agent::GeminiCli,
        "session_start",
        "sess-non-utf8",
        project.path(),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .args(["hook", "session-start", "--agent", "gemini-cli"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("KIOKU_DATA_DIR", home.path().join(".kioku"))
        .env("KIOKU_SERVER_URL", &server.base)
        .env("KIOKU_AUTH_TOKEN", TOKEN)
        .env("KIOKU_WEIRD", OsStr::from_bytes(b"caf\xe9 \xff"))
        .env(OsStr::from_bytes(b"BAD_\xffNAME"), "x")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    let v: Value = serde_json::from_str(&stdout).expect("stdout is exactly one JSON object");
    let ctx = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(ctx.contains(PROJECT), "{stdout}");
}
