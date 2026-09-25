//! `kioku hook <event> --agent <name>` handlers (M1 §8, M2 §3): thin, fail-open HTTP
//! clients of the server.
//!
//! [`run_hook`] is the whole hook as a pure-ish function (stdin text + config in,
//! stdout/stderr/exit code out) so tests can drive it without spawning the binary;
//! [`run_hook_with_env`] additionally takes the environment. Handlers return a neutral
//! [`HookResult`] which [`render`] turns into the agent's wire format. Besides M1's
//! behaviour this owns the Cursor sniff (§3.7), implicit session start (§3.9), Cursor late
//! context (§5.6) and per-agent deadlines (§3.10).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use kioku_core::sanitize::sanitize_payload;
use kioku_core::strings::{fill, strings};
use kioku_core::util::{home_dir_opt, now_ts, one_line};
use kioku_core::{
    Config, DataDir, NewObservation, ObservationKind, ProjectIdentity, SessionInfo,
    SessionStartRequest, SessionStartResponse, identify,
};
use serde_json::{Map, Value, json};

use crate::client::{ApiClient, http_status};
use crate::context::{StartContext, render_session_start};
use crate::event::{Agent, HookEnv, HookEvent, HookEventKind, hook_deadline_ms, parse_value};
use crate::render::{HookResult, render};

/// Minimum tool uses since the last agent handoff before the Stop hook nudges for one.
pub const NUDGE_MIN_TOOL_USES: u32 = kioku_core::HANDOFF_STALE_TOOL_USES;
/// Exit code that makes Claude Code / Codex feed stderr back to the model (Stop nudge).
pub const NUDGE_EXIT_CODE: i32 = 2;
/// `hook.log` is rotated to `hook.log.1` (one generation) before it would exceed this.
pub const HOOK_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// `source` of a session started by a hook other than SessionStart (M2 §3.9).
pub const IMPLICIT_SOURCE: &str = "implicit";
/// Cursor late-context markers older than this are deleted by sessionStart (M2 §5.6).
pub const CURSOR_MARKER_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// What a hook invocation prints and how it exits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookOutcome {
    /// Text for stdout (context injected into the agent, or the agent's JSON reply).
    pub stdout: String,
    /// Text for stderr (Stop nudge for Claude Code / Codex).
    pub stderr: String,
    /// Process exit code (0, or 2 for the Claude Code / Codex Stop nudge).
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
    /// Nudge the agent to write a handoff first.
    Nudge,
    /// Finalize the session (digest, session page, STATE.md, rules handoff).
    Finalize,
}

/// Stop-hook rule (spec §7.1): nudge only when at least [`NUDGE_MIN_TOOL_USES`] tools were
/// used since the session's latest agent handoff (or since the start, without one), the
/// agent is not already continuing because of a Stop hook, and the nudge is enabled.
pub fn stop_decision(
    info: &SessionInfo,
    stop_hook_active: bool,
    nudge_enabled: bool,
) -> StopDecision {
    if nudge_enabled && !stop_hook_active && info.tool_uses_since_handoff() >= NUDGE_MIN_TOOL_USES {
        StopDecision::Nudge
    } else {
        StopDecision::Finalize
    }
}

/// Runs one hook with the real process environment; see [`run_hook_with_env`].
pub fn run_hook(event: HookEventKind, agent: Agent, stdin_json: &str, cfg: &Config) -> HookOutcome {
    run_hook_with_env(event, agent, stdin_json, cfg, &HookEnv::from_process())
}

/// Runs one hook: parses `stdin_json` for `agent`, talks to the server within the
/// per-agent deadline, and returns what to print. Never fails: errors are appended to
/// `logs/hook.log` and yield the agent's silent reply with exit 0.
pub fn run_hook_with_env(
    event: HookEventKind,
    agent: Agent,
    stdin_json: &str,
    cfg: &Config,
    env: &HookEnv,
) -> HookOutcome {
    let mut agent = agent;
    let mut session = String::from("-");
    let raw: anyhow::Result<Value> =
        serde_json::from_str(stdin_json.trim()).context("hook stdin is not JSON");
    if agent == Agent::ClaudeCode
        && let Ok(raw) = &raw
        && is_cursor_invocation(raw, env)
    {
        if cursor_native_hooks_installed(raw, env) {
            // The native Cursor hook handles this event; avoid double capture (§3.7).
            return render(Agent::ClaudeCode, event, HookResult::Silent);
        }
        agent = Agent::Cursor;
    }
    let result = raw
        .and_then(|raw| parse_value(agent, event, raw, env))
        .and_then(|ev| {
            session = ev.session_id.clone();
            handle(&ev, agent, cfg, env)
        });
    match result {
        Ok(r) => render(agent, event, r),
        Err(err) => {
            log_failure(cfg, event, &session, &err);
            render(agent, event, HookResult::Silent)
        }
    }
}

/// True when a `--agent claude-code` invocation actually runs inside Cursor (§3.7):
/// the payload has `cursor_version` or the environment `CURSOR_VERSION`.
pub fn is_cursor_invocation(raw: &Value, env: &HookEnv) -> bool {
    raw.get("cursor_version").is_some_and(|v| !v.is_null()) || env.var("CURSOR_VERSION").is_some()
}

/// Cursor `hooks.json` files that would hold native kioku hooks: `~/.cursor/hooks.json` and
/// `<workspace root>/.cursor/hooks.json` (root = `workspace_roots[0]`, else payload `cwd`,
/// else `CURSOR_PROJECT_DIR`).
pub fn cursor_hooks_files(raw: &Value, env: &HookEnv) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = &env.home {
        files.push(home.join(".cursor").join("hooks.json"));
    }
    let root = raw
        .get("workspace_roots")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .and_then(Value::as_str)
        .or_else(|| raw.get("cwd").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .or_else(|| env.var("CURSOR_PROJECT_DIR"));
    if let Some(root) = root {
        files.push(Path::new(root).join(".cursor").join("hooks.json"));
    }
    files
}

/// True when one of [`cursor_hooks_files`] registers a kioku hook command.
pub fn cursor_native_hooks_installed(raw: &Value, env: &HookEnv) -> bool {
    fn has_kioku_command(v: &Value) -> bool {
        match v {
            Value::Object(map) => map.iter().any(|(k, v)| {
                (k == "command" && v.as_str().is_some_and(crate::install::is_kioku_command))
                    || has_kioku_command(v)
            }),
            Value::Array(items) => items.iter().any(has_kioku_command),
            _ => false,
        }
    }
    cursor_hooks_files(raw, env).iter().any(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("hooks").cloned())
            .is_some_and(|hooks| has_kioku_command(&hooks))
    })
}

fn handle(ev: &HookEvent, agent: Agent, cfg: &Config, env: &HookEnv) -> anyhow::Result<HookResult> {
    let deadline = hook_deadline_ms(agent, ev.event, cfg.client.timeout_ms);
    let client = ApiClient::new(&cfg.client, Duration::from_millis(deadline))?;
    let h = Handler {
        ev,
        agent,
        cfg,
        env,
        client: &client,
    };
    match ev.event {
        HookEventKind::SessionStart => h.session_start(),
        HookEventKind::UserPromptSubmit
        | HookEventKind::PostToolUse
        | HookEventKind::PreCompact => h.record(),
        HookEventKind::Stop => h.stop(),
        HookEventKind::SessionEnd => {
            h.finalize(ev.reason.as_deref().unwrap_or("session_end"))?;
            Ok(HookResult::Silent)
        }
    }
}

/// One invocation's context, shared by the per-event handlers.
struct Handler<'a> {
    ev: &'a HookEvent,
    agent: Agent,
    cfg: &'a Config,
    env: &'a HookEnv,
    client: &'a ApiClient,
}

impl Handler<'_> {
    fn session_start(&self) -> anyhow::Result<HookResult> {
        if self.agent == Agent::Cursor
            && let Some(dir) = cursor_marker_dir(self.cfg, self.env)
        {
            cleanup_markers(&dir, CURSOR_MARKER_MAX_AGE);
        }
        let source = self.ev.source.clone().unwrap_or_default();
        Ok(HookResult::Context(self.start(&source)?))
    }

    /// `POST /sessions/start` for the resolved cwd; returns the `<kioku>` block.
    fn start(&self, source: &str) -> anyhow::Result<String> {
        let cwd = self.cwd()?;
        let project = identify(&cwd)?;
        let req = SessionStartRequest {
            session_id: self.ev.session_id.clone(),
            agent: self.ev.agent.clone(),
            cwd: cwd.display().to_string(),
            source: source.to_string(),
            project: project.clone(),
        };
        let resp = self
            .client
            .post(&["sessions", "start"], &serde_json::to_value(&req)?)?;
        let resp: SessionStartResponse =
            serde_json::from_value(resp).context("unexpected sessions/start response")?;
        Ok(self.block(&project.name, resp))
    }

    fn block(&self, project_name: &str, resp: SessionStartResponse) -> String {
        let ctx = StartContext {
            project_name: project_name.to_string(),
            project_id: resp.project_id,
            session_id: self.ev.session_id.clone(),
            server_url: self.cfg.client.server_url.clone(),
            handoff: resp.pending_handoff.map(|h| h.content_md),
            state: resp.state_excerpt,
        };
        render_session_start(self.cfg.client.lang, &ctx)
    }

    /// cwd per M2 §3.4: the parser's resolution, else the process cwd — except for
    /// Cursor, whose user-level hooks run in `~/.cursor/` (the event is dropped instead).
    fn cwd(&self) -> anyhow::Result<PathBuf> {
        if !self.ev.cwd.is_empty() {
            return Ok(PathBuf::from(&self.ev.cwd));
        }
        if self.agent == Agent::Cursor {
            anyhow::bail!(
                "cursor payload has no cwd / workspace_roots and CURSOR_PROJECT_DIR is unset: dropped"
            );
        }
        self.env
            .cwd
            .clone()
            .context("no cwd in payload and no current directory")
    }

    /// prompt / tool_use / compact observation, with implicit start on an unknown session.
    fn record(&self) -> anyhow::Result<HookResult> {
        let obs = observation_for(self.ev).context("event carries no observation")?;
        let body = serde_json::to_value(&obs)?;
        let mut implicit_block = None;
        if let Err(err) = self.client.post(&["observations"], &body) {
            if http_status(&err) != Some(404) {
                return Err(err);
            }
            implicit_block = Some(self.start(IMPLICIT_SOURCE)?);
            self.client
                .post(&["observations"], &body)
                .context("retrying the observation after an implicit session start")?;
        }
        match self.ev.event {
            HookEventKind::UserPromptSubmit => {
                // Claude / Codex / Gemini show it now; Cursor renders `{"continue":true}`
                // and gets the block from the late-context path on its next tool use.
                Ok(implicit_block.map_or(HookResult::Silent, HookResult::Context))
            }
            HookEventKind::PostToolUse
                if self.agent == Agent::Cursor && self.cfg.client.cursor_late_context =>
            {
                self.cursor_late_context()
            }
            _ => Ok(HookResult::Silent),
        }
    }

    /// First Cursor tool use of a session: the `<kioku>` block via `additional_context`
    /// (M2 §5.6), exactly once per session thanks to an O_EXCL marker file.
    fn cursor_late_context(&self) -> anyhow::Result<HookResult> {
        let Some(dir) = cursor_marker_dir(self.cfg, self.env) else {
            return Ok(HookResult::Silent);
        };
        kioku_core::util::create_private_dir(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let marker = dir.join(marker_name(&self.ev.session_id));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(HookResult::Silent);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", marker.display()));
            }
        }
        let block = (|| -> anyhow::Result<String> {
            let resp = self
                .client
                .get(&["sessions", &self.ev.session_id, "context"], &[])?;
            let resp: SessionStartResponse =
                serde_json::from_value(resp).context("unexpected session context response")?;
            let name = self
                .cwd()
                .ok()
                .and_then(|cwd| identify(&cwd).ok())
                .filter(|p: &ProjectIdentity| p.id == resp.project_id)
                .map_or_else(|| resp.project_id.clone(), |p| p.name);
            Ok(self.block(&name, resp))
        })();
        match block {
            Ok(b) => Ok(HookResult::Context(b)),
            Err(err) => {
                // Let the next tool use try again.
                let _ = std::fs::remove_file(&marker);
                Err(err)
            }
        }
    }

    fn stop(&self) -> anyhow::Result<HookResult> {
        let info = match self.client.get(&["sessions", &self.ev.session_id], &[]) {
            Ok(v) => v,
            Err(err) if http_status(&err) == Some(404) => {
                // Unknown session: start it; there is nothing to finalize yet (§3.9).
                self.start(IMPLICIT_SOURCE)?;
                return Ok(HookResult::Silent);
            }
            Err(err) => return Err(err),
        };
        let info: SessionInfo =
            serde_json::from_value(info).context("unexpected session info response")?;
        // Cursor: aborted / errored stops finalize without a nudge.
        let completed = self.agent != Agent::Cursor
            || self
                .ev
                .stop_status
                .as_deref()
                .is_none_or(|s| s == "completed");
        let nudge_enabled = self.cfg.client.stop_nudge && completed;
        match stop_decision(&info, self.ev.stop_hook_active, nudge_enabled) {
            StopDecision::Nudge => {
                let t = strings(self.cfg.client.lang);
                let template = if self.agent == Agent::ClaudeCode {
                    t.stop_nudge
                } else {
                    t.stop_nudge_generic
                };
                Ok(HookResult::Nudge(fill(
                    template,
                    &[
                        ("project", &info.project_id),
                        ("session", &self.ev.session_id),
                    ],
                )))
            }
            StopDecision::Finalize => {
                self.finalize("stop")?;
                Ok(HookResult::Silent)
            }
        }
    }

    fn finalize(&self, reason: &str) -> anyhow::Result<()> {
        self.client.post(
            &["sessions", &self.ev.session_id, "finalize"],
            &json!({ "reason": reason }),
        )?;
        Ok(())
    }
}

/// The client-side kioku dir: the data dir when it exists (and is absolute), else
/// `~/.kioku` (client-only machine); `None` without a home directory.
pub fn client_state_root(cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    if cfg.data_dir.is_absolute() && cfg.data_dir.is_dir() {
        return Some(cfg.data_dir.clone());
    }
    env.home
        .as_ref()
        .filter(|h| h.is_absolute())
        .map(|h| h.join(".kioku"))
}

/// Directory of the Cursor late-context markers: `<kioku dir>/state/cursor-ctx`.
pub fn cursor_marker_dir(cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    client_state_root(cfg, env).map(|d| d.join("state").join("cursor-ctx"))
}

/// Marker file name for a session id (anything but `[A-Za-z0-9._-]` becomes `_`).
pub fn marker_name(session_id: &str) -> String {
    let name: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with('.') {
        format!("_{name}")
    } else {
        name
    }
}

/// Deletes marker files older than `max_age` (best effort).
pub fn cleanup_markers(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
            put("native_tool", ev.native_tool.clone().map(Value::String));
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

/// Where hook failures are logged: `<data_dir>/logs/hook.log` when the data dir exists
/// (and is absolute), else `~/.kioku/logs/hook.log`; `None` when no home directory is known
/// — never a path relative to the process cwd, which is the user's repository.
pub fn hook_log_path(cfg: &Config) -> Option<PathBuf> {
    if cfg.data_dir.is_absolute() && cfg.data_dir.is_dir() {
        return Some(DataDir::new(&cfg.data_dir).hook_log());
    }
    home_dir_opt()
        .filter(|h| h.is_absolute())
        .map(|h| DataDir::new(&h.join(".kioku")).hook_log())
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
    if let Some(path) = hook_log_path(cfg) {
        let _ = append_line(&path, &line, HOOK_LOG_MAX_BYTES, false);
    }
}

/// Appends `line`, first rotating the file to `<name>.1` when it would grow past `max`
/// bytes (one old generation is kept). `private` creates a new file 0600 (unix).
pub fn append_line(path: &Path, line: &str, max: u64, private: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        kioku_core::util::create_private_dir(parent)?;
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() + line.len() as u64 > max
    {
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut f = opts.open(path)?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::parse_event;
    use kioku_core::{SessionCounts, SessionStatus};

    fn append_line_default(path: &Path, line: &str) {
        append_line(path, line, HOOK_LOG_MAX_BYTES, false).unwrap();
    }

    fn info(tool_uses: u32, has_agent_handoff: bool) -> SessionInfo {
        SessionInfo {
            project_id: "kioku-3f9a1c2e".into(),
            status: SessionStatus::Open,
            counts: SessionCounts {
                prompts: 1,
                tool_uses,
            },
            has_agent_handoff,
            tool_uses_since_handoff: None,
        }
    }

    fn since(total: u32, since: u32) -> SessionInfo {
        SessionInfo {
            tool_uses_since_handoff: Some(since),
            ..info(total, true)
        }
    }

    #[test]
    fn stop_decision_rules() {
        use StopDecision::*;
        // old server (no tool_uses_since_handoff): cumulative count without a handoff
        assert_eq!(stop_decision(&info(3, false), false, true), Nudge);
        assert_eq!(stop_decision(&info(10, false), false, true), Nudge);
        assert_eq!(stop_decision(&info(2, false), false, true), Finalize);
        assert_eq!(stop_decision(&info(3, true), false, true), Finalize);
        assert_eq!(stop_decision(&info(3, false), true, true), Finalize);
        assert_eq!(stop_decision(&info(3, false), false, false), Finalize);
    }

    #[test]
    fn stop_decision_counts_tool_uses_since_the_last_handoff() {
        use StopDecision::*;
        // a handoff covers earlier work: many tool uses overall, none since → no nudge
        assert_eq!(stop_decision(&since(40, 0), false, true), Finalize);
        assert_eq!(stop_decision(&since(40, 2), false, true), Finalize);
        // work continued after the handoff → nudge to refresh it
        assert_eq!(stop_decision(&since(40, 3), false, true), Nudge);
        assert_eq!(stop_decision(&since(40, 3), true, true), Finalize);
        assert_eq!(stop_decision(&since(40, 3), false, false), Finalize);
        // the field wins over has_agent_handoff / counts
        let mut i = since(3, 3);
        i.has_agent_handoff = false;
        assert_eq!(stop_decision(&i, false, true), Nudge);
        i.tool_uses_since_handoff = Some(0);
        assert_eq!(stop_decision(&i, false, true), Finalize);
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
            home_dir_opt().map(|h| h.join(".kioku").join("logs").join("hook.log"))
        );
        let cfg = Config::for_data_dir(dir.path());
        assert_eq!(
            hook_log_path(&cfg),
            Some(dir.path().join("logs").join("hook.log"))
        );
        // a relative data dir (what `~/.kioku` becomes without HOME) is never used, even
        // if it happens to exist relative to the cwd
        let cfg = Config::for_data_dir(Path::new("."));
        assert_ne!(hook_log_path(&cfg), Some(PathBuf::from("./logs/hook.log")));
        assert!(hook_log_path(&cfg).is_none_or(|p| p.is_absolute()));
    }

    #[test]
    fn hook_log_rotates_once_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("hook.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "x".repeat(HOOK_LOG_MAX_BYTES as usize - 10)).unwrap();
        append_line_default(&log, "short\n");
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            HOOK_LOG_MAX_BYTES - 10 + 6
        );
        append_line_default(&log, "this line crosses the cap\n");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "this line crosses the cap\n"
        );
        let old = dir.path().join("logs").join("hook.log.1");
        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            HOOK_LOG_MAX_BYTES - 4
        );
        // the next rotation replaces the single old generation
        std::fs::write(&log, "y".repeat(HOOK_LOG_MAX_BYTES as usize)).unwrap();
        append_line_default(&log, "z\n");
        assert!(std::fs::read_to_string(&old).unwrap().starts_with('y'));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "z\n");
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

    fn env_home(home: &Path) -> HookEnv {
        HookEnv {
            vars: Default::default(),
            home: Some(home.to_path_buf()),
            cwd: None,
        }
    }

    fn fixture_text(rel: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{rel}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn log_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("logs/hook.log")).unwrap_or_default()
    }

    #[test]
    fn cursor_sniff_is_silent_when_native_hooks_exist() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(data.path());
        let stdin = fixture_text("cursor/claude_import_stop.docs.json");
        let raw: Value = serde_json::from_str(&stdin).unwrap();
        let env = env_home(home.path());
        assert!(is_cursor_invocation(&raw, &env));
        assert!(!is_cursor_invocation(&json!({"session_id": "s"}), &env));
        let mut with_var = env.clone();
        with_var
            .vars
            .insert("CURSOR_VERSION".into(), "3.1.0".into());
        assert!(is_cursor_invocation(&json!({"session_id": "s"}), &with_var));

        // No native hooks: re-dispatched to the Cursor parser/renderer (fail-open `{}` here,
        // the server is unreachable) and the failure is logged.
        let out = run_hook_with_env(HookEventKind::Stop, Agent::ClaudeCode, &stdin, &cfg, &env);
        assert_eq!(out.stdout, "{}\n");
        assert_eq!(out.exit_code, 0);
        assert_eq!(log_text(data.path()).lines().count(), 1);

        // Native kioku hook in ~/.cursor/hooks.json (foreign hooks around it) → Silent, no
        // request, nothing logged.
        let cursor = home.path().join(".cursor");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(
            cursor.join("hooks.json"),
            r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"./fmt.sh"}],"stop":[{"command":"/Users/me/.local/bin/kioku hook stop --agent cursor","timeout":10}]}}"#,
        )
        .unwrap();
        let out = run_hook_with_env(HookEventKind::Stop, Agent::ClaudeCode, &stdin, &cfg, &env);
        assert_eq!(out, HookOutcome::ok());
        assert_eq!(log_text(data.path()).lines().count(), 1);
    }

    #[test]
    fn cursor_sniff_finds_project_hooks_and_ignores_foreign_ones() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = env_home(home.path());
        let raw = json!({"conversation_id": "c", "cursor_version": "3.1.0",
            "workspace_roots": [root.path().display().to_string()]});
        assert_eq!(
            cursor_hooks_files(&raw, &env),
            vec![
                home.path().join(".cursor/hooks.json"),
                root.path().join(".cursor/hooks.json")
            ]
        );
        assert!(!cursor_native_hooks_installed(&raw, &env));
        std::fs::create_dir_all(root.path().join(".cursor")).unwrap();
        let hooks = root.path().join(".cursor/hooks.json");
        std::fs::write(&hooks, r#"{"hooks":{"stop":[{"command":"echo kioku"}]}}"#).unwrap();
        assert!(!cursor_native_hooks_installed(&raw, &env), "foreign only");
        std::fs::write(&hooks, "{ not json").unwrap();
        assert!(!cursor_native_hooks_installed(&raw, &env));
        std::fs::write(
            &hooks,
            r#"{"hooks":{"postToolUse":[{"command":"\"/Users/me/my bin/kioku\" hook post-tool-use --agent cursor"}]}}"#,
        )
        .unwrap();
        assert!(cursor_native_hooks_installed(&raw, &env));
        // Claude-shaped payload: the root comes from `cwd`
        let claude_shaped = json!({"session_id": "s", "cwd": root.path().display().to_string()});
        assert!(cursor_native_hooks_installed(&claude_shaped, &env));
    }

    #[test]
    fn cursor_without_root_never_uses_the_process_cwd() {
        let data = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(data.path());
        let env = HookEnv {
            cwd: Some(proj.path().to_path_buf()),
            ..HookEnv::default()
        };
        let out = run_hook_with_env(
            HookEventKind::SessionStart,
            Agent::Cursor,
            r#"{"conversation_id":"c1","hook_event_name":"sessionStart","workspace_roots":[]}"#,
            &cfg,
            &env,
        );
        assert_eq!(out.stdout, "{}\n");
        let log = log_text(data.path());
        assert!(log.contains("session-start session=c1 error: "), "{log}");
        assert!(log.contains("dropped"), "{log}");
    }

    #[test]
    fn fail_open_replies_per_agent() {
        let data = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(data.path());
        let env = HookEnv::default();
        for (agent, event, want) in [
            (Agent::GeminiCli, HookEventKind::SessionStart, "{}\n"),
            (Agent::GeminiCli, HookEventKind::Stop, "{}\n"),
            (
                Agent::Cursor,
                HookEventKind::UserPromptSubmit,
                "{\"continue\":true}\n",
            ),
            (Agent::Cursor, HookEventKind::SessionEnd, "{}\n"),
            (Agent::Codex, HookEventKind::Stop, ""),
            (Agent::ClaudeCode, HookEventKind::Stop, ""),
        ] {
            for stdin in [
                "garbage",
                r#"{"session_id":"s","conversation_id":"s","cwd":"/w"}"#,
            ] {
                let out = run_hook_with_env(event, agent, stdin, &cfg, &env);
                assert_eq!(out.stdout, want, "{agent:?} {event:?} {stdin}");
                assert_eq!(out.exit_code, 0);
                assert!(out.stderr.is_empty());
            }
        }
    }

    #[test]
    fn markers() {
        assert_eq!(marker_name("c7e1-a4b2"), "c7e1-a4b2");
        assert_eq!(marker_name("../etc/passwd"), "_.._etc_passwd");
        assert_eq!(marker_name("a/b c"), "a_b_c");
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let fresh = dir.path().join("fresh");
        std::fs::write(&old, "").unwrap();
        std::fs::write(&fresh, "").unwrap();
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 3600))
            .unwrap();
        cleanup_markers(dir.path(), CURSOR_MARKER_MAX_AGE);
        assert!(!old.exists());
        assert!(fresh.exists());
        cleanup_markers(&dir.path().join("missing"), CURSOR_MARKER_MAX_AGE);

        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = env_home(home.path());
        let cfg = Config::for_data_dir(data.path());
        assert_eq!(
            cursor_marker_dir(&cfg, &env),
            Some(data.path().join("state/cursor-ctx"))
        );
        let cfg = Config::for_data_dir(&data.path().join("absent"));
        assert_eq!(
            cursor_marker_dir(&cfg, &env),
            Some(home.path().join(".kioku/state/cursor-ctx"))
        );
        assert_eq!(cursor_marker_dir(&cfg, &HookEnv::default()), None);
    }

    #[test]
    fn normalized_observations_keep_native_tool_and_patch_paths() {
        let text = fixture_text("codex/post_tool_use_apply_patch.docs.json");
        let ev = parse_event(Agent::Codex, HookEventKind::PostToolUse, &text).unwrap();
        let obs = observation_for(&ev).unwrap();
        assert_eq!(obs.payload["tool_name"], "Edit");
        assert_eq!(obs.payload["native_tool"], "apply_patch");
        let input = &obs.payload["tool_input"];
        assert!(input.to_string().chars().count() <= 4000);
        assert_eq!(input["file_paths"].as_array().unwrap().len(), 3);
        assert!(
            input["file_paths"][2]
                .as_str()
                .unwrap()
                .ends_with("docs/notes/codex.md")
        );
        // Claude payloads carry no native_tool (M1 payload shape unchanged)
        let text = fixture_text("post_tool_use.json");
        let ev = parse_event(Agent::ClaudeCode, HookEventKind::PostToolUse, &text).unwrap();
        assert!(
            observation_for(&ev)
                .unwrap()
                .payload
                .get("native_tool")
                .is_none()
        );
        // Gemini error → is_error survives sanitization
        let text = fixture_text("gemini-cli/after_tool_shell_error.docs.json");
        let ev = parse_event(Agent::GeminiCli, HookEventKind::PostToolUse, &text).unwrap();
        let obs = observation_for(&ev).unwrap();
        assert_eq!(obs.payload["tool_response"]["is_error"], true);
        assert_eq!(obs.payload["native_tool"], "run_shell_command");
    }
}
