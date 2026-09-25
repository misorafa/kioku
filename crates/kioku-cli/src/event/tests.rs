//! Parser tests: M1 Claude Code fixtures and the M2 per-agent fixtures (spec §16.1–2).

use super::*;

const SID: &str = "8d3c1f0e-5b7a-4c2d-9e1f-0a2b3c4d5e6f";

fn fixture(kind: HookEventKind, file: &str) -> HookEvent {
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    let ev = parse_event(Agent::ClaudeCode, kind, &text).unwrap();
    assert_eq!(ev.agent, "claude-code");
    assert_eq!(ev.event, kind);
    assert_eq!(ev.session_id, SID);
    assert_eq!(ev.cwd, "/home/u/kioku");
    assert_eq!(
        ev.raw["hook_event_name"].as_str(),
        Some(kind.claude_code_name())
    );
    ev
}

#[test]
fn session_start_fixture() {
    let ev = fixture(HookEventKind::SessionStart, "session_start.json");
    assert_eq!(ev.source.as_deref(), Some("startup"));
    assert!(ev.prompt.is_none() && ev.tool_name.is_none());
}

#[test]
fn user_prompt_submit_fixture() {
    let ev = fixture(HookEventKind::UserPromptSubmit, "user_prompt_submit.json");
    assert!(
        ev.prompt
            .as_deref()
            .unwrap()
            .starts_with("引き継ぎ書の自動生成")
    );
}

#[test]
fn post_tool_use_fixture() {
    let ev = fixture(HookEventKind::PostToolUse, "post_tool_use.json");
    assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
    assert_eq!(
        ev.tool_input.as_ref().unwrap()["file_path"],
        "/home/u/kioku/crates/kioku-core/src/handoff.rs"
    );
    assert_eq!(ev.tool_response.as_ref().unwrap()["userModified"], false);
    assert_eq!(
        ev.tool_use_id.as_deref(),
        Some("toolu_01KxQ7mY3bT9pVwE2rN8sL4d")
    );
}

#[test]
fn stop_fixture() {
    let ev = fixture(HookEventKind::Stop, "stop.json");
    assert!(!ev.stop_hook_active);
    let mut raw = ev.raw.clone();
    raw["stop_hook_active"] = Value::Bool(true);
    let ev = parse_claude_code(HookEventKind::Stop, raw).unwrap();
    assert!(ev.stop_hook_active);
}

#[test]
fn pre_compact_fixture() {
    let ev = fixture(HookEventKind::PreCompact, "pre_compact.json");
    assert_eq!(ev.trigger.as_deref(), Some("auto"));
}

#[test]
fn session_end_fixture() {
    let ev = fixture(HookEventKind::SessionEnd, "session_end.json");
    assert_eq!(ev.reason.as_deref(), Some("prompt_input_exit"));
}

#[test]
fn rejects_garbage() {
    let k = HookEventKind::Stop;
    assert!(parse_event(Agent::ClaudeCode, k, "").is_err());
    assert!(parse_event(Agent::ClaudeCode, k, "[1]").is_err());
    assert!(parse_event(Agent::ClaudeCode, k, r#"{"cwd":"/x"}"#).is_err());
    let ev = parse_event(Agent::ClaudeCode, k, r#"{"session_id":"s1"}"#).unwrap();
    assert_eq!(ev.cwd, "");
    assert!(!ev.stop_hook_active);
}

#[test]
fn cli_names_match_value_enum() {
    for kind in ALL_EVENTS {
        assert_eq!(
            HookEventKind::from_str(kind.cli_name(), false).unwrap(),
            kind
        );
    }
    assert_eq!(
        Agent::from_str("claude-code", false).unwrap(),
        Agent::ClaudeCode
    );
}

// ---------------------------------------------------------------------------------------
// M2 fixtures: tests/fixtures/<agent>/<name>.docs.json (built from the spec field lists)
// and, once captured with `kioku hook-dump extract`, <name>.captured.json. Every test runs
// on both; value checks that only hold for the docs payloads are guarded by `docs`.

/// One fixture variant: whether it is the docs-derived one, and its raw text.
struct Variant {
    docs: bool,
    text: String,
}

fn variants(agent: Agent, name: &str) -> Vec<Variant> {
    let dir = format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        agent.as_str()
    );
    let mut out = Vec::new();
    for (docs, suffix) in [(true, "docs"), (false, "captured")] {
        let path = format!("{dir}/{name}.{suffix}.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push(Variant { docs, text });
        }
    }
    assert!(!out.is_empty(), "missing fixture {dir}/{name}.docs.json");
    out
}

/// Parses every variant of a fixture (payload-only env) and checks the common invariants.
fn each(agent: Agent, kind: HookEventKind, name: &str, check: impl Fn(&HookEvent, bool)) {
    for v in variants(agent, name) {
        let ev = parse_event(agent, kind, &v.text)
            .unwrap_or_else(|e| panic!("{}/{name}: {e:#}", agent.as_str()));
        assert_eq!(ev.agent, agent.as_str());
        assert_eq!(ev.event, kind);
        assert!(!ev.session_id.is_empty());
        assert!(!ev.native_event.is_empty(), "{name}: hook_event_name");
        if v.docs {
            assert!(
                ev.cwd.starts_with("/Users/me/src/"),
                "{name}: cwd {:?}",
                ev.cwd
            );
        }
        check(&ev, v.docs);
    }
}

const CODEX_SID: &str = "019a6f1e-3c2b-7d10-9a55-2f8e4b1c0d77";
const CURSOR_SID: &str = "c7e1a4b2-9f3d-4e8a-b6c1-0d2e3f4a5b6c";
const GEMINI_SID: &str = "5f0c2d1e-8a7b-4c3d-9e2f-1a0b9c8d7e6f";
const ROOT: &str = "/Users/me/src/kioku";

fn env_of(pairs: &[(&str, &str)]) -> HookEnv {
    HookEnv {
        vars: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        home: None,
        cwd: Some(PathBuf::from("/process/cwd")),
    }
}

// ---- Codex ------------------------------------------------------------------------------

#[test]
fn codex_lifecycle_fixtures() {
    use HookEventKind::*;
    let a = Agent::Codex;
    each(a, SessionStart, "session_start", |ev, docs| {
        assert_eq!(ev.native_event, "SessionStart");
        if docs {
            assert_eq!(ev.session_id, CODEX_SID);
            assert_eq!(ev.cwd, ROOT);
            assert_eq!(ev.source.as_deref(), Some("startup"));
        }
        assert!(ev.workspace_roots.is_empty());
    });
    each(a, UserPromptSubmit, "user_prompt_submit", |ev, docs| {
        if docs {
            assert!(
                ev.prompt
                    .as_deref()
                    .unwrap()
                    .starts_with("引き継ぎの自動化")
            );
            assert_eq!(ev.turn_id.as_deref(), Some("turn_3"));
        }
    });
    each(a, PreCompact, "pre_compact", |ev, docs| {
        if docs {
            assert_eq!(ev.trigger.as_deref(), Some("auto"));
        }
    });
    each(a, Stop, "stop", |ev, _| assert!(!ev.stop_hook_active));
    each(a, Stop, "stop_active", |ev, _| assert!(ev.stop_hook_active));
    each(a, SessionEnd, "session_end", |ev, docs| {
        if docs {
            assert_eq!(ev.session_id, CODEX_SID);
            assert_eq!(ev.reason.as_deref(), Some("other"));
        }
    });
}

#[test]
fn codex_bash_is_normalized() {
    each(
        Agent::Codex,
        HookEventKind::PostToolUse,
        "post_tool_use_bash",
        |ev, docs| {
            assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
            assert_eq!(ev.native_tool.as_deref(), Some("Bash"));
            let input = ev.tool_input.as_ref().unwrap();
            assert_eq!(input.as_object().unwrap().len(), 1, "only {{command}}");
            if docs {
                assert_eq!(input["command"], "cargo test -p kioku-core");
                // tool_response kept as is (a string here; UNVERIFIED shape, §18 #3)
                assert_eq!(
                    ev.tool_response,
                    Some(json!("test result: ok. 126 passed; 0 failed"))
                );
                assert_eq!(ev.tool_use_id.as_deref(), Some("call_Ab12"));
            }
        },
    );
    // an argv array is joined with spaces
    let raw = json!({"session_id": "s", "cwd": "/w", "tool_name": "Bash",
        "tool_input": {"command": ["bash", "-lc", "cargo fmt"]}, "tool_response": {"exit_code": 0}});
    let ev = parse_value(
        Agent::Codex,
        HookEventKind::PostToolUse,
        raw,
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(
        ev.tool_input,
        Some(json!({"command": "bash -lc cargo fmt"}))
    );
    assert_eq!(ev.tool_response, Some(json!({"exit_code": 0})));
}

#[test]
fn codex_apply_patch_paths_survive_truncation() {
    each(
        Agent::Codex,
        HookEventKind::PostToolUse,
        "post_tool_use_apply_patch",
        |ev, docs| {
            assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
            assert_eq!(ev.native_tool.as_deref(), Some("apply_patch"));
            let input = ev.tool_input.as_ref().unwrap();
            let patch = input["patch"].as_str().unwrap();
            assert!(patch.chars().count() <= PATCH_KEEP_CHARS);
            if docs {
                let original = ev.raw["tool_input"]["command"].as_str().unwrap();
                assert!(original.chars().count() > PATCH_KEEP_CHARS);
                assert!(!patch.contains("*** Move to:"), "Move line is past the cut");
                assert_eq!(
                    input["file_paths"],
                    json!([
                        format!("{ROOT}/crates/kioku-cli/src/event.rs"),
                        format!("{ROOT}/docs/old-notes.md"),
                        format!("{ROOT}/docs/notes/codex.md"),
                    ])
                );
            }
        },
    );
}

#[test]
fn patch_paths_parsing() {
    let patch = "*** Begin Patch\r\n*** Add File: a.rs\r\n+x\n*** Delete File: /abs/b.rs\n*** Update File: a.rs\n*** Move to: c.rs\n*** Update File:   \n*** End Patch";
    assert_eq!(patch_paths(patch), vec!["a.rs", "/abs/b.rs", "c.rs"]);
    assert!(patch_paths("no patch here").is_empty());
    // no cwd → relative paths stay relative; input without a patch is kept
    let ev = parse_value(
        Agent::Codex,
        HookEventKind::PostToolUse,
        json!({"session_id": "s", "tool_name": "apply_patch", "tool_input": {"command": ["apply_patch", patch]}}),
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(
        ev.tool_input.unwrap()["file_paths"],
        json!(["a.rs", "/abs/b.rs", "c.rs"])
    );
    let ev = parse_value(
        Agent::Codex,
        HookEventKind::PostToolUse,
        json!({"session_id": "s", "tool_name": "apply_patch", "tool_input": {"other": 1}}),
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
    assert_eq!(ev.tool_input, Some(json!({"other": 1})));
}

// ---- Cursor -----------------------------------------------------------------------------

#[test]
fn cursor_lifecycle_fixtures() {
    use HookEventKind::*;
    let a = Agent::Cursor;
    each(a, SessionStart, "session_start", |ev, docs| {
        assert_eq!(ev.native_event, "sessionStart");
        assert!(!ev.workspace_roots.is_empty());
        if docs {
            assert_eq!(ev.session_id, CURSOR_SID);
            // no payload cwd: workspace_roots[0]
            assert!(ev.raw.get("cwd").is_none());
            assert_eq!(ev.cwd, ROOT);
        }
    });
    each(a, UserPromptSubmit, "before_submit_prompt", |ev, docs| {
        assert_eq!(ev.native_event, "beforeSubmitPrompt");
        if docs {
            assert!(
                ev.prompt
                    .as_deref()
                    .unwrap()
                    .starts_with("引き継ぎを確認して")
            );
        }
    });
    each(a, PreCompact, "pre_compact", |ev, docs| {
        if docs {
            assert_eq!(ev.trigger.as_deref(), Some("auto"));
        }
    });
    each(a, Stop, "stop", |ev, docs| {
        assert!(!ev.stop_hook_active);
        if docs {
            assert_eq!(ev.loop_count, Some(0));
            assert_eq!(ev.stop_status.as_deref(), Some("completed"));
        }
    });
    each(a, Stop, "stop_loop1", |ev, _| {
        assert!(ev.stop_hook_active, "loop_count > 0");
        assert!(ev.loop_count.unwrap() > 0);
    });
    each(a, Stop, "claude_import_stop", |ev, _| {
        assert_eq!(ev.agent, "cursor");
    });
    each(a, SessionEnd, "session_end", |ev, docs| {
        if docs {
            assert_eq!(ev.reason.as_deref(), Some("user_close"));
        }
    });
}

#[test]
fn cursor_tool_normalization() {
    use HookEventKind::PostToolUse;
    let a = Agent::Cursor;
    each(a, PostToolUse, "post_tool_use_shell", |ev, docs| {
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert_eq!(ev.native_tool.as_deref(), Some("Shell"));
        assert_eq!(
            ev.tool_input.as_ref().unwrap().as_object().unwrap().len(),
            1
        );
        if docs {
            assert_eq!(
                ev.tool_input,
                Some(json!({"command": "cargo test -p kioku-cli"}))
            );
            // tool_output is a JSON string → parsed
            assert_eq!(ev.tool_response.as_ref().unwrap()["exitCode"], 0);
        }
    });
    each(a, PostToolUse, "post_tool_use_read", |ev, docs| {
        assert_eq!(ev.tool_name.as_deref(), Some("Read"));
        if docs {
            assert_eq!(
                ev.tool_input,
                Some(json!({"file_path": format!("{ROOT}/crates/kioku-cli/src/hook.rs")}))
            );
        }
    });
    each(a, PostToolUse, "post_tool_use_failure", |ev, docs| {
        assert_eq!(ev.native_event, "postToolUseFailure");
        let resp = ev.tool_response.as_ref().unwrap();
        assert_eq!(resp["is_error"], true);
        if docs {
            assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
            assert_eq!(ev.tool_input, Some(json!({"command": "npm test"})));
            assert_eq!(resp["error"], "Command timed out after 30s");
            assert_eq!(resp["failure_type"], "timeout");
        }
    });
    each(a, PostToolUse, "after_file_edit", |ev, docs| {
        assert_eq!(ev.native_event, "afterFileEdit");
        assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
        assert_eq!(ev.native_tool.as_deref(), Some("afterFileEdit"));
        if docs {
            assert_eq!(
                ev.tool_input,
                Some(json!({"file_path": format!("{ROOT}/crates/kioku-cli/src/event.rs")}))
            );
            assert_eq!(ev.tool_response, Some(json!({"edits": 2})));
        }
    });
    each(
        a,
        PostToolUse,
        "post_tool_use_empty_conversation_id",
        |ev, docs| {
            if docs {
                assert_eq!(ev.raw["conversation_id"], "");
                assert_eq!(ev.session_id, CURSOR_SID, "falls back to session_id");
            }
        },
    );
}

#[test]
fn cursor_read_path_keys_and_odd_outputs() {
    for key in ["file_path", "path", "target_file", "filePath"] {
        let raw = json!({"conversation_id": "c", "workspace_roots": ["/w"], "tool_name": "Read",
            "tool_input": {key: "/w/a.rs"}, "tool_output": "not json"});
        let ev = parse_value(
            Agent::Cursor,
            HookEventKind::PostToolUse,
            raw,
            &HookEnv::default(),
        )
        .unwrap();
        assert_eq!(
            ev.tool_input,
            Some(json!({"file_path": "/w/a.rs"})),
            "{key}"
        );
        assert_eq!(ev.tool_response, Some(json!("not json")));
    }
    // unknown key: input kept; Write / MCP tools pass through untouched
    let raw = json!({"conversation_id": "c", "tool_name": "Read", "tool_input": {"file": "x"}});
    let ev = parse_value(
        Agent::Cursor,
        HookEventKind::PostToolUse,
        raw,
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(ev.tool_input, Some(json!({"file": "x"})));
    let raw = json!({"conversation_id": "c", "tool_name": "MCP:kioku_query", "tool_input": {"query": "引き継ぎ"},
        "tool_output": {"already": "object"}});
    let ev = parse_value(
        Agent::Cursor,
        HookEventKind::PostToolUse,
        raw,
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(ev.tool_name.as_deref(), Some("MCP:kioku_query"));
    assert_eq!(ev.tool_response, Some(json!({"already": "object"})));
}

#[test]
fn cursor_session_id_and_cwd_resolution() {
    let k = HookEventKind::Stop;
    // conversation_id wins over session_id; empty conversation_id falls back
    let ev = parse_value(
        Agent::Cursor,
        k,
        json!({"conversation_id": "conv", "session_id": "sess"}),
        &HookEnv::default(),
    )
    .unwrap();
    assert_eq!(ev.session_id, "conv");
    // neither → parse error (logged, fail-open)
    assert!(
        parse_value(
            Agent::Cursor,
            k,
            json!({"conversation_id": "", "workspace_roots": ["/w"]}),
            &HookEnv::default()
        )
        .is_err()
    );
    // payload cwd → workspace_roots[0] → CURSOR_PROJECT_DIR → CLAUDE_PROJECT_DIR; never the process cwd
    let env = env_of(&[
        ("CURSOR_PROJECT_DIR", "/env/cursor"),
        ("CLAUDE_PROJECT_DIR", "/env/claude"),
    ]);
    let cwd = |raw: Value, env: &HookEnv| parse_value(Agent::Cursor, k, raw, env).unwrap().cwd;
    assert_eq!(
        cwd(
            json!({"conversation_id": "c", "cwd": "/p", "workspace_roots": ["/r1", "/r2"]}),
            &env
        ),
        "/p"
    );
    assert_eq!(
        cwd(
            json!({"conversation_id": "c", "workspace_roots": ["/r1", "/r2"]}),
            &env
        ),
        "/r1"
    );
    assert_eq!(
        cwd(json!({"conversation_id": "c", "workspace_roots": []}), &env),
        "/env/cursor"
    );
    assert_eq!(
        cwd(
            json!({"conversation_id": "c"}),
            &env_of(&[("CLAUDE_PROJECT_DIR", "/env/claude")])
        ),
        "/env/claude"
    );
    // relative paths never qualify; nothing left → empty (the handler drops the event)
    assert_eq!(
        cwd(
            json!({"conversation_id": "c", "cwd": "rel", "workspace_roots": ["also/rel"]}),
            &env_of(&[])
        ),
        ""
    );
    // Windows roots are absolute too
    assert_eq!(
        cwd(
            json!({"conversation_id": "c", "workspace_roots": ["C:\\src\\app"]}),
            &env_of(&[])
        ),
        "C:\\src\\app"
    );
}

#[test]
fn cursor_accepts_claude_shaped_payloads() {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/stop.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let ev = parse_event(Agent::Cursor, HookEventKind::Stop, &text).unwrap();
    assert_eq!(ev.session_id, SID);
    assert_eq!(ev.cwd, "/home/u/kioku");
    assert!(!ev.stop_hook_active);
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/post_tool_use.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let ev = parse_event(Agent::Cursor, HookEventKind::PostToolUse, &text).unwrap();
    assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
    assert_eq!(ev.tool_response.as_ref().unwrap()["userModified"], false);
}

// ---- Gemini CLI -------------------------------------------------------------------------

#[test]
fn gemini_lifecycle_fixtures() {
    use HookEventKind::*;
    let a = Agent::GeminiCli;
    each(a, SessionStart, "session_start", |ev, docs| {
        if docs {
            assert_eq!(ev.session_id, GEMINI_SID);
            assert_eq!(ev.source.as_deref(), Some("startup"));
        }
    });
    each(a, UserPromptSubmit, "before_agent", |ev, docs| {
        assert_eq!(ev.native_event, "BeforeAgent");
        if docs {
            assert_eq!(ev.prompt.as_deref(), Some("STATE.md を読んで続きをやって"));
        }
    });
    each(a, Stop, "after_agent", |ev, _| {
        assert!(!ev.stop_hook_active)
    });
    each(a, Stop, "after_agent_active", |ev, _| {
        assert!(ev.stop_hook_active)
    });
    each(a, PreCompact, "pre_compress", |ev, docs| {
        assert_eq!(ev.native_event, "PreCompress");
        if docs {
            assert_eq!(ev.trigger.as_deref(), Some("auto"));
        }
    });
    each(a, SessionEnd, "session_end", |ev, docs| {
        if docs {
            assert_eq!(ev.reason.as_deref(), Some("exit"));
        }
    });
}

#[test]
fn gemini_tool_normalization() {
    use HookEventKind::PostToolUse;
    let a = Agent::GeminiCli;
    each(a, PostToolUse, "after_tool_replace", |ev, docs| {
        assert_eq!(ev.tool_name.as_deref(), Some("Edit"));
        assert_eq!(ev.native_tool.as_deref(), Some("replace"));
        assert!(ev.tool_response.as_ref().unwrap().get("is_error").is_none());
        if docs {
            assert_eq!(
                ev.tool_input,
                Some(json!({"file_path": format!("{ROOT}/src/lib.rs")}))
            );
        }
    });
    each(a, PostToolUse, "after_tool_shell_error", |ev, docs| {
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert_eq!(ev.native_tool.as_deref(), Some("run_shell_command"));
        assert_eq!(ev.tool_response.as_ref().unwrap()["is_error"], true);
        if docs {
            assert_eq!(ev.tool_input, Some(json!({"command": "cargo build"})));
        }
    });
    for (native, name) in [
        ("write_file", "Write"),
        ("read_file", "Read"),
        ("replace", "Edit"),
    ] {
        let raw = json!({"session_id": "g", "cwd": "/w", "tool_name": native,
            "tool_input": {"file_path": "/w/a.rs", "content": "x"}, "tool_response": {"llmContent": "ok", "error": null}});
        let ev = parse_value(a, PostToolUse, raw, &HookEnv::default()).unwrap();
        assert_eq!(ev.tool_name.as_deref(), Some(name));
        assert_eq!(ev.tool_input, Some(json!({"file_path": "/w/a.rs"})));
        assert!(
            ev.tool_response.unwrap().get("is_error").is_none(),
            "null error is no error"
        );
    }
}

#[test]
fn gemini_env_fallbacks() {
    let env = env_of(&[
        ("GEMINI_SESSION_ID", "from-env"),
        ("GEMINI_PROJECT_DIR", "/env/gemini"),
        ("CLAUDE_PROJECT_DIR", "/env/claude"),
    ]);
    let ev = parse_value(Agent::GeminiCli, HookEventKind::Stop, json!({}), &env).unwrap();
    assert_eq!(ev.session_id, "from-env");
    assert_eq!(ev.cwd, "/env/gemini");
    let env = env_of(&[("GEMINI_CWD", "/env/cwd")]);
    let ev = parse_value(
        Agent::GeminiCli,
        HookEventKind::Stop,
        json!({"session_id": "s"}),
        &env,
    )
    .unwrap();
    assert_eq!(ev.cwd, "/env/cwd");
    assert!(
        parse_value(
            Agent::GeminiCli,
            HookEventKind::Stop,
            json!({}),
            &env_of(&[])
        )
        .is_err()
    );
}

#[test]
fn claude_and_codex_env_cwd() {
    let env = env_of(&[
        ("CLAUDE_PROJECT_DIR", "/env/claude"),
        ("CURSOR_PROJECT_DIR", "/env/cursor"),
    ]);
    let ev = parse_event_env(
        Agent::ClaudeCode,
        HookEventKind::Stop,
        r#"{"session_id":"s"}"#,
        &env,
    )
    .unwrap();
    assert_eq!(ev.cwd, "/env/claude");
    // Codex has no project-dir variable: the handler falls back to the process cwd
    let ev = parse_event_env(
        Agent::Codex,
        HookEventKind::Stop,
        r#"{"session_id":"s"}"#,
        &env,
    )
    .unwrap();
    assert_eq!(ev.cwd, "");
}

#[test]
fn every_agent_rejects_garbage_without_panicking() {
    for agent in ALL_AGENTS {
        for kind in ALL_EVENTS {
            for text in [
                "",
                "null",
                "[1]",
                "{}",
                r#"{"session_id": 7}"#,
                r#"{"tool_input": "x"}"#,
            ] {
                assert!(
                    parse_event(agent, kind, text).is_err(),
                    "{agent:?} {kind:?} {text}"
                );
            }
            // a bare session id is enough for every event, with odd optional fields
            let ev = parse_event(
                agent,
                kind,
                r#"{"session_id":"s","conversation_id":"s","tool_name":7,"tool_input":"str","tool_output":null,
                   "workspace_roots":[1,""],"loop_count":-1,"stop_hook_active":"yes","edits":{}}"#,
            )
            .unwrap();
            assert_eq!(ev.session_id, "s");
            assert!(!ev.stop_hook_active);
        }
    }
}

#[test]
fn deadlines_per_agent() {
    use HookEventKind::*;
    assert_eq!(hook_deadline_ms(Agent::Codex, SessionEnd, 3000), 2500);
    for agent in ALL_AGENTS {
        for kind in ALL_EVENTS {
            let d = hook_deadline_ms(agent, kind, 3000);
            if !(agent == Agent::Codex && kind == SessionEnd) {
                assert_eq!(d, 3000, "{agent:?} {kind:?}: registrations are ≥ 5 s");
            }
            assert!(d + 500 <= registered_timeout_ms(agent, kind));
        }
    }
    // a larger timeout_ms is capped by the registration
    assert_eq!(hook_deadline_ms(Agent::Cursor, PostToolUse, 8000), 4500);
    assert_eq!(hook_deadline_ms(Agent::ClaudeCode, Stop, 8000), 8000);
    assert_eq!(hook_deadline_ms(Agent::Codex, SessionEnd, 0), 1);
}

#[test]
fn agent_labels_match_value_enum() {
    for agent in ALL_AGENTS {
        assert_eq!(Agent::from_str(agent.as_str(), false).unwrap(), agent);
    }
    assert_eq!(Agent::GeminiCli.as_str(), "gemini-cli");
}
