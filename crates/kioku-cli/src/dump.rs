//! `KIOKU_HOOK_DUMP` payload capture (M2 §3.8) and `kioku hook-dump extract`.
//!
//! When enabled (env `KIOKU_HOOK_DUMP=1` or `[client] hook_dump = true`), every hook
//! invocation appends one JSON line — raw stdin, filtered env, argv and the outcome — to
//! `<log dir>/hook-dump.jsonl` (0600, rotated at 5 MiB). Dumping never changes the outcome
//! and never fails the hook. `extract` turns the newest matching line into a test fixture.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use kioku_core::Config;
use kioku_core::util::now_ts;
use serde_json::{Value, json};

use crate::event::{Agent, HookEnv, HookEventKind};
use crate::hook::{HookOutcome, append_line, hook_log_path, run_hook_with_env};

/// File name of the dump, next to `hook.log`.
pub const HOOK_DUMP_FILE: &str = "hook-dump.jsonl";
/// The dump is rotated to `hook-dump.jsonl.1` before it would exceed this.
pub const HOOK_DUMP_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Environment variable prefixes copied into a dump line (`KIOKU_AUTH_TOKEN` excluded).
pub const DUMP_ENV_PREFIXES: [&str; 5] = ["CURSOR_", "GEMINI_", "CODEX_", "CLAUDE_", "KIOKU_"];

/// True when `KIOKU_HOOK_DUMP` is set to a true value or `[client] hook_dump = true`.
pub fn dump_enabled(cfg: &Config, env: &HookEnv) -> bool {
    let from_env = env
        .var("KIOKU_HOOK_DUMP")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    from_env || cfg.client.hook_dump
}

/// `<log dir>/hook-dump.jsonl` (log dir as for `hook.log`).
pub fn dump_path(cfg: &Config) -> Option<PathBuf> {
    hook_log_path(cfg).and_then(|p| p.parent().map(|d| d.join(HOOK_DUMP_FILE)))
}

/// The agent-related environment variables recorded in a dump line; values of
/// secret-looking names (`TOKEN`, `SECRET`, `PASSWORD`, `API_KEY`, `CREDENTIAL`, `AUTH`)
/// become `[REDACTED]` — agents export such variables (e.g. `CLAUDE_CODE_OAUTH_TOKEN`).
pub fn filtered_env(env: &HookEnv) -> BTreeMap<String, String> {
    env.vars
        .iter()
        .filter(|(k, _)| {
            DUMP_ENV_PREFIXES.iter().any(|p| k.starts_with(p)) && k.as_str() != "KIOKU_AUTH_TOKEN"
        })
        .map(|(k, v)| {
            let upper = k.to_ascii_uppercase();
            let secret = [
                "TOKEN",
                "SECRET",
                "PASSW",
                "API_KEY",
                "APIKEY",
                "CREDENTIAL",
                "AUTH",
            ]
            .iter()
            .any(|w| upper.contains(w));
            let v = if secret {
                "[REDACTED]".to_string()
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect()
}

/// One dump line (M2 §3.8 shape).
pub fn dump_record(
    agent: Agent,
    event: HookEventKind,
    argv: &[String],
    env: &HookEnv,
    stdin: &str,
    outcome: &HookOutcome,
) -> Value {
    json!({
        "ts": now_ts(),
        "agent": agent.as_str(),
        "event": event.cli_name(),
        "argv": argv,
        "cwd": env.cwd.as_ref().map(|c| c.display().to_string()).unwrap_or_default(),
        "env": filtered_env(env),
        "stdin": stdin,
        "outcome": {
            "exit_code": outcome.exit_code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
        },
    })
}

/// Appends a record to the dump file (created 0600, rotated at [`HOOK_DUMP_MAX_BYTES`]).
pub fn write_dump(path: &Path, record: &Value) -> std::io::Result<()> {
    append_line(path, &format!("{record}\n"), HOOK_DUMP_MAX_BYTES, true)
}

/// Writes the dump line for one invocation when dumping is enabled; errors are ignored.
pub fn dump_invocation(
    cfg: &Config,
    env: &HookEnv,
    agent: Agent,
    event: HookEventKind,
    argv: &[String],
    stdin: &str,
    outcome: &HookOutcome,
) {
    if !dump_enabled(cfg, env) {
        return;
    }
    if let Some(path) = dump_path(cfg) {
        let _ = write_dump(&path, &dump_record(agent, event, argv, env, stdin, outcome));
    }
}

/// A whole `kioku hook` invocation after config and stdin were read: run the hook, then
/// dump it (when enabled). The outcome is exactly [`run_hook_with_env`]'s.
pub fn run_hook_invocation(
    event: HookEventKind,
    agent: Agent,
    argv: &[String],
    stdin: &str,
    cfg: &Config,
    env: &HookEnv,
) -> HookOutcome {
    let outcome = run_hook_with_env(event, agent, stdin, cfg, env);
    dump_invocation(cfg, env, agent, event, argv, stdin, &outcome);
    outcome
}

/// snake_case fixture name for an event given as `post-tool-use`, `afterFileEdit`,
/// `BeforeAgent` or `after_file_edit`.
pub fn fixture_name(event: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in event.chars() {
        if c == '-' || c == '_' || c == ' ' {
            out.push('_');
            prev_lower = false;
        } else if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        }
    }
    out
}

/// Writes the newest dumped stdin of `agent` whose neutral event or native
/// `hook_event_name` matches `event` to `<out>/<agent>/<event>.captured.json`
/// (pretty-printed); returns the written path. Searches the current dump, then `.1`.
pub fn extract(dump: &Path, agent: Agent, event: &str, out: &Path) -> anyhow::Result<PathBuf> {
    let want = fixture_name(event);
    let mut rotated = dump.as_os_str().to_os_string();
    rotated.push(".1");
    let mut found: Option<Value> = None;
    for file in [dump.to_path_buf(), PathBuf::from(rotated)] {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines().rev() {
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if rec.get("agent").and_then(Value::as_str) != Some(agent.as_str()) {
                continue;
            }
            let stdin = rec.get("stdin").and_then(Value::as_str).unwrap_or("");
            let payload: Option<Value> = serde_json::from_str(stdin.trim()).ok();
            let native = payload
                .as_ref()
                .and_then(|p| p.get("hook_event_name"))
                .and_then(Value::as_str);
            let neutral = rec.get("event").and_then(Value::as_str);
            let matches = [neutral, native]
                .into_iter()
                .flatten()
                .any(|e| fixture_name(e) == want);
            if matches {
                found = Some(payload.with_context(|| {
                    format!(
                        "the newest {} {event} dump line has non-JSON stdin",
                        agent.as_str()
                    )
                })?);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let payload = found.with_context(|| {
        format!(
            "no {} {event} invocation in {} (enable KIOKU_HOOK_DUMP=1 and run the agent)",
            agent.as_str(),
            dump.display()
        )
    })?;
    let dir = out.join(agent.as_str());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{want}.captured.json"));
    let text = serde_json::to_string_pretty(&payload)? + "\n";
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HookEnv {
        let vars = [
            ("CURSOR_PROJECT_DIR", "/Users/me/src/app"),
            ("CURSOR_VERSION", "3.1.0"),
            ("GEMINI_SESSION_ID", "g"),
            ("CODEX_HOME", "/Users/me/.codex"),
            ("CLAUDE_PROJECT_DIR", "/Users/me/src/app"),
            ("KIOKU_HOOK_DUMP", "1"),
            ("KIOKU_AUTH_TOKEN", "secret-token-value"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-secret-value"),
            ("HOME", "/Users/me"),
            ("AWS_SECRET_ACCESS_KEY", "nope"),
            ("PATH", "/usr/bin"),
        ];
        HookEnv {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: None,
            cwd: Some(PathBuf::from("/Users/me/.cursor")),
        }
    }

    fn cfg_unreachable(dir: &Path) -> Config {
        let mut cfg = Config::for_data_dir(dir);
        cfg.client.server_url = "http://127.0.0.1:9".into();
        cfg.client.timeout_ms = 300;
        cfg
    }

    fn argv(agent: &str, event: &str) -> Vec<String> {
        ["hook", event, "--agent", agent]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn enabled_by_env_or_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_data_dir(dir.path());
        assert!(!dump_enabled(&cfg, &HookEnv::default()));
        assert!(dump_enabled(&cfg, &env()));
        let mut off = env();
        off.vars.insert("KIOKU_HOOK_DUMP".into(), "0".into());
        assert!(!dump_enabled(&cfg, &off));
        cfg.client.hook_dump = true;
        assert!(dump_enabled(&cfg, &HookEnv::default()));
        assert_eq!(
            dump_path(&cfg),
            Some(dir.path().join("logs").join(HOOK_DUMP_FILE))
        );
    }

    #[test]
    fn one_line_per_invocation_with_outcome_and_filtered_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(dir.path());
        let stdin = r#"{"conversation_id":"c1","workspace_roots":["/Users/me/src/app"],"hook_event_name":"stop","status":"completed","loop_count":0}"#;
        let out = run_hook_invocation(
            HookEventKind::Stop,
            Agent::Cursor,
            &argv("cursor", "stop"),
            stdin,
            &cfg,
            &env(),
        );
        assert_eq!(out.stdout, "{}\n", "fail-open Cursor reply");
        let out2 = run_hook_invocation(
            HookEventKind::SessionStart,
            Agent::GeminiCli,
            &argv("gemini-cli", "session-start"),
            "not json at all",
            &cfg,
            &env(),
        );
        let path = dir.path().join("logs").join(HOOK_DUMP_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let rec = &lines[0];
        let keys: Vec<&str> = rec
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "ts", "agent", "event", "argv", "cwd", "env", "stdin", "outcome"
            ]
        );
        assert_eq!(rec["agent"], "cursor");
        assert_eq!(rec["event"], "stop");
        assert_eq!(rec["argv"], json!(["hook", "stop", "--agent", "cursor"]));
        assert_eq!(rec["cwd"], "/Users/me/.cursor");
        assert_eq!(rec["stdin"], stdin, "raw, verbatim");
        assert_eq!(
            rec["outcome"],
            json!({"exit_code": 0, "stdout": "{}\n", "stderr": ""})
        );
        let env_keys: Vec<&str> = rec["env"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            env_keys,
            [
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_PROJECT_DIR",
                "CODEX_HOME",
                "CURSOR_PROJECT_DIR",
                "CURSOR_VERSION",
                "GEMINI_SESSION_ID",
                "KIOKU_HOOK_DUMP"
            ]
        );
        assert!(!text.contains("secret-token-value"));
        assert!(!text.contains("oauth-secret-value"));
        assert_eq!(rec["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "[REDACTED]");
        assert_eq!(rec["env"]["CURSOR_VERSION"], "3.1.0");
        assert_eq!(lines[1]["stdin"], "not json at all");
        assert_eq!(lines[1]["outcome"]["stdout"], out2.stdout);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn dump_failure_never_changes_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(dir.path());
        let stdin = r#"{"session_id":"g1","cwd":"/w","hook_event_name":"AfterAgent","stop_hook_active":false}"#;
        let baseline = run_hook_with_env(
            HookEventKind::Stop,
            Agent::GeminiCli,
            stdin,
            &cfg,
            &HookEnv::default(),
        );
        // logs/hook-dump.jsonl is a directory: every write fails
        std::fs::create_dir_all(dir.path().join("logs").join(HOOK_DUMP_FILE)).unwrap();
        let out = run_hook_invocation(
            HookEventKind::Stop,
            Agent::GeminiCli,
            &argv("gemini-cli", "stop"),
            stdin,
            &cfg,
            &env(),
        );
        assert_eq!(out, baseline);
        assert_eq!(out.stdout, "{}\n");
    }

    #[test]
    fn rotation_at_five_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOOK_DUMP_FILE);
        std::fs::write(&path, "x".repeat(HOOK_DUMP_MAX_BYTES as usize - 20)).unwrap();
        write_dump(&path, &json!({"a": 1})).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            HOOK_DUMP_MAX_BYTES - 20 + 8
        );
        write_dump(&path, &json!({"b": "this line crosses the cap"})).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"b\":\"this line crosses the cap\"}\n"
        );
        assert!(dir.path().join(format!("{HOOK_DUMP_FILE}.1")).exists());
    }

    #[test]
    fn fixture_names() {
        for (input, want) in [
            ("post-tool-use", "post_tool_use"),
            ("afterFileEdit", "after_file_edit"),
            ("BeforeAgent", "before_agent"),
            ("PreCompress", "pre_compress"),
            ("session_end", "session_end"),
            ("postToolUseFailure", "post_tool_use_failure"),
        ] {
            assert_eq!(fixture_name(input), want, "{input}");
        }
    }

    #[test]
    fn extract_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join(HOOK_DUMP_FILE);
        let e = env();
        let ok = HookOutcome::ok();
        let older =
            r#"{"conversation_id":"old","hook_event_name":"afterFileEdit","file_path":"/a"}"#;
        let newer = r#"{"conversation_id":"new","hook_event_name":"afterFileEdit","file_path":"/b","edits":[]}"#;
        let shell =
            r#"{"conversation_id":"new","hook_event_name":"postToolUse","tool_name":"Shell"}"#;
        let codex = r#"{"session_id":"x","hook_event_name":"PostToolUse"}"#;
        for (agent, stdin) in [
            (Agent::Cursor, older),
            (Agent::Cursor, newer),
            (Agent::Cursor, shell),
            (Agent::Codex, codex),
        ] {
            let rec = dump_record(
                agent,
                HookEventKind::PostToolUse,
                &argv(agent.as_str(), "post-tool-use"),
                &e,
                stdin,
                &ok,
            );
            write_dump(&dump, &rec).unwrap();
        }
        let out = dir.path().join("fixtures");
        // by native event name: newest afterFileEdit, not the newer Shell line
        let path = extract(&dump, Agent::Cursor, "afterFileEdit", &out).unwrap();
        assert_eq!(
            path,
            out.join("cursor").join("after_file_edit.captured.json")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\n  \"conversation_id\": \"new\""), "{text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            serde_json::from_str::<Value>(newer).unwrap()
        );
        // by neutral event name: the newest line of that agent
        let path = extract(&dump, Agent::Cursor, "post-tool-use", &out).unwrap();
        assert!(path.ends_with("cursor/post_tool_use.captured.json"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("Shell"));
        let path = extract(&dump, Agent::Codex, "post-tool-use", &out).unwrap();
        assert!(std::fs::read_to_string(path).unwrap().contains("\"x\""));
        // nothing for that agent / event
        assert!(extract(&dump, Agent::GeminiCli, "stop", &out).is_err());
        assert!(extract(&dir.path().join("missing"), Agent::Cursor, "stop", &out).is_err());
    }
}
