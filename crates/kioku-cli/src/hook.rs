//! `kioku hook <event> --agent <name>` handlers (M1 §8, M2 §3): thin, fail-open HTTP
//! clients of the server.
//!
//! [`run_hook`] is the whole hook as a pure-ish function (stdin text + config in,
//! stdout/stderr/exit code out) so tests can drive it without spawning the binary;
//! [`run_hook_with_env`] additionally takes the environment. Handlers return a neutral
//! [`HookResult`] which [`render`] turns into the agent's wire format. Besides M1's
//! behaviour this owns the Cursor sniff (§3.7), implicit session start (§3.9), Cursor late
//! context (§5.6), per-agent deadlines (§3.10) and Antigravity's PreInvocation prompt
//! capture, late context and Stop guard (M2.1 §3.5–§3.7).

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

/// Hook stdin bytes as text (SPEC-M2.2 §4.4): a leading UTF-8 BOM is dropped and the rest
/// decoded as UTF-8 (lossily: a stray invalid byte never loses the whole payload). Never
/// goes through an ANSI code page, so Japanese survives on Japanese Windows; CRLF is left
/// to the JSON parser, which treats it as whitespace.
pub fn decode_stdin(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
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
        serde_json::from_str(stdin_json.trim_start_matches('\u{feff}').trim())
            .context("hook stdin is not JSON");
    if agent == Agent::ClaudeCode
        && let Ok(raw) = &raw
        && is_cursor_invocation(raw)
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

/// True when a `--agent claude-code` invocation actually runs inside Cursor (§3.7), decided
/// by the payload alone: `cursor_version`, `conversation_id`, `workspace_roots` or a
/// camelCase `hook_event_name` (Claude's are PascalCase). `CURSOR_VERSION` in the
/// environment is never enough — it leaks into Claude Code started from Cursor's terminal.
pub fn is_cursor_invocation(raw: &Value) -> bool {
    let present = |k: &str| raw.get(k).is_some_and(|v| !v.is_null());
    present("cursor_version")
        || present("conversation_id")
        || present("workspace_roots")
        || raw
            .get("hook_event_name")
            .and_then(Value::as_str)
            .and_then(|n| n.chars().next())
            .is_some_and(|c| c.is_ascii_lowercase())
}

/// True when a Cursor post-tool event can carry `additional_context`: only Cursor's native
/// `postToolUse` (or a Claude-shaped PostToolUse from imported hooks) — never `afterFileEdit`
/// or `postToolUseFailure` (M2 §5.6).
pub fn cursor_accepts_late_context(ev: &HookEvent) -> bool {
    match ev.native_event.as_str() {
        "postToolUse" => true,
        "" | "PostToolUse" => {
            ev.native_tool.as_deref() != Some("afterFileEdit")
                && ev
                    .tool_response
                    .as_ref()
                    .and_then(|r| r.get("is_error"))
                    .and_then(Value::as_bool)
                    != Some(true)
        }
        _ => false,
    }
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
        HookEventKind::UserPromptSubmit if agent == Agent::Antigravity => {
            h.antigravity_invocation()
        }
        HookEventKind::UserPromptSubmit
        | HookEventKind::PostToolUse
        | HookEventKind::PreCompact => h.record(),
        HookEventKind::Stop => h.stop(),
        HookEventKind::SessionEnd => {
            match h.finalize(ev.reason.as_deref().unwrap_or("session_end")) {
                // Unknown session: SessionStart never fired (Codex fires it with the first
                // turn, so open-then-quit sends only SessionEnd) — nothing to finalize.
                Err(err) if http_status(&err) == Some(404) => {}
                other => other?,
            }
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
        if let Some(dir) = marker_dir(self.agent, self.cfg, self.env) {
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
        // Antigravity runs hooks in the directory of their hooks.json (M2.1 §3.3): a
        // workspace `.agents/` identifies the repository, `~/.gemini/config` must not.
        if self.agent == Agent::Antigravity {
            let gemini = self.env.home.as_ref().map(|h| h.join(".gemini"));
            return self
                .env
                .cwd
                .clone()
                .filter(|c| gemini.as_ref().is_none_or(|g| !c.starts_with(g)))
                .context("antigravity payload has empty workspacePaths: dropped");
        }
        self.env
            .cwd
            .clone()
            .context("no cwd in payload and no current directory")
    }

    /// prompt / tool_use / compact observation, with implicit start on an unknown session.
    fn record(&self) -> anyhow::Result<HookResult> {
        let implicit_block = self.post_observation(self.ev)?;
        if self.agent == Agent::Antigravity
            && self.ev.event == HookEventKind::PostToolUse
            && let Some(d) = marker_dir(self.agent, self.cfg, self.env)
        {
            // This agy fires tool events: stop estimating tool rounds (M2.1 §3.7).
            let _ = kioku_core::util::create_private_dir(&d);
            let _ = std::fs::write(d.join(tools_marker_name(&self.ev.session_id)), "");
        }
        match self.ev.event {
            HookEventKind::UserPromptSubmit => {
                // Claude / Codex / Gemini show it now; Cursor renders `{"continue":true}`
                // and gets the block from the late-context path on its next tool use.
                Ok(implicit_block.map_or(HookResult::Silent, HookResult::Context))
            }
            HookEventKind::PostToolUse
                if self.agent == Agent::Cursor
                    && self.cfg.client.cursor_late_context
                    && cursor_accepts_late_context(self.ev) =>
            {
                self.late_context()
            }
            _ => Ok(HookResult::Silent),
        }
    }

    /// Posts `ev`'s observation; an unknown session is started implicitly (§3.9) and the
    /// observation retried — the start's `<kioku>` block is returned then.
    fn post_observation(&self, ev: &HookEvent) -> anyhow::Result<Option<String>> {
        let obs = observation_for(ev).context("event carries no observation")?;
        let body = serde_json::to_value(&obs)?;
        let Err(err) = self.client.post(&["observations"], &body) else {
            return Ok(None);
        };
        if http_status(&err) != Some(404) {
            return Err(err);
        }
        let block = self.start(IMPLICIT_SOURCE)?;
        self.client
            .post(&["observations"], &body)
            .context("retrying the observation after an implicit session start")?;
        Ok(Some(block))
    }

    /// Antigravity PreInvocation (M2.1 §3.5, §3.6): records a prompt that is new in the
    /// transcript, then delivers the `<kioku>` block once per conversation.
    fn antigravity_invocation(&self) -> anyhow::Result<HookResult> {
        let dir = marker_dir(self.agent, self.cfg, self.env);
        let transcript = self
            .ev
            .raw
            .get("transcriptPath")
            .and_then(Value::as_str)
            .filter(|p| Path::new(p).is_absolute());
        let offset_file = dir
            .as_ref()
            .map(|d| d.join(format!("{}.prompt", marker_name(&self.ev.session_id))));
        let seen: u64 = offset_file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        let mut block = None;
        if let Some((prompt, end)) = transcript
            .and_then(|p| last_user_input(Path::new(p)))
            .filter(|&(_, end)| end > seen)
        {
            let mut ev = self.ev.clone();
            ev.prompt = Some(prompt);
            block = self.post_observation(&ev)?;
            if let (Some(d), Some(f)) = (&dir, &offset_file) {
                let _ = kioku_core::util::create_private_dir(d);
                let _ = std::fs::write(f, end.to_string());
            }
        }
        // A model call after the first one means the previous call ran tools: one
        // `tool_round` so the Stop nudge threshold works without tool events (M2.1 §3.7).
        let round = self
            .ev
            .raw
            .get("invocationNum")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let real_tools = dir
            .as_ref()
            .is_some_and(|d| d.join(tools_marker_name(&self.ev.session_id)).exists());
        if round > 0 && !real_tools {
            let mut ev = self.ev.clone();
            ev.event = HookEventKind::PostToolUse;
            ev.tool_name = Some(ANTIGRAVITY_TOOL_ROUND.to_string());
            ev.native_tool = Some("PreInvocation".to_string());
            ev.tool_input = Some(json!({ "invocationNum": round }));
            if let Some(b) = self.post_observation(&ev)? {
                block.get_or_insert(b);
            }
        }
        match block {
            Some(b) => {
                // The implicit start already carries the block: mark it delivered.
                if let Some(d) = &dir {
                    let _ = kioku_core::util::create_private_dir(d);
                    let _ = std::fs::write(
                        d.join(ctx_marker_name(self.agent, &self.ev.session_id)),
                        "",
                    );
                }
                Ok(HookResult::Context(b))
            }
            None => self.late_context(),
        }
    }

    /// First Cursor tool use / Antigravity model call of a session: the `<kioku>` block
    /// (M2 §5.6, M2.1 §3.6), exactly once per session thanks to an O_EXCL marker file. An
    /// unknown session is started implicitly and gets that start's block.
    fn late_context(&self) -> anyhow::Result<HookResult> {
        let Some(dir) = marker_dir(self.agent, self.cfg, self.env) else {
            return Ok(HookResult::Silent);
        };
        kioku_core::util::create_private_dir(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let marker = dir.join(ctx_marker_name(self.agent, &self.ev.session_id));
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
            let resp = match self
                .client
                .get(&["sessions", &self.ev.session_id, "context"], &[])
            {
                Ok(v) => v,
                Err(err) if http_status(&err) == Some(404) => {
                    return self.start(IMPLICIT_SOURCE);
                }
                Err(err) => return Err(err),
            };
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
        // Antigravity (M2.1 §3.7): our own marker says the previous Stop nudged.
        let nudge_marker = (self.agent == Agent::Antigravity)
            .then(|| marker_dir(self.agent, self.cfg, self.env))
            .flatten()
            .map(|d| d.join(format!("{}.nudge", marker_name(&self.ev.session_id))));
        let mut active = self.ev.stop_hook_active;
        if let Some(m) = &nudge_marker
            && std::fs::remove_file(m).is_ok()
        {
            active = true;
        }
        match stop_decision(&info, active, nudge_enabled) {
            StopDecision::Nudge => {
                if let Some(m) = &nudge_marker
                    && let Some(d) = m.parent()
                {
                    let _ = kioku_core::util::create_private_dir(d);
                    let _ = std::fs::write(m, "");
                }
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

/// Directory of the Antigravity markers: `<kioku dir>/state/antigravity` (M2.1 §3.5–§3.7).
pub fn antigravity_marker_dir(cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    client_state_root(cfg, env).map(|d| d.join("state").join("antigravity"))
}

/// Late-context marker directory of an agent (Cursor, Antigravity); `None` for the others.
pub fn marker_dir(agent: Agent, cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    match agent {
        Agent::Cursor => cursor_marker_dir(cfg, env),
        Agent::Antigravity => antigravity_marker_dir(cfg, env),
        _ => None,
    }
}

/// Late-context marker file name: Cursor keeps M2's bare name, Antigravity adds `.ctx` next
/// to its `.prompt` / `.nudge` files.
fn ctx_marker_name(agent: Agent, session_id: &str) -> String {
    let name = marker_name(session_id);
    if agent == Agent::Antigravity {
        format!("{name}.ctx")
    } else {
        name
    }
}

/// `<id>.tools`: this conversation has sent a real PostToolUse (agy ≥ 1.2.12).
fn tools_marker_name(session_id: &str) -> String {
    format!("{}.tools", marker_name(session_id))
}

/// The user's text of an agy USER_INPUT step: the `<USER_REQUEST>` element when present
/// (agy appends `<ADDITIONAL_METADATA>` etc. after it), else the whole content.
pub fn user_request(content: &str) -> String {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    match content.find(OPEN) {
        Some(start) => {
            let rest = &content[start + OPEN.len()..];
            let body = rest.find(CLOSE).map_or(rest, |end| &rest[..end]);
            body.trim().to_string()
        }
        None => content.to_string(),
    }
}

/// Tool name recorded for an Antigravity model call after the first of a turn (M2.1 §3.7).
pub const ANTIGRAVITY_TOOL_ROUND: &str = "tool_round";

/// Bytes of an Antigravity transcript's tail searched for the last prompt (M2.1 §3.5).
pub const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

/// The last `USER_INPUT` step in the tail of an Antigravity transcript (JSONL) and the byte
/// offset where its line ends; `None` when the file is unreadable or holds none.
pub fn last_user_input(path: &Path) -> Option<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    let mut found = None;
    let mut end = start;
    for line in buf.split_inclusive(|&b| b == b'\n') {
        end += line.len() as u64;
        // A partial first line (tail cut) or last line (still being written) fails to parse.
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("USER_INPUT") {
            continue;
        }
        let text = match v.get("content") {
            Some(Value::String(t)) => Some(t.clone()),
            Some(c) => c.get("text").and_then(Value::as_str).map(str::to_string),
            None => None,
        };
        if let Some(t) = text {
            found = Some((user_request(&t), end));
        }
    }
    found
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

    #[test]
    fn last_user_input_reads_the_newest_prompt_from_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        assert_eq!(last_user_input(&path), None, "missing file");
        let first = r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","content":"一つ目の指示"}"#;
        let model = r#"{"type":"PLANNER_RESPONSE","source":"MODEL","content":"了解"}"#;
        let second = r#"{"type":"USER_INPUT","content":{"text":"二つ目の指示"}}"#;
        std::fs::write(&path, format!("{first}\n{model}\n")).unwrap();
        let (text, end1) = last_user_input(&path).unwrap();
        assert_eq!(text, "一つ目の指示");
        assert_eq!(end1, first.len() as u64 + 1);
        // A newer prompt and a half-written line after it.
        let tail = format!("{second}\n{{\"type\":\"USER_IN");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(tail.as_bytes()).unwrap();
        let (text, end2) = last_user_input(&path).unwrap();
        assert_eq!(text, "二つ目の指示");
        assert!(end2 > end1);
        // Only the last TRANSCRIPT_TAIL_BYTES are read: a prompt before them is not seen.
        let big = dir.path().join("big.jsonl");
        let filler = format!("{model}\n").repeat(TRANSCRIPT_TAIL_BYTES as usize / model.len() + 1);
        std::fs::write(&big, format!("{first}\n{filler}")).unwrap();
        assert_eq!(last_user_input(&big), None);
    }

    #[test]
    fn user_request_is_cut_out_of_the_captured_transcript() {
        let path = std::path::PathBuf::from(format!(
            "{}/tests/fixtures/antigravity/transcript.captured.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ));
        let (text, _) = last_user_input(&path).unwrap();
        assert!(
            text.starts_with("このリポジトリ直下に hello.txt を作って"),
            "{text}"
        );
        assert!(text.ends_with("終了してください。"), "{text}");
        assert_eq!(user_request("素のテキスト"), "素のテキスト");
        assert_eq!(user_request("<USER_REQUEST>\n途中で切れた"), "途中で切れた");
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
    fn cursor_version_env_alone_does_not_make_a_claude_payload_cursor() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(data.path());
        let mut env = env_home(home.path());
        env.vars.insert("CURSOR_VERSION".into(), "3.1.0".into());
        // Claude Code started from Cursor's integrated terminal inherits CURSOR_VERSION.
        let stdin = json!({
            "session_id": "claude-in-cursor-terminal",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": home.path().display().to_string(),
            "hook_event_name": "Stop",
            "stop_hook_active": false,
        })
        .to_string();
        let out = run_hook_with_env(HookEventKind::Stop, Agent::ClaudeCode, &stdin, &cfg, &env);
        // Claude Code's fail-open rendering (empty stdout), not Cursor's `{}`.
        assert_eq!(out, HookOutcome::ok());
    }

    #[test]
    fn stdin_with_bom_crlf_and_japanese_survives() {
        // SPEC-M2.2 §4.4: what a Windows runtime may hand us — BOM, CRLF, raw UTF-8.
        let body = "{\r\n  \"session_id\": \"s-win\",\r\n  \"cwd\": \"C:\\\\Users\\\\山田\\\\repo\",\r\n  \"hook_event_name\": \"UserPromptSubmit\",\r\n  \"prompt\": \"検索インデックスの日本語トークナイズを直して\"\r\n}\r\n";
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(body.as_bytes());
        let text = decode_stdin(&bytes);
        assert!(!text.starts_with('\u{feff}'));
        let ev = parse_event(Agent::ClaudeCode, HookEventKind::UserPromptSubmit, &text).unwrap();
        assert_eq!(
            ev.prompt.as_deref(),
            Some("検索インデックスの日本語トークナイズを直して")
        );
        assert_eq!(ev.cwd, r"C:\Users\山田\repo");
        // A BOM that reaches run_hook as text is tolerated too.
        let with_bom = format!("\u{feff}{body}");
        let raw: Value = serde_json::from_str(with_bom.trim_start_matches('\u{feff}')).unwrap();
        assert_eq!(raw["session_id"], "s-win");
        // One invalid byte does not lose the payload.
        let mut broken = body.as_bytes().to_vec();
        broken.insert(2, 0xFF);
        assert!(decode_stdin(&broken).contains("日本語"));
    }

    #[test]
    fn cursor_late_context_only_on_native_post_tool_use() {
        let env = HookEnv::default();
        let parse = |rel: &str| {
            let raw: Value = serde_json::from_str(&fixture_text(rel)).unwrap();
            crate::event::parse_value(Agent::Cursor, HookEventKind::PostToolUse, raw, &env).unwrap()
        };
        assert!(cursor_accepts_late_context(&parse(
            "cursor/post_tool_use_shell.docs.json"
        )));
        assert!(cursor_accepts_late_context(&parse(
            "cursor/post_tool_use_read.docs.json"
        )));
        assert!(!cursor_accepts_late_context(&parse(
            "cursor/after_file_edit.docs.json"
        )));
        assert!(!cursor_accepts_late_context(&parse(
            "cursor/post_tool_use_failure.docs.json"
        )));
    }

    #[test]
    fn cursor_sniff_is_silent_when_native_hooks_exist() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let cfg = cfg_unreachable(data.path());
        let stdin = fixture_text("cursor/claude_import_stop.docs.json");
        let raw: Value = serde_json::from_str(&stdin).unwrap();
        let env = env_home(home.path());
        assert!(is_cursor_invocation(&raw));
        assert!(!is_cursor_invocation(&json!({"session_id": "s"})));
        for cursor in [
            json!({"conversation_id": "c"}),
            json!({"session_id": "s", "workspace_roots": ["/w"]}),
            json!({"session_id": "s", "hook_event_name": "postToolUse"}),
            json!({"session_id": "s", "cursor_version": "3.1.0"}),
        ] {
            assert!(is_cursor_invocation(&cursor), "{cursor}");
        }
        assert!(!is_cursor_invocation(
            &json!({"session_id": "s", "hook_event_name": "PostToolUse", "cursor_version": null})
        ));

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
            // (`conversation_id` would make a claude-code invocation a Cursor one, §3.7.)
            let shaped = if agent == Agent::ClaudeCode {
                r#"{"session_id":"s","cwd":"/w"}"#
            } else {
                r#"{"session_id":"s","conversation_id":"s","cwd":"/w"}"#
            };
            for stdin in ["garbage", shaped] {
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
