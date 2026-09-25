//! `kioku hook <event>` handlers (spec §8): thin, fail-open HTTP clients of the server.
//!
//! [`run_hook`] is the whole hook as a pure-ish function (stdin text + config in,
//! stdout/stderr/exit code out) so tests can drive it without spawning the binary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use kioku_core::sanitize::sanitize_payload;
use kioku_core::strings::{fill, strings};
use kioku_core::util::{home_dir, now_ts, one_line};
use kioku_core::{
    Config, DataDir, NewObservation, ObservationKind, SessionInfo, SessionStartRequest,
    SessionStartResponse, identify,
};
use serde_json::{Map, Value, json};

use crate::client::{ApiClient, http_status};
use crate::context::{StartContext, render_session_start};
use crate::event::{Agent, HookEvent, HookEventKind, parse_event};

/// Minimum tool uses in a session before the Stop hook nudges for a handoff.
pub const NUDGE_MIN_TOOL_USES: u32 = 3;
/// Exit code that makes Claude Code feed stderr back to the model (Stop nudge).
pub const NUDGE_EXIT_CODE: i32 = 2;

/// What a hook invocation prints and how it exits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookOutcome {
    /// Text for stdout (SessionStart: context injected into the agent).
    pub stdout: String,
    /// Text for stderr (Stop nudge).
    pub stderr: String,
    /// Process exit code (0, or 2 for the Stop nudge).
    pub exit_code: i32,
}

impl HookOutcome {
    /// Silent success.
    pub fn ok() -> HookOutcome {
        HookOutcome::default()
    }
}

/// What the Stop hook should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopDecision {
    /// Exit 2 with the nudge on stderr so the agent writes a handoff first.
    Nudge,
    /// Finalize the session (digest, session page, STATE.md, rules handoff).
    Finalize,
}

/// Stop-hook rule (spec §7.1): nudge only when no agent handoff exists, the session used
/// at least [`NUDGE_MIN_TOOL_USES`] tools, the agent is not already continuing because of
/// a Stop hook, and the nudge is enabled.
pub fn stop_decision(
    info: &SessionInfo,
    stop_hook_active: bool,
    nudge_enabled: bool,
) -> StopDecision {
    if nudge_enabled
        && !stop_hook_active
        && !info.has_agent_handoff
        && info.counts.tool_uses >= NUDGE_MIN_TOOL_USES
    {
        StopDecision::Nudge
    } else {
        StopDecision::Finalize
    }
}

/// Runs one hook: parses `stdin_json`, talks to the server within `timeout_ms`, and returns
/// what to print. Never fails: errors are appended to `logs/hook.log` and yield a silent exit 0.
pub fn run_hook(event: HookEventKind, agent: Agent, stdin_json: &str, cfg: &Config) -> HookOutcome {
    let mut session = String::from("-");
    let result = parse_event(agent, event, stdin_json).and_then(|ev| {
        session = ev.session_id.clone();
        handle(&ev, cfg)
    });
    match result {
        Ok(outcome) => outcome,
        Err(err) => {
            log_failure(cfg, event, &session, &err);
            HookOutcome::ok()
        }
    }
}

fn handle(ev: &HookEvent, cfg: &Config) -> anyhow::Result<HookOutcome> {
    let client = ApiClient::new(
        &cfg.client,
        Duration::from_millis(cfg.client.timeout_ms.max(1)),
    )?;
    match ev.event {
        HookEventKind::SessionStart => session_start(ev, cfg, &client),
        HookEventKind::UserPromptSubmit
        | HookEventKind::PostToolUse
        | HookEventKind::PreCompact => {
            let obs = observation_for(ev).context("event carries no observation")?;
            client.post(&["observations"], &serde_json::to_value(&obs)?)?;
            Ok(HookOutcome::ok())
        }
        HookEventKind::Stop => stop(ev, cfg, &client),
        HookEventKind::SessionEnd => {
            finalize(
                &client,
                &ev.session_id,
                ev.reason.as_deref().unwrap_or("session_end"),
            )?;
            Ok(HookOutcome::ok())
        }
    }
}

fn session_start(ev: &HookEvent, cfg: &Config, client: &ApiClient) -> anyhow::Result<HookOutcome> {
    let cwd = if ev.cwd.is_empty() {
        std::env::current_dir().context("no cwd in payload and no current directory")?
    } else {
        PathBuf::from(&ev.cwd)
    };
    let project = identify(&cwd)?;
    let req = SessionStartRequest {
        session_id: ev.session_id.clone(),
        agent: ev.agent.clone(),
        cwd: cwd.display().to_string(),
        source: ev.source.clone().unwrap_or_default(),
        project: project.clone(),
    };
    let resp = client.post(&["sessions", "start"], &serde_json::to_value(&req)?)?;
    let resp: SessionStartResponse =
        serde_json::from_value(resp).context("unexpected sessions/start response")?;
    let ctx = StartContext {
        project_name: project.name,
        project_id: resp.project_id,
        server_url: cfg.client.server_url.clone(),
        handoff: resp.pending_handoff.map(|h| h.content_md),
        state: resp.state_excerpt,
    };
    Ok(HookOutcome {
        stdout: render_session_start(cfg.client.lang, &ctx),
        ..HookOutcome::default()
    })
}

fn stop(ev: &HookEvent, cfg: &Config, client: &ApiClient) -> anyhow::Result<HookOutcome> {
    let info = client.get(&["sessions", &ev.session_id], &[])?;
    let info: SessionInfo =
        serde_json::from_value(info).context("unexpected session info response")?;
    match stop_decision(&info, ev.stop_hook_active, cfg.client.stop_nudge) {
        StopDecision::Nudge => Ok(HookOutcome {
            stdout: String::new(),
            stderr: format!(
                "{}\n",
                fill(
                    strings(cfg.client.lang).stop_nudge,
                    &[("project", &info.project_id)]
                )
            ),
            exit_code: NUDGE_EXIT_CODE,
        }),
        StopDecision::Finalize => {
            finalize(client, &ev.session_id, "stop")?;
            Ok(HookOutcome::ok())
        }
    }
}

fn finalize(client: &ApiClient, session: &str, reason: &str) -> anyhow::Result<()> {
    client.post(
        &["sessions", session, "finalize"],
        &json!({ "reason": reason }),
    )?;
    Ok(())
}

/// The sanitized observation an event records (prompt / tool_use / compact), if any.
pub fn observation_for(ev: &HookEvent) -> Option<NewObservation> {
    let mut payload = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            payload.insert(k.to_string(), v);
        }
    };
    let kind = match ev.event {
        HookEventKind::UserPromptSubmit => {
            put(
                "prompt",
                Some(Value::String(ev.prompt.clone().unwrap_or_default())),
            );
            ObservationKind::Prompt
        }
        HookEventKind::PostToolUse => {
            put("tool_name", ev.tool_name.clone().map(Value::String));
            put("tool_input", ev.tool_input.clone());
            put("tool_response", ev.tool_response.clone());
            put("tool_use_id", ev.tool_use_id.clone().map(Value::String));
            ObservationKind::ToolUse
        }
        HookEventKind::PreCompact => {
            put("trigger", ev.trigger.clone().map(Value::String));
            ObservationKind::Compact
        }
        _ => return None,
    };
    Some(NewObservation {
        session_id: ev.session_id.clone(),
        kind,
        ts: Some(now_ts()),
        payload: sanitize_payload(&Value::Object(payload)),
    })
}

/// Where hook failures are logged: `<data_dir>/logs/hook.log` when the data dir exists,
/// else `~/.kioku/logs/hook.log`.
pub fn hook_log_path(cfg: &Config) -> PathBuf {
    if cfg.data_dir.is_dir() {
        DataDir::new(&cfg.data_dir).hook_log()
    } else {
        DataDir::new(&home_dir().join(".kioku")).hook_log()
    }
}

/// Appends one line describing a hook failure; errors while logging are ignored.
pub fn log_failure(cfg: &Config, event: HookEventKind, session: &str, err: &anyhow::Error) {
    let status = http_status(err)
        .map(|s| format!(" status={s}"))
        .unwrap_or_default();
    let line = format!(
        "{} {} session={session}{status} error: {}\n",
        now_ts(),
        event.cli_name(),
        one_line(&format!("{err:#}"))
    );
    append_line(&hook_log_path(cfg), &line);
}

fn append_line(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kioku_core::{SessionCounts, SessionStatus};

    fn info(tool_uses: u32, has_agent_handoff: bool) -> SessionInfo {
        SessionInfo {
            project_id: "kioku-3f9a1c2e".into(),
            status: SessionStatus::Open,
            counts: SessionCounts {
                prompts: 1,
                tool_uses,
            },
            has_agent_handoff,
        }
    }

    #[test]
    fn stop_decision_rules() {
        use StopDecision::*;
        assert_eq!(stop_decision(&info(3, false), false, true), Nudge);
        assert_eq!(stop_decision(&info(10, false), false, true), Nudge);
        assert_eq!(stop_decision(&info(2, false), false, true), Finalize);
        assert_eq!(stop_decision(&info(3, true), false, true), Finalize);
        assert_eq!(stop_decision(&info(3, false), true, true), Finalize);
        assert_eq!(stop_decision(&info(3, false), false, false), Finalize);
    }

    fn cfg_unreachable(dir: &Path) -> Config {
        let mut cfg = Config::for_data_dir(dir);
        // Port 9 (discard) on loopback: connection refused immediately.
        cfg.client.server_url = "http://127.0.0.1:9".into();
        cfg.client.timeout_ms = 500;
        cfg
    }

    #[test]
    fn fail_open_logs_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(dir.path());
        let out = run_hook(
            HookEventKind::Stop,
            Agent::ClaudeCode,
            r#"{"session_id":"s-1","stop_hook_active":false}"#,
            &cfg,
        );
        assert_eq!(out, HookOutcome::ok());
        let out = run_hook(
            HookEventKind::SessionStart,
            Agent::ClaudeCode,
            "not json",
            &cfg,
        );
        assert_eq!(out, HookOutcome::ok());
        let log = std::fs::read_to_string(dir.path().join("logs/hook.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].contains(" stop session=s-1 error: "), "{log}");
        assert!(
            lines[1].contains(" session-start session=- error: "),
            "{log}"
        );
    }

    #[test]
    fn log_falls_back_to_home_when_data_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(&dir.path().join("absent"));
        assert_eq!(
            hook_log_path(&cfg),
            home_dir().join(".kioku").join("logs").join("hook.log")
        );
        let cfg = Config::for_data_dir(dir.path());
        assert_eq!(
            hook_log_path(&cfg),
            dir.path().join("logs").join("hook.log")
        );
    }

    #[test]
    fn observations_are_sanitized() {
        let path = format!(
            "{}/tests/fixtures/user_prompt_submit.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).unwrap();
        let ev = parse_event(Agent::ClaudeCode, HookEventKind::UserPromptSubmit, &text).unwrap();
        let obs = observation_for(&ev).unwrap();
        assert_eq!(obs.kind, ObservationKind::Prompt);
        let prompt = obs.payload["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("引き継ぎ書の自動生成"));
        assert!(!prompt.contains("sk-live"), "{prompt}");
        assert!(prompt.contains("[REDACTED]"));

        let mut raw = ev.raw.clone();
        raw["tool_name"] = json!("Bash");
        raw["tool_input"] = json!({"command": "echo password=hunter2", "big": "x".repeat(9000)});
        raw["tool_response"] = json!({"stdout": "y".repeat(9000), "is_error": false});
        let ev = crate::event::parse_claude_code(HookEventKind::PostToolUse, raw).unwrap();
        let obs = observation_for(&ev).unwrap();
        assert_eq!(obs.kind, ObservationKind::ToolUse);
        assert_eq!(obs.payload["tool_name"], "Bash");
        assert!(obs.payload["tool_input"].to_string().chars().count() <= 4000);
        assert!(obs.payload["tool_response"].to_string().chars().count() <= 2000);
        assert!(!obs.payload.to_string().contains("hunter2"));

        let stop = HookEvent {
            event: HookEventKind::Stop,
            ..ev
        };
        assert!(observation_for(&stop).is_none());
    }
}
