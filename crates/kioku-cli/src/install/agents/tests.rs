//! Installer tests (M2 §16.6) on a temp HOME: foreign content survives, second run is
//! byte-identical, a moved binary replaces the path, uninstall leaves only foreign content.

use std::collections::BTreeMap;

use clap::ValueEnum;

use super::*;
use crate::install::backup_path;
use crate::install::block::{MD_MARKERS, TOML_MARKERS};

const BIN: &str = "/opt/kioku/bin/kioku";
const MOVED: &str = "/home/me/.local/bin/kioku";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn ctx(home: &Path, cwd: &Path, bin: &str) -> InstallCtx {
    InstallCtx {
        home: home.to_path_buf(),
        codex_home: home.join(".codex"),
        cwd: cwd.to_path_buf(),
        bin: bin.to_string(),
        client: ClientConfig {
            server_url: "http://127.0.0.1:7391".into(),
            auth_token: Some(TOKEN.into()),
            ..ClientConfig::default()
        },
    }
}

fn opts(project: bool) -> InstallOptions {
    InstallOptions {
        project,
        ..InstallOptions::default()
    }
}

/// Every file under `dir` (relative path → bytes).
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&p).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn write_json(path: &Path, v: &Value) {
    write(path, &(serde_json::to_string_pretty(v).unwrap() + "\n"));
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// All hook commands of ours in a hook file, per event key.
fn our_commands(v: &Value) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Some(hooks) = v.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (key, list) in hooks {
        let mut cmds = Vec::new();
        for item in list.as_array().into_iter().flatten() {
            let handlers = match item.get("hooks").and_then(Value::as_array) {
                Some(h) => h.clone(),
                None => vec![item.clone()],
            };
            for h in handlers {
                if let Some(c) = h.get("command").and_then(Value::as_str)
                    && crate::install::is_kioku_command(c)
                {
                    cmds.push(c.to_string());
                }
            }
        }
        if !cmds.is_empty() {
            out.insert(key.clone(), cmds);
        }
    }
    out
}

/// Foreign content per agent, written at the paths the installer will edit.
fn seed_foreign(agent: Agent, c: &InstallCtx, project: bool) {
    let hooks = hooks_path(agent, c, project);
    match agent {
        Agent::ClaudeCode => {
            write_json(
                &hooks,
                &json!({"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}),
            );
            write_json(
                &mcp_path(agent, c),
                &json!({"numStartups": 3, "mcpServers": {"other": {"type": "stdio", "command": "x"}}}),
            );
        }
        Agent::Codex => {
            write_json(
                &hooks,
                &json!({"hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "notify-send codex", "timeout": 3}]}],
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]
                }}),
            );
            write(
                &mcp_path(agent, c),
                "# codex settings\nmodel = \"gpt-5-codex\"  # pinned\n\n[mcp_servers.github]\nurl = \"https://api.githubcopilot.com/mcp/\"\n",
            );
        }
        Agent::Cursor => {
            write_json(
                &hooks,
                &json!({"version": 1, "hooks": {
                    "stop": [{"command": "./audit.sh"}],
                    "beforeShellExecution": [{"command": "guard.sh", "matcher": "rm"}]
                }}),
            );
            write_json(
                &mcp_path(agent, c),
                &json!({"mcpServers": {"github": {"url": "https://api.githubcopilot.com/mcp/"}}}),
            );
        }
        Agent::GeminiCli => {
            write_json(
                &hooks,
                &json!({"theme": "GitHub", "hooksConfig": {"enabled": true}, "hooks": {
                    "AfterTool": [{"matcher": "write_file", "hooks": [{"name": "fmt", "type": "command", "command": "prettier -w"}]}]
                }}),
            );
            if project {
                write_json(
                    &mcp_path(agent, c),
                    &json!({"mcpServers": {"other": {"command": "x"}}}),
                );
            } else {
                // Same file as the hooks: add a foreign server to it.
                let mut v = read_json(&hooks);
                v["mcpServers"] = json!({"other": {"command": "x"}});
                write_json(&hooks, &v);
            }
        }
    }
    for path in instruction_files(agent, c, project).into_iter().take(1) {
        if path.extension().is_none_or(|e| e != "mdc") {
            write(&path, "# 自分のルール\n\n- テストを先に書く\n");
        }
    }
}

fn is_json(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
}

fn roundtrip(agent: Agent, project: bool) {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), proj.path(), BIN);
    let mut o = opts(project);
    if agent == Agent::ClaudeCode {
        o.instructions = Instructions::Force;
    }
    seed_foreign(agent, &c, project);
    let before_home = snapshot(home.path());
    let before_proj = snapshot(proj.path());

    // Install: our entry for every event, foreign content kept.
    let hooks = hooks_path(agent, &c, project);
    let orig_hooks = std::fs::read_to_string(&hooks).unwrap();
    let r = install_agent(agent, &c, &o).unwrap();
    assert!(r.changed, "{agent:?}: {:?}", r.lines);
    let v = read_json(&hooks);
    let ours = our_commands(&v);
    let expected_keys: Vec<String> = hook_specs(agent, BIN).into_iter().map(|s| s.key).collect();
    assert_eq!(ours.len(), expected_keys.len(), "{agent:?} {ours:?}");
    for cmds in ours.values() {
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds[0].starts_with(BIN),
            "absolute binary path: {}",
            cmds[0]
        );
    }
    let text = serde_json::to_string(&v).unwrap();
    for foreign in [
        "say done",
        "notify-send codex",
        "guard.sh",
        "./audit.sh",
        "prettier -w",
    ] {
        if orig_hooks.contains(foreign) {
            assert!(
                text.contains(foreign),
                "{agent:?} lost foreign hook {foreign}"
            );
        }
    }
    // MCP always at user level; the token never lands in the project.
    let mcp = mcp_path(agent, &c);
    let mcp_text = std::fs::read_to_string(&mcp).unwrap();
    assert!(
        mcp_text.contains(TOKEN),
        "{agent:?}: MCP entry carries the token"
    );
    assert!(mcp_text.contains("http://127.0.0.1:7391/mcp"));
    if project {
        for (p, bytes) in snapshot(proj.path()) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(TOKEN),
                "{agent:?}: token in project file {}",
                p.display()
            );
        }
    }
    let instr = instruction_files(agent, &c, project).into_iter().next();
    if let Some(p) = &instr {
        let t = std::fs::read_to_string(p).unwrap();
        assert!(
            t.contains("kioku_handoff_write"),
            "{agent:?} instructions: {t}"
        );
    }

    // Second run: byte-identical, reported unchanged.
    let after_home = snapshot(home.path());
    let after_proj = snapshot(proj.path());
    let r = install_agent(agent, &c, &o).unwrap();
    assert!(!r.changed, "{agent:?} second run: {:?}", r.lines);
    assert_eq!(snapshot(home.path()), after_home, "{agent:?}");
    assert_eq!(snapshot(proj.path()), after_proj, "{agent:?}");

    // Backups hold the originals.
    for (rel, bytes) in before_home.iter() {
        let bak = backup_path(&home.path().join(rel));
        if bak.exists() {
            assert_eq!(&std::fs::read(&bak).unwrap(), bytes);
        }
    }

    // Moved binary: replaced in place, one entry per event, backups untouched.
    let moved = ctx(home.path(), proj.path(), MOVED);
    let r = install_agent(agent, &moved, &o).unwrap();
    assert!(r.changed);
    assert!(
        r.lines
            .iter()
            .all(|l| !l.contains("backup of the original"))
    );
    let ours = our_commands(&read_json(&hooks));
    assert_eq!(ours.len(), expected_keys.len());
    for cmds in ours.values() {
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].starts_with(MOVED), "{}", cmds[0]);
    }
    for (rel, bytes) in before_home.iter() {
        let bak = backup_path(&home.path().join(rel));
        if bak.exists() {
            assert_eq!(
                &std::fs::read(&bak).unwrap(),
                bytes,
                "backup never overwritten"
            );
        }
    }

    // Uninstall: exactly the foreign content is left.
    let r = uninstall_agent(agent, &moved, project, false).unwrap();
    assert!(r.changed, "{agent:?}: {:?}", r.lines);
    for (tree, before) in [(home.path(), &before_home), (proj.path(), &before_proj)] {
        let now = snapshot(tree);
        for (rel, bytes) in before {
            let cur = now
                .get(rel)
                .unwrap_or_else(|| panic!("{agent:?}: {} vanished", rel.display()));
            if is_json(rel) {
                assert_eq!(
                    serde_json::from_slice::<Value>(cur).unwrap(),
                    serde_json::from_slice::<Value>(bytes).unwrap(),
                    "{agent:?}: {}",
                    rel.display()
                );
            } else {
                assert_eq!(
                    String::from_utf8_lossy(cur),
                    String::from_utf8_lossy(bytes),
                    "{agent:?}: {}",
                    rel.display()
                );
            }
        }
        for (rel, bytes) in &now {
            if before.contains_key(rel) || rel.to_string_lossy().ends_with(".kioku-bak") {
                continue;
            }
            let t = String::from_utf8_lossy(bytes);
            assert!(
                !t.contains("kioku"),
                "{agent:?}: leftover {}: {t}",
                rel.display()
            );
        }
    }
    let r = uninstall_agent(agent, &moved, project, false).unwrap();
    assert!(!r.changed, "{agent:?} second uninstall: {:?}", r.lines);
}

#[test]
fn claude_code_user_and_project() {
    roundtrip(Agent::ClaudeCode, false);
    roundtrip(Agent::ClaudeCode, true);
}

#[test]
fn codex_user_and_project() {
    roundtrip(Agent::Codex, false);
    roundtrip(Agent::Codex, true);
}

#[test]
fn cursor_user_and_project() {
    roundtrip(Agent::Cursor, false);
    roundtrip(Agent::Cursor, true);
}

#[test]
fn gemini_user_and_project() {
    roundtrip(Agent::GeminiCli, false);
    roundtrip(Agent::GeminiCli, true);
}

#[test]
fn exact_hook_shapes_on_an_empty_home() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    for agent in [Agent::Codex, Agent::Cursor, Agent::GeminiCli] {
        install_agent(agent, &c, &opts(false)).unwrap();
    }
    let cmd = |a: &str, e: &str| format!("{BIN} hook {e} --agent {a}");
    let codex = |e: &str| cmd("codex", e);
    assert_eq!(
        read_json(&home.path().join(".codex/hooks.json")),
        json!({"hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": codex("session-start"),
                "timeout": 10, "statusMessage": "kioku: loading handoff", "additionalContextLimit": 0}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": codex("user-prompt-submit"), "timeout": 5}]}],
            "PostToolUse": [{"matcher": "^(Bash|apply_patch)$",
                "hooks": [{"type": "command", "command": codex("post-tool-use"), "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": codex("stop"), "timeout": 10}]}],
            "PreCompact": [{"hooks": [{"type": "command", "command": codex("pre-compact"), "timeout": 5}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": codex("session-end"), "timeout": 3}]}]
        }})
    );
    let cur = |e: &str| cmd("cursor", e);
    let cursor = read_json(&home.path().join(".cursor/hooks.json"));
    assert_eq!(
        cursor,
        json!({"version": 1, "hooks": {
            "sessionStart": [{"command": cur("session-start"), "timeout": 10}],
            "beforeSubmitPrompt": [{"command": cur("user-prompt-submit"), "timeout": 5}],
            "postToolUse": [{"command": cur("post-tool-use"), "matcher": "Shell|Read", "timeout": 5}],
            "postToolUseFailure": [{"command": cur("post-tool-use"), "matcher": "Shell", "timeout": 5}],
            "afterFileEdit": [{"command": cur("post-tool-use"), "timeout": 5}],
            "preCompact": [{"command": cur("pre-compact"), "timeout": 5}],
            "stop": [{"command": cur("stop"), "timeout": 10}],
            "sessionEnd": [{"command": cur("session-end"), "timeout": 5}]
        }})
    );
    assert_eq!(
        cursor.as_object().unwrap().keys().next().unwrap(),
        "version"
    );
    assert_eq!(
        read_json(&home.path().join(".cursor/mcp.json")),
        json!({"mcpServers": {"kioku": {"url": "http://127.0.0.1:7391/mcp",
            "headers": {"Authorization": format!("Bearer {TOKEN}")}}}})
    );
    let gem = |e: &str| cmd("gemini-cli", e);
    assert_eq!(
        read_json(&home.path().join(".gemini/settings.json")),
        json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"name": "kioku-session-start", "type": "command", "command": gem("session-start"), "timeout": 10000}]}],
                "BeforeAgent": [{"hooks": [{"name": "kioku-user-prompt", "type": "command", "command": gem("user-prompt-submit"), "timeout": 5000}]}],
                "AfterTool": [{"matcher": "run_shell_command|write_file|replace|read_file",
                    "hooks": [{"name": "kioku-post-tool", "type": "command", "command": gem("post-tool-use"), "timeout": 5000}]}],
                "PreCompress": [{"hooks": [{"name": "kioku-pre-compact", "type": "command", "command": gem("pre-compact"), "timeout": 5000}]}],
                "AfterAgent": [{"hooks": [{"name": "kioku-stop", "type": "command", "command": gem("stop"), "timeout": 10000}]}],
                "SessionEnd": [{"hooks": [{"name": "kioku-session-end", "type": "command", "command": gem("session-end"), "timeout": 5000}]}]
            },
            "mcpServers": {"kioku": {"httpUrl": "http://127.0.0.1:7391/mcp",
                "headers": {"Authorization": format!("Bearer {TOKEN}")}, "timeout": 10000}}
        })
    );
    let toml_text = std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
    assert_eq!(
        toml_text,
        format!(
            "{}\n[mcp_servers.kioku]\nurl = \"http://127.0.0.1:7391/mcp\"\nhttp_headers = {{ Authorization = \"Bearer {TOKEN}\" }}\n{}\n",
            TOML_MARKERS.begin, TOML_MARKERS.end
        )
    );
    // Global instruction snippets (default for Codex and Gemini; none for user-level Cursor).
    for p in [".codex/AGENTS.md", ".gemini/GEMINI.md"] {
        let t = std::fs::read_to_string(home.path().join(p)).unwrap();
        assert!(t.starts_with(MD_MARKERS.begin), "{p}");
        assert!(t.contains("`kioku project id` の出力"), "{p}: {t}");
    }
    assert!(!home.path().join(".cursor/rules").exists());
    #[cfg(unix)]
    {
        // Token-bearing new files 0600, hook-only new files 0644.
        for p in [
            ".codex/config.toml",
            ".cursor/mcp.json",
            ".gemini/settings.json",
        ] {
            assert_eq!(mode(&home.path().join(p)), 0o600, "{p}");
        }
        for p in [
            ".codex/hooks.json",
            ".cursor/hooks.json",
            ".codex/AGENTS.md",
        ] {
            assert_eq!(mode(&home.path().join(p)), 0o644, "{p}");
        }
    }
}

#[test]
fn registered_timeouts_match_the_hook_deadlines() {
    for agent in ALL_AGENTS {
        for spec in hook_specs(agent, BIN) {
            let handlers: Vec<Value> = match spec.entry.get("hooks") {
                Some(Value::Array(h)) => h.clone(),
                _ => vec![spec.entry.clone()],
            };
            for h in handlers {
                let cmd = h["command"].as_str().unwrap();
                let ev = cmd
                    .split(" hook ")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap();
                let event = HookEventKind::from_str(ev, false).unwrap();
                let want = registered_timeout_ms(agent, event);
                let got_ms = match (agent, h.get("timeout").and_then(Value::as_u64)) {
                    (Agent::GeminiCli, Some(ms)) => ms,
                    (_, Some(s)) => s * 1000,
                    // Claude Code without an explicit timeout: its 60 s default.
                    (Agent::ClaudeCode, None) => 60_000,
                    (_, None) => panic!("{agent:?} {ev}: no timeout"),
                };
                assert_eq!(got_ms, want, "{agent:?} {} {ev}", spec.key);
            }
        }
    }
}

#[test]
fn project_instructions_embed_the_project_id_and_cursor_rule_is_exact() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    write(
        &proj.path().join(".kioku.toml"),
        "project = \"chord-life-ace9dc4a\"\n",
    );
    let c = ctx(home.path(), proj.path(), BIN);
    install_agent(Agent::Cursor, &c, &opts(true)).unwrap();
    let mdc = std::fs::read_to_string(proj.path().join(".cursor/rules/kioku.mdc")).unwrap();
    assert_eq!(
        mdc,
        block::mdc_content(&block::instructions_body(
            kioku_core::Lang::Ja,
            Some("chord-life-ace9dc4a")
        ))
    );
    assert!(mdc.contains("`chord-life-ace9dc4a`"));
    assert!(
        !proj.path().join("AGENTS.md").exists(),
        "Cursor never writes AGENTS.md"
    );
    // A foreign kioku.mdc is not overwritten.
    write(
        &proj.path().join(".cursor/rules/kioku.mdc"),
        "---\ndescription: mine\n---\n",
    );
    let r = install_agent(Agent::Cursor, &c, &opts(true)).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("not written by kioku")));
    uninstall_agent(Agent::Cursor, &c, true, false).unwrap();
    assert!(proj.path().join(".cursor/rules/kioku.mdc").exists());

    install_agent(Agent::Codex, &c, &opts(true)).unwrap();
    let agents = std::fs::read_to_string(proj.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("`chord-life-ace9dc4a`"));
    assert!(proj.path().join(".codex/hooks.json").exists());
    // MCP stays user-level.
    assert!(home.path().join(".codex/config.toml").exists());
    assert!(!proj.path().join(".codex/config.toml").exists());
}

#[test]
fn codex_override_md_is_preferred_when_non_empty() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    write(
        &home.path().join(".codex/AGENTS.override.md"),
        "# override\n",
    );
    write(&home.path().join(".codex/AGENTS.md"), "# base\n");
    install_agent(Agent::Codex, &c, &opts(false)).unwrap();
    assert!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.override.md"))
            .unwrap()
            .contains(MD_MARKERS.end)
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.md")).unwrap(),
        "# base\n"
    );
    uninstall_agent(Agent::Codex, &c, false, false).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.override.md")).unwrap(),
        "# override\n"
    );

    // An empty override is ignored by Codex, so the block goes to AGENTS.md.
    write(&home.path().join(".codex/AGENTS.override.md"), "  \n");
    install_agent(Agent::Codex, &c, &opts(false)).unwrap();
    assert!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.md"))
            .unwrap()
            .contains(MD_MARKERS.end)
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/AGENTS.override.md")).unwrap(),
        "  \n"
    );
}

#[test]
fn gemini_context_file_name_is_honoured_and_dollar_paths_rejected() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    let settings = home.path().join(".gemini/settings.json");
    write_json(
        &settings,
        &json!({"context": {"fileName": ["AGENT.md", "CONTEXT.md"]}}),
    );
    install_agent(Agent::GeminiCli, &c, &opts(false)).unwrap();
    assert!(home.path().join(".gemini/AGENT.md").exists());
    assert!(!home.path().join(".gemini/GEMINI.md").exists());
    uninstall_agent(Agent::GeminiCli, &c, false, false).unwrap();
    assert!(
        !home.path().join(".gemini/AGENT.md").exists(),
        "kioku created it"
    );

    write_json(
        &settings,
        &json!({"context": {"fileName": ["CONTEXT.md", "GEMINI.md"]}}),
    );
    install_agent(Agent::GeminiCli, &c, &opts(false)).unwrap();
    assert!(home.path().join(".gemini/GEMINI.md").exists());

    let dollar = ctx(home.path(), home.path(), "/opt/$HOME/kioku");
    let before = snapshot(home.path());
    assert!(install_agent(Agent::GeminiCli, &dollar, &opts(false)).is_err());
    assert_eq!(snapshot(home.path()), before);
}

#[test]
fn instructions_flags() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    let skip = InstallOptions {
        instructions: Instructions::Skip,
        ..InstallOptions::default()
    };
    install_agent(Agent::Codex, &c, &skip).unwrap();
    assert!(!home.path().join(".codex/AGENTS.md").exists());
    install_agent(Agent::ClaudeCode, &c, &opts(false)).unwrap();
    assert!(
        !home.path().join(".claude/CLAUDE.md").exists(),
        "off by default for Claude"
    );
    let force = InstallOptions {
        instructions: Instructions::Force,
        ..InstallOptions::default()
    };
    install_agent(Agent::ClaudeCode, &c, &force).unwrap();
    assert!(home.path().join(".claude/CLAUDE.md").exists());
}

#[test]
fn invalid_json_is_untouched_and_the_snippet_reported() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    for agent in [Agent::Codex, Agent::Cursor, Agent::GeminiCli] {
        let path = hooks_path(agent, &c, false);
        write(&path, "{ \"hooks\": // comment\n}");
        let err = install_agent(agent, &c, &opts(false)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid JSON") && msg.contains("hook session-start"),
            "{msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"hooks\": // comment\n}"
        );
        assert!(!backup_path(&path).exists());
    }
    // An unparseable MCP file is a reported warning, not a failure.
    std::fs::remove_file(hooks_path(Agent::Cursor, &c, false)).unwrap();
    write(&home.path().join(".cursor/mcp.json"), "[");
    let r = install_agent(Agent::Cursor, &c, &opts(false)).unwrap();
    assert!(r.lines.iter().any(|l| l.contains("\"mcpServers\"")));
    assert_eq!(
        std::fs::read_to_string(home.path().join(".cursor/mcp.json")).unwrap(),
        "["
    );
}

#[test]
fn dry_run_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    for d in [".claude", ".codex", ".cursor", ".gemini"] {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    let c = ctx(home.path(), home.path(), BIN);
    seed_foreign(Agent::Codex, &c, false);
    let before = snapshot(home.path());
    let o = InstallOptions {
        dry_run: true,
        ..InstallOptions::default()
    };
    for (agent, status) in install_all(&c, &o, &[]) {
        assert!(
            matches!(status, AllStatus::Changed(_)),
            "{agent:?}: {status:?}"
        );
    }
    assert_eq!(snapshot(home.path()), before);
    install_all(&c, &opts(false), &[]);
    let installed = snapshot(home.path());
    for (_, status) in uninstall_all(&c, false, true) {
        assert!(matches!(status, AllStatus::Changed(_)));
    }
    assert_eq!(snapshot(home.path()), installed);
}

#[test]
fn install_all_detects_agents_by_directory() {
    // Only ~/.codex and ~/.gemini exist → exactly those are installed.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    let results = install_all(&c, &opts(false), &[]);
    let summary: Vec<(Agent, &str)> = results
        .iter()
        .map(|(a, s)| {
            (
                *a,
                match s {
                    AllStatus::Changed(_) => "installed",
                    AllStatus::Unchanged(_) => "unchanged",
                    AllStatus::NotDetected(_) => "skipped",
                    AllStatus::Error(_) => "error",
                },
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (Agent::ClaudeCode, "skipped"),
            (Agent::Codex, "installed"),
            (Agent::Cursor, "skipped"),
            (Agent::GeminiCli, "installed"),
        ]
    );
    assert!(!home.path().join(".claude").exists());
    assert!(!home.path().join(".claude.json").exists());
    assert!(!home.path().join(".cursor").exists());
    assert!(home.path().join(".codex/hooks.json").exists());
    assert!(home.path().join(".gemini/settings.json").exists());
    // Second run: unchanged.
    assert!(
        install_all(&c, &opts(false), &[])
            .iter()
            .all(|(_, s)| !matches!(s, AllStatus::Changed(_) | AllStatus::Error(_)))
    );

    // `--agents` restricts the set.
    let r = install_all(&c, &opts(false), &[Agent::Codex]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, Agent::Codex);

    // No agent dirs: everything skipped, nothing written.
    let empty = tempfile::tempdir().unwrap();
    let ce = ctx(empty.path(), empty.path(), BIN);
    assert!(
        install_all(&ce, &opts(false), &[])
            .iter()
            .all(|(_, s)| matches!(s, AllStatus::NotDetected(_)))
    );
    assert!(snapshot(empty.path()).is_empty());

    // All four present.
    let full = tempfile::tempdir().unwrap();
    for d in [".claude", ".codex", ".cursor", ".gemini"] {
        std::fs::create_dir_all(full.path().join(d)).unwrap();
    }
    let cf = ctx(full.path(), full.path(), BIN);
    assert!(
        install_all(&cf, &opts(false), &[])
            .iter()
            .all(|(_, s)| matches!(s, AllStatus::Changed(_)))
    );
    // uninstall all removes everything kioku wrote (detected or not); files kioku created
    // go away entirely and no backup of them is written.
    for (a, s) in uninstall_all(&cf, false, false) {
        assert!(matches!(s, AllStatus::Changed(_)), "{a:?}: {s:?}");
    }
    assert!(
        snapshot(full.path()).is_empty(),
        "{:?}",
        snapshot(full.path()).keys().collect::<Vec<_>>()
    );
}

#[test]
fn install_all_continues_past_a_failing_agent() {
    let home = tempfile::tempdir().unwrap();
    let c = ctx(home.path(), home.path(), BIN);
    write(&home.path().join(".codex/hooks.json"), "not json");
    std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
    let results = install_all(&c, &opts(false), &[]);
    let codex = &results.iter().find(|(a, _)| *a == Agent::Codex).unwrap().1;
    assert!(matches!(codex, AllStatus::Error(e) if e.contains("not valid JSON")));
    let gemini = &results
        .iter()
        .find(|(a, _)| *a == Agent::GeminiCli)
        .unwrap()
        .1;
    assert!(matches!(gemini, AllStatus::Changed(_)));
    assert_eq!(
        std::fs::read_to_string(home.path().join(".codex/hooks.json")).unwrap(),
        "not json"
    );
}

#[test]
fn notes_and_binary_warning() {
    let r = AgentReport {
        agent: Agent::Codex,
        changed: true,
        lines: Vec::new(),
    };
    let notes = post_install_notes(Agent::Codex, &opts(true), &r).join("\n");
    assert!(notes.contains("/hooks") && notes.contains("trusted") && notes.contains("commit"));
    assert!(post_install_notes(Agent::ClaudeCode, &opts(false), &r).is_empty());
    assert!(unstable_binary_warning("/home/me/src/kioku/target/debug/kioku").is_some());
    assert!(unstable_binary_warning("/tmp/x/kioku").is_some());
    assert!(unstable_binary_warning("/home/me/.local/bin/kioku").is_none());
}
