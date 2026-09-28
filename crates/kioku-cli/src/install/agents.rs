//! Per-agent installers (M2 §8): the file table of §8.2, the exact hook shapes of §4.2
//! (Codex), §5.2 (Cursor) and §6.2 (Gemini CLI) — timeouts derived from
//! [`registered_timeout_ms`] so the hook deadlines of §3.10 match what is registered — MCP
//! registration (§4.5, §5.5, §6.5), the instruction snippet (§7) and `install all` (§8.3).
//!
//! Every path is taken from an [`InstallCtx`], so tests run against a temp HOME without
//! touching the process environment.

use std::path::{Path, PathBuf};

use anyhow::Context;
use kioku_core::ClientConfig;
use serde_json::{Map, Value, json};

use super::block::{self, FileOutcome};
use super::{
    HookSpec, McpChange, SettingsChange, Written, agent_hook_command, claude_specs,
    mcp_server_entry, mcp_url, merge_flat, merge_mcp_entry, merge_nested, read_settings,
    register_mcp_entry, remove_flat, remove_hooks, remove_mcp_server, rewrite_json_after_removal,
    unregister_mcp_entry, write_settings,
};
use crate::event::{ALL_AGENTS, ALL_EVENTS, Agent, HookEventKind, registered_timeout_ms};

/// Where an installer reads and writes: home, Codex home, working directory, binary, client
/// config.
#[derive(Clone, Debug)]
pub struct InstallCtx {
    /// Home directory (`~`).
    pub home: PathBuf,
    /// `$CODEX_HOME`, default `~/.codex`.
    pub codex_home: PathBuf,
    /// Working directory (`--project` root: its git top level, else itself).
    pub cwd: PathBuf,
    /// Absolute path of the kioku binary registered in hook commands.
    pub bin: String,
    /// `[client]` settings: server URL, token, language.
    pub client: ClientConfig,
}

impl InstallCtx {
    /// Context from the real process environment.
    pub fn from_process(client: ClientConfig, bin: String) -> anyhow::Result<InstallCtx> {
        let home = kioku_core::util::home_dir();
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Ok(InstallCtx {
            home,
            codex_home,
            cwd: std::env::current_dir().context("reading current directory")?,
            bin,
            client,
        })
    }

    /// Project root for `--project`: git top level of the cwd, else the cwd (M2 §8.1).
    pub fn project_root(&self) -> PathBuf {
        kioku_core::project::git_toplevel(&self.cwd).unwrap_or_else(|| self.cwd.clone())
    }

    /// The directory `--project` writes into for `agent` (Claude Code: the cwd, M1; the
    /// others: [`InstallCtx::project_root`]).
    pub fn project_dir(&self, agent: Agent) -> PathBuf {
        if agent == Agent::ClaudeCode {
            self.cwd.clone()
        } else {
            self.project_root()
        }
    }

    /// Error when `--project` would write into the home directory itself (the project files
    /// would then be the user-level ones, e.g. `~/.cursor/hooks.json`, or a dotfiles repo
    /// rooted at `~`).
    pub fn check_project_dir(&self, agent: Agent) -> anyhow::Result<()> {
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let dir = self.project_dir(agent);
        if canon(&dir) == canon(&self.home) {
            anyhow::bail!(
                "--project resolves to your home directory ({}){}; run it inside a project directory, or drop --project for the user-level install",
                dir.display(),
                if dir != self.cwd {
                    " because it is the git top level"
                } else {
                    ""
                }
            );
        }
        Ok(())
    }

    fn token(&self) -> Option<String> {
        self.client
            .auth_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    }
}

/// Instruction snippet choice (`--instructions` / `--no-instructions`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Instructions {
    /// The agent's default (Codex, Gemini CLI, Antigravity: yes; Cursor: `--project` only; Claude: no).
    #[default]
    Default,
    /// `--no-instructions`.
    Skip,
    /// `--instructions`.
    Force,
}

/// Flags of `kioku install`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// `--project`: hooks and instructions in the project instead of the user config.
    pub project: bool,
    /// Instruction snippet choice.
    pub instructions: Instructions,
    /// `--dry-run`: report, write nothing.
    pub dry_run: bool,
    /// Codex `--enable-hooks-feature`.
    pub enable_hooks_feature: bool,
    /// Gemini CLI `--trust-mcp`.
    pub trust_mcp: bool,
    /// `--mcp-http`: register the v0.3 URL + token MCP entry instead of the `kioku mcp`
    /// stdio bridge (M2 §20.2).
    pub mcp_http: bool,
}

/// What one agent's install / uninstall did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentReport {
    /// The agent.
    pub agent: Agent,
    /// True when any file was (dry run: would be) written or removed.
    pub changed: bool,
    /// Report lines (never the token, except inside a manual-registration snippet).
    pub lines: Vec<String>,
}

impl AgentReport {
    fn new(agent: Agent) -> AgentReport {
        AgentReport {
            agent,
            changed: false,
            lines: Vec::new(),
        }
    }

    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn mcp(&mut self, change: McpChange) {
        self.changed |= change.changed;
        self.push(change.message);
    }

    fn file(&mut self, what: &str, path: &Path, outcome: &FileOutcome, dry_run: bool) {
        let verb = match (outcome, dry_run) {
            (FileOutcome::Unchanged, _) => "unchanged",
            (FileOutcome::Written, false) => "written",
            (FileOutcome::Written, true) => "would be written",
            (FileOutcome::Deleted, false) => "removed (kioku created it)",
            (FileOutcome::Deleted, true) => "would be removed (kioku created it)",
        };
        if *outcome != FileOutcome::Unchanged {
            self.changed = true;
        }
        self.push(format!("{what}: {} ({verb})", path.display()));
    }
}

// ---------------------------------------------------------------------------------------
// Paths (M2 §8.2)
// ---------------------------------------------------------------------------------------

/// Hook file of an agent (user level, or the project's with `project`).
pub fn hooks_path(agent: Agent, ctx: &InstallCtx, project: bool) -> PathBuf {
    match (agent, project) {
        (Agent::ClaudeCode, false) => ctx.home.join(".claude").join("settings.json"),
        // M1 keeps the cwd (not the git root) for Claude Code's project settings.
        (Agent::ClaudeCode, true) => ctx.cwd.join(".claude").join("settings.json"),
        (Agent::Codex, false) => ctx.codex_home.join("hooks.json"),
        (Agent::Codex, true) => ctx.project_root().join(".codex").join("hooks.json"),
        (Agent::Cursor, false) => ctx.home.join(".cursor").join("hooks.json"),
        (Agent::Cursor, true) => ctx.project_root().join(".cursor").join("hooks.json"),
        (Agent::GeminiCli, false) => gemini_user_settings(ctx),
        (Agent::GeminiCli, true) => ctx.project_root().join(".gemini").join("settings.json"),
        (Agent::Antigravity, false) => antigravity_config_dir(ctx).join("hooks.json"),
        (Agent::Antigravity, true) => ctx.project_root().join(".agents").join("hooks.json"),
    }
}

/// MCP config file of an agent (always user level — the token never goes into a repository).
pub fn mcp_path(agent: Agent, ctx: &InstallCtx) -> PathBuf {
    match agent {
        Agent::ClaudeCode => ctx.home.join(".claude.json"),
        Agent::Codex => ctx.codex_home.join("config.toml"),
        Agent::Cursor => ctx.home.join(".cursor").join("mcp.json"),
        Agent::GeminiCli => gemini_user_settings(ctx),
        Agent::Antigravity => antigravity_config_dir(ctx).join("mcp_config.json"),
    }
}

/// `~/.gemini/config`: Antigravity CLI's user hooks, MCP servers and rules (M2.1 §4).
fn antigravity_config_dir(ctx: &InstallCtx) -> PathBuf {
    ctx.home.join(".gemini").join("config")
}

fn gemini_user_settings(ctx: &InstallCtx) -> PathBuf {
    ctx.home.join(".gemini").join("settings.json")
}

/// Paths whose existence means the agent is installed on this machine (M2 §8.3, M2.1 §4.4);
/// the first is the one named when none exists. `~/.gemini` itself is shared by Gemini CLI
/// and Antigravity, so neither uses it.
pub fn detection_paths(agent: Agent, ctx: &InstallCtx) -> Vec<PathBuf> {
    match agent {
        Agent::ClaudeCode => vec![ctx.home.join(".claude")],
        Agent::Codex => vec![ctx.codex_home.clone()],
        Agent::Cursor => vec![ctx.home.join(".cursor")],
        Agent::GeminiCli => vec![ctx.home.join(".gemini").join("tmp")],
        Agent::Antigravity => vec![
            ctx.home.join(".gemini").join("antigravity-cli"),
            ctx.home.join(".local").join("bin").join("agy"),
        ],
    }
}

/// True when the agent is installed on this machine (any [`detection_paths`] entry exists).
pub fn is_detected(agent: Agent, ctx: &InstallCtx) -> bool {
    detection_paths(agent, ctx).iter().any(|p| p.exists())
}

/// Gemini's context file name: `context.fileName` (string or array) when it does not include
/// `GEMINI.md` — its first entry — else `GEMINI.md` (M2 §6.6). Project settings win.
fn gemini_context_file(ctx: &InstallCtx, project: bool) -> String {
    let from = |path: PathBuf| -> Option<String> {
        let v = read_settings(&path).ok()??;
        let names: Vec<String> = match v.get("context")?.get("fileName")? {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => return None,
        };
        let names: Vec<String> = names.into_iter().filter(|n| !n.is_empty()).collect();
        if names.is_empty() || names.iter().any(|n| n == "GEMINI.md") {
            Some("GEMINI.md".to_string())
        } else {
            Some(names[0].clone())
        }
    };
    let project_name = if project {
        from(hooks_path(Agent::GeminiCli, ctx, true))
    } else {
        None
    };
    project_name
        .or_else(|| from(gemini_user_settings(ctx)))
        .unwrap_or_else(|| "GEMINI.md".to_string())
}

/// Instruction files of an agent: where `install` writes (first) and every file `uninstall`
/// cleans. Cursor has no user-level rule file (User Rules are UI-only).
pub fn instruction_files(agent: Agent, ctx: &InstallCtx, project: bool) -> Vec<PathBuf> {
    match (agent, project) {
        (Agent::ClaudeCode, false) => vec![ctx.home.join(".claude").join("CLAUDE.md")],
        (Agent::ClaudeCode, true) => vec![ctx.cwd.join("CLAUDE.md")],
        (Agent::Codex, false) => {
            let over = ctx.codex_home.join("AGENTS.override.md");
            let plain = ctx.codex_home.join("AGENTS.md");
            let override_active = std::fs::read_to_string(&over)
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false);
            if override_active {
                vec![over, plain]
            } else {
                vec![plain, over]
            }
        }
        (Agent::Codex, true) => vec![ctx.project_root().join("AGENTS.md")],
        (Agent::Cursor, false) => Vec::new(),
        (Agent::Cursor, true) => vec![
            ctx.project_root()
                .join(".cursor")
                .join("rules")
                .join("kioku.mdc"),
        ],
        (Agent::GeminiCli, false) => {
            vec![
                ctx.home
                    .join(".gemini")
                    .join(gemini_context_file(ctx, false)),
            ]
        }
        (Agent::GeminiCli, true) => vec![ctx.project_root().join(gemini_context_file(ctx, true))],
        // Global rules (M2.1 §4.3): the same file as Gemini CLI's default — shared-file rule.
        (Agent::Antigravity, false) => vec![ctx.home.join(".gemini").join("GEMINI.md")],
        (Agent::Antigravity, true) => vec![ctx.project_root().join("AGENTS.md")],
    }
}

/// True when `install` writes the instruction snippet for `agent` with these options (M2 §7).
pub fn wants_instructions(agent: Agent, opts: &InstallOptions) -> bool {
    match opts.instructions {
        Instructions::Skip => false,
        Instructions::Force => true,
        Instructions::Default => match agent {
            Agent::ClaudeCode => false,
            Agent::Cursor => opts.project,
            Agent::Codex | Agent::GeminiCli | Agent::Antigravity => true,
        },
    }
}

// ---------------------------------------------------------------------------------------
// Hook shapes (M2 §4.2, §5.2, §6.2)
// ---------------------------------------------------------------------------------------

fn secs(agent: Agent, event: HookEventKind) -> u64 {
    registered_timeout_ms(agent, event) / 1000
}

/// Codex `hooks.json` groups (M2 §4.2).
pub fn codex_specs(bin: &str) -> Vec<HookSpec> {
    let a = Agent::Codex;
    ALL_EVENTS
        .iter()
        .map(|&event| {
            let mut handler = Map::new();
            handler.insert("type".into(), json!("command"));
            handler.insert("command".into(), json!(agent_hook_command(a, bin, event)));
            handler.insert("timeout".into(), json!(secs(a, event)));
            if event == HookEventKind::SessionStart {
                handler.insert("statusMessage".into(), json!("kioku: loading handoff"));
                handler.insert("additionalContextLimit".into(), json!(0));
            }
            let mut group = Map::new();
            if event == HookEventKind::PostToolUse {
                group.insert("matcher".into(), json!("^(Bash|apply_patch)$"));
            }
            group.insert("hooks".into(), json!([handler]));
            HookSpec {
                key: event.claude_code_name().to_string(),
                entry: Value::Object(group),
            }
        })
        .collect()
}

/// Cursor `hooks.json` handlers (M2 §5.2): native event key, neutral event, matcher.
pub const CURSOR_EVENTS: [(&str, HookEventKind, Option<&str>); 8] = [
    ("sessionStart", HookEventKind::SessionStart, None),
    ("beforeSubmitPrompt", HookEventKind::UserPromptSubmit, None),
    (
        "postToolUse",
        HookEventKind::PostToolUse,
        Some("Shell|Read"),
    ),
    (
        "postToolUseFailure",
        HookEventKind::PostToolUse,
        Some("Shell"),
    ),
    ("afterFileEdit", HookEventKind::PostToolUse, None),
    ("preCompact", HookEventKind::PreCompact, None),
    ("stop", HookEventKind::Stop, None),
    ("sessionEnd", HookEventKind::SessionEnd, None),
];

/// Cursor `hooks.json` handlers (M2 §5.2).
pub fn cursor_specs(bin: &str) -> Vec<HookSpec> {
    let a = Agent::Cursor;
    CURSOR_EVENTS
        .iter()
        .map(|&(key, event, matcher)| {
            let mut handler = Map::new();
            handler.insert("command".into(), json!(agent_hook_command(a, bin, event)));
            if let Some(m) = matcher {
                handler.insert("matcher".into(), json!(m));
            }
            handler.insert("timeout".into(), json!(secs(a, event)));
            HookSpec {
                key: key.to_string(),
                entry: Value::Object(handler),
            }
        })
        .collect()
}

/// Gemini CLI hook groups (M2 §6.2): native event key, neutral event, hook name.
pub const GEMINI_EVENTS: [(&str, HookEventKind, &str); 6] = [
    (
        "SessionStart",
        HookEventKind::SessionStart,
        "kioku-session-start",
    ),
    (
        "BeforeAgent",
        HookEventKind::UserPromptSubmit,
        "kioku-user-prompt",
    ),
    ("AfterTool", HookEventKind::PostToolUse, "kioku-post-tool"),
    (
        "PreCompress",
        HookEventKind::PreCompact,
        "kioku-pre-compact",
    ),
    ("AfterAgent", HookEventKind::Stop, "kioku-stop"),
    ("SessionEnd", HookEventKind::SessionEnd, "kioku-session-end"),
];

/// Gemini CLI `settings.json` hook groups (M2 §6.2); timeouts in milliseconds.
pub fn gemini_specs(bin: &str) -> Vec<HookSpec> {
    let a = Agent::GeminiCli;
    GEMINI_EVENTS
        .iter()
        .map(|&(key, event, name)| {
            let handler = json!({
                "name": name,
                "type": "command",
                "command": agent_hook_command(a, bin, event),
                "timeout": registered_timeout_ms(a, event),
            });
            let mut group = Map::new();
            if event == HookEventKind::PostToolUse {
                group.insert(
                    "matcher".into(),
                    json!("run_shell_command|write_file|replace|read_file"),
                );
            }
            group.insert("hooks".into(), json!([handler]));
            HookSpec {
                key: key.to_string(),
                entry: Value::Object(group),
            }
        })
        .collect()
}

/// Antigravity CLI hook group kioku owns in `hooks.json` (M2.1 §4.1).
pub const ANTIGRAVITY_GROUP: &str = "kioku";

/// Antigravity CLI events (M2.1 §3.2): native key, neutral event, tool matcher (tool events
/// take a `{matcher, hooks}` group, lifecycle events a bare handler).
pub const ANTIGRAVITY_EVENTS: [(&str, HookEventKind, Option<&str>); 4] = [
    ("SessionStart", HookEventKind::SessionStart, None),
    ("PreInvocation", HookEventKind::UserPromptSubmit, None),
    ("PostToolUse", HookEventKind::PostToolUse, Some("*")),
    ("Stop", HookEventKind::Stop, None),
];

/// Antigravity CLI handlers of the `kioku` group (M2.1 §4.1); timeouts in seconds.
pub fn antigravity_specs(bin: &str) -> Vec<HookSpec> {
    let a = Agent::Antigravity;
    ANTIGRAVITY_EVENTS
        .iter()
        .map(|&(key, event, matcher)| {
            let handler = json!({
                "type": "command",
                "command": agent_hook_command(a, bin, event),
                "timeout": secs(a, event),
            });
            let entry = match matcher {
                Some(m) => json!({ "matcher": m, "hooks": [handler] }),
                None => handler,
            };
            HookSpec {
                key: key.to_string(),
                entry,
            }
        })
        .collect()
}

/// Antigravity format: `settings` with the `kioku` group replaced by ours (in place when it
/// exists); foreign groups are untouched.
fn merge_group(settings: &Value, specs: &[HookSpec]) -> anyhow::Result<Value> {
    let mut out = settings.clone();
    let root = out
        .as_object_mut()
        .context("the hook file is not a JSON object")?;
    let group: Map<String, Value> = specs
        .iter()
        .map(|s| (s.key.clone(), json!([s.entry])))
        .collect();
    root.insert(ANTIGRAVITY_GROUP.into(), Value::Object(group));
    Ok(out)
}

/// Antigravity format: `settings` without the `kioku` group and the number of kioku handlers
/// it held (at least 1 when the group existed).
fn remove_group(settings: &Value) -> anyhow::Result<(Value, usize)> {
    let mut out = settings.clone();
    let root = out
        .as_object_mut()
        .context("the hook file is not a JSON object")?;
    let Some(group) = root.shift_remove(ANTIGRAVITY_GROUP) else {
        return Ok((settings.clone(), 0));
    };
    let handlers = group
        .as_object()
        .map(|g| g.keys().map(|k| hook_commands(&group, k).len()).sum())
        .unwrap_or(0);
    Ok((out, handlers.max(1)))
}

/// kioku hook commands registered under event `key` of a hook map (`hooks` object, or an
/// Antigravity group): bare handlers and handlers inside `{matcher, hooks}` groups alike.
pub fn hook_commands(map: &Value, key: &str) -> Vec<String> {
    let Some(list) = map.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .flat_map(|item| match item.get("hooks").and_then(Value::as_array) {
            Some(inner) => inner.iter().collect::<Vec<_>>(),
            None => vec![item],
        })
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .filter(|c| crate::install::is_kioku_command(c))
        .map(str::to_string)
        .collect()
}

/// The object holding an agent's event lists: `hooks`, or Antigravity's `kioku` group.
pub fn hooks_map(agent: Agent, settings: &Value) -> Option<&Value> {
    let key = if agent == Agent::Antigravity {
        ANTIGRAVITY_GROUP
    } else {
        "hooks"
    };
    settings.get(key)
}

/// The hook registrations `install` writes for `agent` (one per native event key).
pub fn hook_specs(agent: Agent, bin: &str) -> Vec<HookSpec> {
    match agent {
        Agent::ClaudeCode => claude_specs(bin),
        Agent::Codex => codex_specs(bin),
        Agent::Cursor => cursor_specs(bin),
        Agent::GeminiCli => gemini_specs(bin),
        Agent::Antigravity => antigravity_specs(bin),
    }
}

/// True for Cursor's flat hook format (no matcher groups).
pub fn is_flat(agent: Agent) -> bool {
    agent == Agent::Cursor
}

/// Our hooks merged into `settings` in the agent's format.
pub fn merge_agent_hooks(agent: Agent, settings: &Value, bin: &str) -> anyhow::Result<Value> {
    if agent == Agent::Antigravity {
        merge_group(settings, &hook_specs(agent, bin))
    } else if is_flat(agent) {
        merge_flat(settings, &hook_specs(agent, bin))
    } else {
        merge_nested(settings, &hook_specs(agent, bin))
    }
}

/// `settings` without our hooks, in the agent's format, and the number removed.
pub fn remove_agent_hooks(agent: Agent, settings: &Value) -> anyhow::Result<(Value, usize)> {
    if agent == Agent::Antigravity {
        remove_group(settings)
    } else if is_flat(agent) {
        remove_flat(settings)
    } else {
        remove_hooks(settings)
    }
}

/// The hooks to add by hand when the file cannot be edited.
fn hooks_snippet(agent: Agent, bin: &str) -> String {
    let v = merge_agent_hooks(agent, &json!({}), bin).unwrap_or_default();
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// MCP entries
// ---------------------------------------------------------------------------------------

fn bearer(ctx: &InstallCtx) -> Option<Value> {
    ctx.token()
        .map(|t| json!({ "Authorization": format!("Bearer {t}") }))
}

/// Cursor `mcpServers.kioku` (M2 §5.5): `{url, headers}`, no `type`.
pub fn cursor_mcp_entry(ctx: &InstallCtx) -> Value {
    let mut v = json!({ "url": mcp_url(&ctx.client) });
    if let Some(h) = bearer(ctx) {
        v["headers"] = h;
    }
    v
}

/// Antigravity CLI `mcpServers.kioku` (M2.1 §4.2): `{serverUrl, headers}` — agy rejects
/// `url` / `httpUrl`.
pub fn antigravity_mcp_entry(ctx: &InstallCtx) -> Value {
    let mut v = json!({ "serverUrl": mcp_url(&ctx.client) });
    if let Some(h) = bearer(ctx) {
        v["headers"] = h;
    }
    v
}

/// The `kioku mcp` stdio bridge entry (M2 §20.2): `{command, args: ["mcp"]}` — no URL,
/// no token; Claude Code adds `type: "stdio"`, Gemini CLI its `timeout` (+ `trust`).
pub fn stdio_mcp_entry(agent: Agent, ctx: &InstallCtx, trust: bool) -> Value {
    let mut v = Map::new();
    if agent == Agent::ClaudeCode {
        v.insert("type".into(), json!("stdio"));
    }
    v.insert("command".into(), json!(ctx.bin));
    v.insert("args".into(), json!(["mcp"]));
    if agent == Agent::GeminiCli {
        v.insert("timeout".into(), json!(30_000));
        if trust {
            v.insert("trust".into(), json!(true));
        }
    }
    Value::Object(v)
}

/// The agent's `mcpServers.kioku` entry: the stdio bridge, or with `--mcp-http` the v0.3
/// URL + token form. (Codex's TOML block is [`block::CodexMcp`].)
pub fn mcp_entry(agent: Agent, ctx: &InstallCtx, opts: &InstallOptions) -> Value {
    if !opts.mcp_http {
        return stdio_mcp_entry(agent, ctx, opts.trust_mcp);
    }
    match agent {
        Agent::ClaudeCode | Agent::Codex => mcp_server_entry(&ctx.client),
        Agent::Cursor => cursor_mcp_entry(ctx),
        Agent::GeminiCli => gemini_mcp_entry(ctx, opts.trust_mcp),
        Agent::Antigravity => antigravity_mcp_entry(ctx),
    }
}

/// Gemini CLI `mcpServers.kioku` (M2 §6.5): `{httpUrl, headers, timeout}` (+ `trust`).
pub fn gemini_mcp_entry(ctx: &InstallCtx, trust: bool) -> Value {
    let mut v = json!({ "httpUrl": mcp_url(&ctx.client) });
    if let Some(h) = bearer(ctx) {
        v["headers"] = h;
    }
    v["timeout"] = json!(10_000);
    if trust {
        v["trust"] = json!(true);
    }
    v
}

// ---------------------------------------------------------------------------------------
// Install / uninstall
// ---------------------------------------------------------------------------------------

/// Edits a hook file: our hooks in, plus (Gemini user level) the MCP entry in the same write.
fn install_hook_file(
    agent: Agent,
    path: &Path,
    ctx: &InstallCtx,
    mcp_entry: Option<&Value>,
    dry_run: bool,
) -> anyhow::Result<SettingsChange> {
    let manual = |e: anyhow::Error| {
        let mut msg = format!(
            "{e:#}\nAdd these hooks to {} yourself:\n{}",
            path.display(),
            hooks_snippet(agent, &ctx.bin)
        );
        if let Some(entry) = mcp_entry {
            msg.push_str(&format!(
                "\nand this MCP server:\n{}",
                super::mcp_entry_snippet(entry)
            ));
        }
        anyhow::anyhow!(msg)
    };
    let current = read_settings(path).map_err(manual)?;
    let existed = current.is_some();
    let before = current.unwrap_or_else(|| Value::Object(Map::new()));
    let mut after = merge_agent_hooks(agent, &before, &ctx.bin)
        .with_context(|| format!("{}: not touching it", path.display()))
        .map_err(manual)?;
    if let Some(entry) = mcp_entry {
        after = merge_mcp_entry(&after, entry)
            .with_context(|| format!("{}: not touching it", path.display()))
            .map_err(manual)?;
    }
    let changed = !existed || after != before;
    let written = if changed && !dry_run {
        write_settings(path, existed, &after, mcp_entry.is_some())?
    } else {
        Written::default()
    };
    Ok(SettingsChange {
        path: path.to_path_buf(),
        changed,
        backup: written.backup,
        removed: 0,
        made_private: written.made_private,
    })
}

fn report_hooks(r: &mut AgentReport, c: &SettingsChange, dry_run: bool, what: &str) {
    r.changed |= c.changed;
    let verb = match (c.changed, dry_run) {
        (false, _) => "unchanged",
        (true, false) => "installed",
        (true, true) => "would be installed",
    };
    r.push(format!("{what}: {} ({verb})", c.path.display()));
    if let Some(b) = &c.backup {
        r.push(format!("  backup of the original: {}", b.display()));
    }
    if c.made_private {
        r.push(format!("  {}", super::made_private_note(&c.path)));
    }
}

fn install_instructions(
    agent: Agent,
    ctx: &InstallCtx,
    opts: &InstallOptions,
    r: &mut AgentReport,
) -> anyhow::Result<()> {
    if !wants_instructions(agent, opts) {
        return Ok(());
    }
    let Some(path) = instruction_files(agent, ctx, opts.project)
        .into_iter()
        .next()
    else {
        if agent == Agent::Cursor {
            r.push("instructions: Cursor has no user-level rule file; use `kioku install cursor --project` in a repository");
        }
        return Ok(());
    };
    let project_id = if opts.project {
        kioku_core::identify(&ctx.project_root()).ok().map(|p| p.id)
    } else {
        None
    };
    let body = block::instructions_body(ctx.client.lang, project_id.as_deref());
    let outcome = if agent == Agent::Cursor {
        let before = block::read_text(&path)?;
        if let Some(t) = before.as_deref()
            && !block::is_our_mdc(t)
        {
            r.push(format!(
                "instructions: {} exists and was not written by kioku; not touching it",
                path.display()
            ));
            return Ok(());
        }
        block::save_text(
            &path,
            before.as_deref(),
            &block::mdc_content(&body),
            false,
            opts.dry_run,
        )?
    } else {
        block::install_md_block(&path, &body, opts.dry_run)?
    };
    r.file("instructions", &path, &outcome, opts.dry_run);
    Ok(())
}

/// Installs kioku for one agent (hooks, MCP, instructions). An error means the hook file could
/// not be edited; MCP / instruction problems are reported as lines.
pub fn install_agent(
    agent: Agent,
    ctx: &InstallCtx,
    opts: &InstallOptions,
) -> anyhow::Result<AgentReport> {
    let mut r = AgentReport::new(agent);
    let dry = opts.dry_run;
    if opts.project {
        ctx.check_project_dir(agent)?;
    }
    if agent == Agent::GeminiCli && ctx.bin.contains('$') {
        anyhow::bail!(
            "the kioku binary path {} contains `$`, which Gemini CLI expands in settings.json; install kioku to a path without `$`",
            ctx.bin
        );
    }
    let hooks = hooks_path(agent, ctx, opts.project);
    match agent {
        Agent::GeminiCli if !opts.project => {
            // Hooks and MCP share ~/.gemini/settings.json: one edit, one backup.
            let entry = mcp_entry(agent, ctx, opts);
            let c = install_hook_file(agent, &hooks, ctx, Some(&entry), dry)?;
            report_hooks(&mut r, &c, dry, "hooks + MCP server `kioku`");
        }
        _ => {
            let c = install_hook_file(agent, &hooks, ctx, None, dry)?;
            report_hooks(&mut r, &c, dry, "hooks");
        }
    }
    match agent {
        Agent::ClaudeCode => {
            let entry = mcp_entry(agent, ctx, opts);
            r.mcp(register_mcp_entry(
                &mcp_path(agent, ctx),
                &entry,
                "Claude Code",
                dry,
            ));
        }
        Agent::Cursor => {
            r.mcp(register_mcp_entry(
                &mcp_path(agent, ctx),
                &mcp_entry(agent, ctx, opts),
                "Cursor",
                dry,
            ));
        }
        Agent::GeminiCli if opts.project => {
            let entry = mcp_entry(agent, ctx, opts);
            r.mcp(register_mcp_entry(
                &mcp_path(agent, ctx),
                &entry,
                "Gemini CLI",
                dry,
            ));
        }
        Agent::GeminiCli => {}
        Agent::Antigravity => {
            r.mcp(register_mcp_entry(
                &mcp_path(agent, ctx),
                &mcp_entry(agent, ctx, opts),
                "Antigravity CLI",
                dry,
            ));
        }
        Agent::Codex => {
            let path = mcp_path(agent, ctx);
            let token = ctx.token();
            let url = mcp_url(&ctx.client);
            let mcp = if opts.mcp_http {
                block::CodexMcp::Http(&url, token.as_deref())
            } else {
                block::CodexMcp::Stdio(&ctx.bin)
            };
            let c = block::install_codex_config(&path, mcp, opts.enable_hooks_feature, dry)?;
            r.file("MCP server `kioku`", &path, &c.outcome, dry);
            r.lines.extend(c.notes);
            let layer = if opts.project {
                ctx.project_root().join(".codex").join("config.toml")
            } else {
                path
            };
            if block::read_codex_config(&layer).is_some_and(|t| block::codex_has_inline_hooks(&t)) {
                r.push(format!(
                    "info: {} also defines inline [hooks]; Codex warns at startup that this layer has both forms (harmless)",
                    layer.display()
                ));
            }
        }
    }
    install_instructions(agent, ctx, opts, &mut r)?;
    Ok(r)
}

/// Removes kioku from one agent: our hook entries, our MCP entry / block, our instruction
/// block or rule file. Foreign content stays.
pub fn uninstall_agent(
    agent: Agent,
    ctx: &InstallCtx,
    project: bool,
    dry_run: bool,
) -> anyhow::Result<AgentReport> {
    let mut r = AgentReport::new(agent);
    let hooks = hooks_path(agent, ctx, project);
    let gemini_shared = agent == Agent::GeminiCli && !project;
    if let Some(before) = read_settings(&hooks)? {
        let (mut after, removed) = remove_agent_hooks(agent, &before)
            .with_context(|| format!("{}: not touching it", hooks.display()))?;
        let mut mcp_removed = false;
        if gemini_shared {
            let (v, found) = remove_mcp_server(&after);
            after = v;
            mcp_removed = found;
        }
        if removed > 0 || mcp_removed {
            let deleted = !dry_run && rewrite_json_after_removal(&hooks, &after)?;
            r.changed = true;
            let verb = if dry_run { "would remove" } else { "removed" };
            r.push(format!(
                "hooks: {verb} {removed} kioku entries from {}{}",
                hooks.display(),
                if deleted {
                    " (file removed: kioku created it)"
                } else {
                    ""
                }
            ));
            if mcp_removed {
                r.push(format!(
                    "MCP server `kioku`: {verb} from {}",
                    hooks.display()
                ));
            }
        } else {
            r.push(format!("hooks: none in {}", hooks.display()));
        }
    } else {
        r.push(format!("hooks: none ({} does not exist)", hooks.display()));
    }
    match agent {
        Agent::Codex => {
            let path = mcp_path(agent, ctx);
            let outcome = block::uninstall_codex_config(&path, dry_run)?;
            r.file("MCP server `kioku`", &path, &outcome, dry_run);
        }
        Agent::GeminiCli if !project => {}
        _ => r.mcp(unregister_mcp_entry(&mcp_path(agent, ctx), dry_run)),
    }
    for path in instruction_files(agent, ctx, project) {
        // Gemini's `context.fileName` can point at the same file another agent uses (e.g.
        // AGENTS.md with Codex): keep the shared block while that agent is still installed.
        if let Some(other) = ALL_AGENTS.into_iter().find(|&o| {
            o != agent
                && instruction_files(o, ctx, project).contains(&path)
                && agent_hooks_installed(o, ctx, project)
        }) {
            if block::read_text(&path)?.is_some_and(|t| t.contains(block::MD_MARKERS.end)) {
                r.push(format!(
                    "instructions: {} kept ({} still uses it)",
                    path.display(),
                    other.as_str()
                ));
            }
            continue;
        }
        let outcome = if agent == Agent::Cursor {
            match block::read_text(&path)? {
                Some(t) if block::is_our_mdc(&t) => {
                    if !dry_run {
                        std::fs::remove_file(&path)
                            .with_context(|| format!("removing {}", path.display()))?;
                    }
                    FileOutcome::Deleted
                }
                _ => FileOutcome::Unchanged,
            }
        } else {
            block::uninstall_md_block(&path, dry_run)?
        };
        if outcome != FileOutcome::Unchanged {
            r.file("instructions", &path, &outcome, dry_run);
        }
    }
    Ok(r)
}

/// True when `agent`'s hook file (user or project level) holds kioku hook entries.
pub fn agent_hooks_installed(agent: Agent, ctx: &InstallCtx, project: bool) -> bool {
    read_settings(&hooks_path(agent, ctx, project))
        .ok()
        .flatten()
        .and_then(|v| remove_agent_hooks(agent, &v).ok())
        .is_some_and(|(_, n)| n > 0)
}

/// Notes printed after installing an agent (Codex trust, project hook files).
pub fn post_install_notes(
    agent: Agent,
    opts: &InstallOptions,
    report: &AgentReport,
) -> Vec<String> {
    let mut out = Vec::new();
    if agent == Agent::Codex && report.changed {
        out.push("Codex: open Codex and run /hooks once to trust kioku's hooks (kioku never writes trust state; until then the hooks do not run)".to_string());
        if opts.project {
            out.push(
                "Codex: project hooks load only when this project is trusted in Codex".to_string(),
            );
        }
    }
    if agent == Agent::GeminiCli && opts.project && report.changed {
        out.push(
            "Gemini CLI: shows a one-time warning before running new project hooks".to_string(),
        );
    }
    if opts.project && agent != Agent::ClaudeCode && report.changed {
        out.push("note: project hook files contain an absolute, machine-specific binary path; do not commit them".to_string());
    }
    out
}

/// Warning when the binary lives somewhere hooks would break once it moves (M2 §8.1).
pub fn unstable_binary_warning(bin: &str) -> Option<String> {
    let path = Path::new(bin);
    let in_target = path.components().any(|c| c.as_os_str() == "target");
    let tmp = std::env::temp_dir();
    let in_tmp = path.starts_with(&tmp)
        || std::fs::canonicalize(&tmp).is_ok_and(|t| path.starts_with(t))
        || path.starts_with("/tmp");
    (in_target || in_tmp).then(|| {
        format!(
            "warning: {bin} is a build or temp location; hooks will break when it moves (install with install.sh or copy it to ~/.local/bin first)"
        )
    })
}

// ---------------------------------------------------------------------------------------
// install all / uninstall all (M2 §8.3)
// ---------------------------------------------------------------------------------------

/// Status of one agent in `install all` / `uninstall all`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllStatus {
    /// Something was written (install) or removed (uninstall).
    Changed(AgentReport),
    /// Nothing to do.
    Unchanged(AgentReport),
    /// Not detected (install only).
    NotDetected(PathBuf),
    /// Failed; other agents continue.
    Error(String),
}

/// Runs `install all`: every detected agent (optionally restricted to `only`), continuing past
/// failures.
pub fn install_all(
    ctx: &InstallCtx,
    opts: &InstallOptions,
    only: &[Agent],
) -> Vec<(Agent, AllStatus)> {
    ALL_AGENTS
        .iter()
        .filter(|a| only.is_empty() || only.contains(a))
        .map(|&agent| {
            let status = if !is_detected(agent, ctx) {
                AllStatus::NotDetected(detection_paths(agent, ctx).swap_remove(0))
            } else {
                match install_agent(agent, ctx, opts) {
                    Ok(r) if r.changed => AllStatus::Changed(r),
                    Ok(r) => AllStatus::Unchanged(r),
                    Err(e) => AllStatus::Error(format!("{e:#}")),
                }
            };
            (agent, status)
        })
        .collect()
}

/// Runs `uninstall all`: every agent, detected or not.
pub fn uninstall_all(ctx: &InstallCtx, project: bool, dry_run: bool) -> Vec<(Agent, AllStatus)> {
    ALL_AGENTS
        .iter()
        .map(|&agent| {
            let status = match uninstall_agent(agent, ctx, project, dry_run) {
                Ok(r) if r.changed => AllStatus::Changed(r),
                Ok(r) => AllStatus::Unchanged(r),
                Err(e) => AllStatus::Error(format!("{e:#}")),
            };
            (agent, status)
        })
        .collect()
}

#[cfg(test)]
mod tests;
