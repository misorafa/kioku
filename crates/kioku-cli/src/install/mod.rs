//! `kioku install|uninstall <agent>` (M1 §8.5, M2 §8): idempotent merges of our hook
//! entries into each agent's hook file, the MCP server entry (edited directly — never via an
//! agent CLI, whose `--header` would put the token in `ps`) and the instruction snippet.
//!
//! This module owns the Claude Code installer and the file primitives every installer shares
//! (JSON hook merges for the nested and the flat hook format, backup-once writes that keep
//! the file mode); [`agents`] owns the per-agent file table and `install all`, [`block`] the
//! delimited managed blocks (Codex `config.toml`, AGENTS.md / GEMINI.md / CLAUDE.md).
//!
//! Our entries are recognized by the `kioku hook` command substring, so foreign hooks are
//! never touched and re-installing (even from a moved binary) leaves one entry per event.

pub mod agents;
pub mod block;

use std::path::{Path, PathBuf};

use anyhow::Context;
use kioku_core::ClientConfig;
use serde_json::{Map, Value, json};

use crate::event::{ALL_EVENTS, Agent, HookEventKind};

/// Suffix of the one-time backup written next to settings.json.
pub const BACKUP_SUFFIX: &str = ".kioku-bak";
/// SessionStart matcher (all sources).
pub const SESSION_START_MATCHER: &str = "startup|resume|clear|compact";
/// PostToolUse matcher (tools the digest understands).
pub const POST_TOOL_USE_MATCHER: &str = "Edit|Write|MultiEdit|NotebookEdit|Read|Bash";
/// Timeout (seconds) set on the SessionStart hook entry.
pub const SESSION_START_TIMEOUT_SECS: u64 = 10;
/// Name of the MCP server registered with Claude Code.
pub const MCP_NAME: &str = "kioku";

/// Settings file: `~/.claude/settings.json`, or `<cwd>/.claude/settings.json` with `--project`.
pub fn settings_path(project: bool) -> anyhow::Result<PathBuf> {
    let base = if project {
        std::env::current_dir().context("reading current directory")?
    } else {
        kioku_core::util::home_dir()
    };
    Ok(base.join(".claude").join("settings.json"))
}

/// Shell command registered for an event, e.g. `/usr/local/bin/kioku hook stop`.
pub fn hook_command(bin: &str, event: HookEventKind) -> String {
    format!("{} hook {}", shell_quote(bin), event.cli_name())
}

/// Hook command for an agent: Claude Code keeps M1's form, the others add `--agent <name>`
/// (M2 §8.1), e.g. `/usr/local/bin/kioku hook stop --agent codex`.
pub fn agent_hook_command(agent: Agent, bin: &str, event: HookEventKind) -> String {
    match agent {
        Agent::ClaudeCode => hook_command(bin, event),
        _ => format!("{} --agent {}", hook_command(bin, event), agent.as_str()),
    }
}

/// True when a hook command is one of ours (`…kioku hook …`, quoted or `.exe`).
pub fn is_kioku_command(cmd: &str) -> bool {
    [
        "kioku hook ",
        "kioku\" hook ",
        "kioku.exe hook ",
        "kioku.exe\" hook ",
    ]
    .iter()
    .any(|needle| cmd.contains(needle))
}

fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Our matcher group for one event.
fn our_group(bin: &str, event: HookEventKind) -> Value {
    let mut hook = Map::new();
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), json!(hook_command(bin, event)));
    if event == HookEventKind::SessionStart {
        hook.insert("timeout".into(), json!(SESSION_START_TIMEOUT_SECS));
    }
    let mut group = Map::new();
    match event {
        HookEventKind::SessionStart => {
            group.insert("matcher".into(), json!(SESSION_START_MATCHER));
        }
        HookEventKind::PostToolUse => {
            group.insert("matcher".into(), json!(POST_TOOL_USE_MATCHER));
        }
        _ => {}
    }
    group.insert("hooks".into(), Value::Array(vec![Value::Object(hook)]));
    Value::Object(group)
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(is_kioku_command)
}

/// Removes our hook entries from one event's group list. Returns the cleaned list, the
/// position our group occupied first (if any) and how many entries were removed.
fn strip_groups(groups: &[Value]) -> (Vec<Value>, Option<usize>, usize) {
    let mut out = Vec::new();
    let mut first = None;
    let mut removed = 0;
    for group in groups {
        let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
            out.push(group.clone());
            continue;
        };
        let kept: Vec<Value> = hooks.iter().filter(|h| !is_ours(h)).cloned().collect();
        if kept.len() == hooks.len() {
            out.push(group.clone());
            continue;
        }
        removed += hooks.len() - kept.len();
        first.get_or_insert(out.len());
        if !kept.is_empty() {
            let mut g = group.clone();
            g["hooks"] = Value::Array(kept);
            out.push(g);
        }
    }
    (out, first, removed)
}

/// One hook registration: the event key in the agent's hook config and the JSON inserted
/// under it — a matcher group (`{matcher?, hooks: [handler]}`) for the nested format of
/// Claude Code / Codex / Gemini CLI, a bare handler for Cursor's flat format.
#[derive(Clone, Debug, PartialEq)]
pub struct HookSpec {
    /// Event key (`SessionStart`, `sessionStart`, `AfterTool`, …).
    pub key: String,
    /// Group (nested format) or handler (flat format).
    pub entry: Value,
}

fn hooks_object(settings: &mut Value) -> anyhow::Result<&mut Map<String, Value>> {
    let root = settings
        .as_object_mut()
        .context("the hook file is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    hooks
        .as_object_mut()
        .context("the hook file's `hooks` is not an object")
}

fn event_list(hooks: &Map<String, Value>, key: &str) -> anyhow::Result<Vec<Value>> {
    match hooks.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => anyhow::bail!("hooks.{key} is not an array"),
    }
}

/// Nested format: returns `settings` with exactly our group per spec (foreign hooks kept; our
/// group takes the place of the first group that held one of ours).
pub fn merge_nested(settings: &Value, specs: &[HookSpec]) -> anyhow::Result<Value> {
    let mut out = settings.clone();
    let hooks = hooks_object(&mut out)?;
    for spec in specs {
        let existing = event_list(hooks, &spec.key)?;
        let (mut groups, first, _) = strip_groups(&existing);
        let at = first.unwrap_or(groups.len()).min(groups.len());
        groups.insert(at, spec.entry.clone());
        hooks.insert(spec.key.clone(), Value::Array(groups));
    }
    Ok(out)
}

/// Flat format (Cursor `hooks.json`): returns `settings` with exactly our handler per spec,
/// in place of our old one, and `"version": 1` (first key) when the file had no version.
pub fn merge_flat(settings: &Value, specs: &[HookSpec]) -> anyhow::Result<Value> {
    let mut out = settings.clone();
    let root = out
        .as_object_mut()
        .context("the hook file is not a JSON object")?;
    if !root.contains_key("version") {
        let mut with_version = Map::new();
        with_version.insert("version".into(), json!(1));
        with_version.extend(std::mem::take(root));
        *root = with_version;
    }
    let hooks = hooks_object(&mut out)?;
    for spec in specs {
        let existing = event_list(hooks, &spec.key)?;
        let first = existing.iter().position(is_ours);
        let mut kept: Vec<Value> = existing.into_iter().filter(|h| !is_ours(h)).collect();
        let at = first.unwrap_or(kept.len()).min(kept.len());
        kept.insert(at, spec.entry.clone());
        hooks.insert(spec.key.clone(), Value::Array(kept));
    }
    Ok(out)
}

/// Flat format: `settings` without our handlers and the number removed. Emptied event lists
/// and an emptied `hooks` object are removed; so is a `version` that is then all that is left.
pub fn remove_flat(settings: &Value) -> anyhow::Result<(Value, usize)> {
    let mut out = settings.clone();
    let Some(root) = out.as_object_mut() else {
        anyhow::bail!("the hook file is not a JSON object");
    };
    let Some(Value::Object(hooks)) = root.get_mut("hooks") else {
        return Ok((settings.clone(), 0));
    };
    let mut total = 0;
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        let Some(Value::Array(list)) = hooks.get(&key) else {
            continue;
        };
        let kept: Vec<Value> = list.iter().filter(|h| !is_ours(h)).cloned().collect();
        let removed = list.len() - kept.len();
        if removed == 0 {
            continue;
        }
        total += removed;
        if kept.is_empty() {
            hooks.shift_remove(&key);
        } else {
            hooks.insert(key, Value::Array(kept));
        }
    }
    if total > 0 && hooks.is_empty() {
        root.shift_remove("hooks");
        if root.len() == 1 && root.get("version") == Some(&json!(1)) {
            root.shift_remove("version");
        }
    }
    Ok((out, total))
}

/// Claude Code's hook specs (M1 §8.5).
pub fn claude_specs(bin: &str) -> Vec<HookSpec> {
    ALL_EVENTS
        .iter()
        .map(|&event| HookSpec {
            key: event.claude_code_name().to_string(),
            entry: our_group(bin, event),
        })
        .collect()
}

/// Returns `settings` with exactly one kioku entry per Claude Code event (foreign hooks kept).
pub fn merge_hooks(settings: &Value, bin: &str) -> anyhow::Result<Value> {
    merge_nested(settings, &claude_specs(bin))
}

/// Returns `settings` without any kioku hook entry and the number of entries removed.
/// Event lists (and the `hooks` object) that only held our entries are removed too.
pub fn remove_hooks(settings: &Value) -> anyhow::Result<(Value, usize)> {
    let mut out = settings.clone();
    let Some(root) = out.as_object_mut() else {
        anyhow::bail!("the hook file is not a JSON object");
    };
    let Some(Value::Object(hooks)) = root.get_mut("hooks") else {
        return Ok((settings.clone(), 0));
    };
    let mut total = 0;
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        let Some(Value::Array(groups)) = hooks.get(&key) else {
            continue;
        };
        let (groups, _, removed) = strip_groups(groups);
        if removed == 0 {
            continue;
        }
        total += removed;
        if groups.is_empty() {
            hooks.shift_remove(&key);
        } else {
            hooks.insert(key, Value::Array(groups));
        }
    }
    if total > 0 && hooks.is_empty() {
        root.shift_remove("hooks");
    }
    Ok((out, total))
}

/// Result of editing a settings file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsChange {
    /// The settings file.
    pub path: PathBuf,
    /// True when the file was written.
    pub changed: bool,
    /// Backup written by this call (only before the first modification).
    pub backup: Option<PathBuf>,
    /// Hook entries removed (uninstall).
    pub removed: usize,
    /// True when this write added the token and tightened the file to 0600.
    pub made_private: bool,
}

/// What one [`write_text`] did besides writing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// Backup written by this call (only before the first modification).
    pub backup: Option<PathBuf>,
    /// True when the write added a bearer token to an existing file that others could read,
    /// and its mode was set to 0600 (unix).
    pub made_private: bool,
}

/// The one line reported when [`Written::made_private`] is set.
pub fn made_private_note(path: &Path) -> String {
    format!(
        "{} now holds the kioku token: its permissions were set to 0600",
        path.display()
    )
}

/// The file a write to `path` really changes: a symlink (e.g. into a dotfiles repository)
/// is followed so the target is updated in place instead of being replaced by a regular file.
pub fn resolve_target(path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return path.to_path_buf();
    }
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        // Dangling link: write where it points (relative to the link's directory).
        match std::fs::read_link(path) {
            Ok(target) if target.is_absolute() => target,
            Ok(target) => path
                .parent()
                .map_or_else(|| target.clone(), |p| p.join(&target)),
            Err(_) => path.to_path_buf(),
        }
    })
}

/// Reads a JSON file: `None` when missing, `{}` when empty, an error when not valid JSON.
pub fn read_settings(path: &Path) -> anyhow::Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Some(Value::Object(Map::new())));
    }
    let v: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON; not touching it", path.display()))?;
    Ok(Some(v))
}

/// `<file>.kioku-bak` next to `path`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

/// Writes `value` as pretty JSON through [`write_text`].
fn write_settings(
    path: &Path,
    existed: bool,
    value: &Value,
    secret: bool,
) -> anyhow::Result<Written> {
    let text = serde_json::to_string_pretty(value).context("serializing settings")? + "\n";
    write_text(path, existed, &text, secret)
}

/// Writes `text` to `path`, backing the file up once first (only if it existed; an existing
/// backup is never overwritten). A symlinked `path` is followed ([`resolve_target`]). The
/// write goes through a temp file + rename that keeps the original's permissions — except
/// that a write adding a bearer token (`secret`) to a group/world-readable file makes it
/// 0600; a new file is 0600 when it holds the token, else 0644. Parent directories are
/// created as needed.
pub fn write_text(path: &Path, existed: bool, text: &str, secret: bool) -> anyhow::Result<Written> {
    write_text_inner(path, existed, text, secret, true)
}

/// [`write_text`] for an uninstall: never writes a backup (install already backed up a
/// pre-existing file; a file without one is kioku's own).
pub fn rewrite_text(path: &Path, text: &str) -> anyhow::Result<()> {
    write_text_inner(path, true, text, false, false).map(|_| ())
}

/// Writes a file kioku owns entirely (a service definition): temp file + rename, no backup,
/// a new file 0644, an existing one keeps its mode.
pub fn write_definition_file(path: &Path, existed: bool, text: &str) -> anyhow::Result<()> {
    write_text_inner(path, existed, text, false, false).map(|_| ())
}

/// True when a JSON value holds nothing but what kioku may leave behind (`{}`, or an empty
/// `mcpServers` object).
pub fn is_blank_json(v: &Value) -> bool {
    v.as_object().is_some_and(|m| {
        m.iter()
            .all(|(k, v)| k == "mcpServers" && v.as_object().is_some_and(Map::is_empty))
    })
}

/// Writes a JSON file after removing our entries: without a backup, and a file kioku created
/// (no `.kioku-bak`) that is now blank is deleted instead. Returns true when deleted.
pub fn rewrite_json_after_removal(path: &Path, after: &Value) -> anyhow::Result<bool> {
    if is_blank_json(after) && !backup_path(&resolve_target(path)).exists() {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        return Ok(true);
    }
    let text = serde_json::to_string_pretty(after).context("serializing settings")? + "\n";
    rewrite_text(path, &text)?;
    Ok(false)
}

fn write_text_inner(
    path: &Path,
    existed: bool,
    text: &str,
    secret: bool,
    make_backup: bool,
) -> anyhow::Result<Written> {
    let target = resolve_target(path);
    let path = target.as_path();
    let mut backup = None;
    let adds_token = secret
        && existed
        && std::fs::read_to_string(path)
            .is_ok_and(|old| text.matches("Bearer ").count() > old.matches("Bearer ").count());
    #[allow(unused_mut)] // only set on unix
    let mut made_private = false;
    let bak = backup_path(path);
    if make_backup && existed && !bak.exists() {
        std::fs::copy(path, &bak).with_context(|| format!("backing up to {}", bak.display()))?;
        backup = Some(bak);
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.kioku-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    kioku_core::util::write_private_file(&tmp, text)
        .with_context(|| format!("writing {}", tmp.display()))?;
    if existed && let Ok(meta) = std::fs::metadata(path) {
        #[allow(unused_mut)]
        let mut perms = meta.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if adds_token && perms.mode() & 0o077 != 0 {
                perms.set_mode(0o600);
                made_private = true;
            }
        }
        let _ = std::fs::set_permissions(&tmp, perms);
    } else if !secret {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644));
        }
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    let _ = adds_token;
    Ok(Written {
        backup,
        made_private,
    })
}

/// Installs our hooks into the settings file at `path` using `bin` as the kioku binary.
pub fn install_settings(path: &Path, bin: &str) -> anyhow::Result<SettingsChange> {
    let current = read_settings(path)?;
    let existed = current.is_some();
    let before = current.unwrap_or_else(|| Value::Object(Map::new()));
    let after = merge_hooks(&before, bin)?;
    let changed = !existed || after != before;
    let backup = if changed {
        write_settings(path, existed, &after, false)?.backup
    } else {
        None
    };
    Ok(SettingsChange {
        path: path.to_path_buf(),
        changed,
        backup,
        removed: 0,
        made_private: false,
    })
}

/// Removes our hooks from the settings file at `path` (other content untouched).
pub fn uninstall_settings(path: &Path) -> anyhow::Result<SettingsChange> {
    let Some(before) = read_settings(path)? else {
        return Ok(SettingsChange {
            path: path.to_path_buf(),
            changed: false,
            backup: None,
            removed: 0,
            made_private: false,
        });
    };
    let (after, removed) = remove_hooks(&before)?;
    let backup = if removed > 0 {
        write_settings(path, true, &after, false)?.backup
    } else {
        None
    };
    Ok(SettingsChange {
        path: path.to_path_buf(),
        changed: removed > 0,
        backup,
        removed,
        made_private: false,
    })
}

/// `<server_url>/mcp`.
pub fn mcp_url(cfg: &ClientConfig) -> String {
    format!("{}/mcp", cfg.server_url.trim().trim_end_matches('/'))
}

/// `~/.claude.json`, where Claude Code keeps user-scope MCP servers.
pub fn claude_json_path() -> PathBuf {
    kioku_core::util::home_dir().join(".claude.json")
}

/// Our `mcpServers.kioku` entry: `{type: "http", url, headers: {Authorization}}`.
pub fn mcp_server_entry(cfg: &ClientConfig) -> Value {
    let mut server = json!({ "type": "http", "url": mcp_url(cfg) });
    if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
        server["headers"] = json!({ "Authorization": format!("Bearer {}", token.trim()) });
    }
    server
}

/// The `~/.claude.json` `mcpServers` snippet for manual registration.
pub fn mcp_snippet(cfg: &ClientConfig) -> String {
    let snippet = json!({ "mcpServers": { MCP_NAME: mcp_server_entry(cfg) } });
    serde_json::to_string_pretty(&snippet).unwrap_or_default()
}

/// `{"mcpServers": {"kioku": entry}}`, pretty-printed, for manual registration.
pub fn mcp_entry_snippet(entry: &Value) -> String {
    let snippet = json!({ "mcpServers": { MCP_NAME: entry } });
    serde_json::to_string_pretty(&snippet).unwrap_or_default()
}

/// Returns `json` with `mcpServers.kioku` set to `entry` (everything else kept, key order
/// preserved).
pub fn merge_mcp_entry(json: &Value, entry: &Value) -> anyhow::Result<Value> {
    let mut out = json.clone();
    let root = out
        .as_object_mut()
        .context("the file is not a JSON object")?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .context("`mcpServers` is not an object")?;
    servers.insert(MCP_NAME.to_string(), entry.clone());
    Ok(out)
}

/// Returns `claude_json` with `mcpServers.kioku` set to our entry (everything else kept,
/// key order preserved).
pub fn merge_mcp_server(claude_json: &Value, cfg: &ClientConfig) -> anyhow::Result<Value> {
    merge_mcp_entry(claude_json, &mcp_server_entry(cfg))
}

/// Returns `claude_json` without `mcpServers.kioku` and whether it was present.
pub fn remove_mcp_server(claude_json: &Value) -> (Value, bool) {
    let mut out = claude_json.clone();
    let removed = out
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.shift_remove(MCP_NAME))
        .is_some();
    (out, removed)
}

/// Outcome of registering or removing an MCP entry: a report (never the token) and
/// whether the file was (or, in a dry run, would be) written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpChange {
    /// Human-readable report.
    pub message: String,
    /// True when the file changed.
    pub changed: bool,
}

/// Sets `mcpServers.kioku` = `entry` in the JSON file at `path` (idempotent, one backup,
/// new file 0600). An unparseable file is left alone and the snippet is reported instead.
pub fn register_mcp_entry(path: &Path, entry: &Value, agent: &str, dry_run: bool) -> McpChange {
    let url = entry
        .get("url")
        .or_else(|| entry.get("httpUrl"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let manual = |why: String| McpChange {
        message: format!(
            "{why}\nAdd this to {} yourself to register the MCP server:\n{}",
            path.display(),
            mcp_entry_snippet(entry)
        ),
        changed: false,
    };
    let current = match read_settings(path) {
        Ok(c) => c,
        Err(e) => return manual(format!("{e:#}")),
    };
    let existed = current.is_some();
    let before = current.unwrap_or_else(|| Value::Object(Map::new()));
    let after = match merge_mcp_entry(&before, entry) {
        Ok(v) => v,
        Err(e) => return manual(format!("{} {e:#}; not touching it", path.display())),
    };
    if existed && after == before {
        return McpChange {
            message: format!(
                "MCP server `{MCP_NAME}` already registered in {}: {url}",
                path.display()
            ),
            changed: false,
        };
    }
    if dry_run {
        return McpChange {
            message: format!(
                "would register MCP server `{MCP_NAME}` in {}: {url}",
                path.display()
            ),
            changed: true,
        };
    }
    match write_settings(path, existed, &after, true) {
        Ok(written) => {
            let mut msg = format!(
                "MCP server `{MCP_NAME}` registered in {} (user scope): {url}",
                path.display()
            );
            if let Some(b) = written.backup {
                msg.push_str(&format!("\n  backup: {}", b.display()));
            }
            if written.made_private {
                msg.push_str(&format!("\n  {}", made_private_note(path)));
            }
            msg.push_str(&format!("\n  restart {agent} to pick it up"));
            McpChange {
                message: msg,
                changed: true,
            }
        }
        Err(e) => manual(format!("{e:#}")),
    }
}

/// Registers the MCP server in the `~/.claude.json` at `path` (idempotent, one backup);
/// returns a report. An unparseable file is left alone and the snippet is printed instead.
pub fn register_mcp(path: &Path, cfg: &ClientConfig) -> String {
    register_mcp_entry(path, &mcp_server_entry(cfg), "Claude Code", false).message
}

/// Removes `mcpServers.kioku` from the JSON file at `path` (nothing else).
pub fn unregister_mcp_entry(path: &Path, dry_run: bool) -> McpChange {
    let unchanged = |message: String| McpChange {
        message,
        changed: false,
    };
    let current = match read_settings(path) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return unchanged(format!(
                "MCP server `{MCP_NAME}` not registered ({} does not exist).",
                path.display()
            ));
        }
        Err(e) => {
            return unchanged(format!(
                "{e:#}\nRemove `mcpServers.{MCP_NAME}` from {} manually.",
                path.display()
            ));
        }
    };
    let (after, removed) = remove_mcp_server(&current);
    if !removed {
        return unchanged(format!(
            "MCP server `{MCP_NAME}` not registered in {}.",
            path.display()
        ));
    }
    if dry_run {
        return McpChange {
            message: format!(
                "would remove MCP server `{MCP_NAME}` from {}.",
                path.display()
            ),
            changed: true,
        };
    }
    match rewrite_json_after_removal(path, &after) {
        Ok(_) => McpChange {
            message: format!("MCP server `{MCP_NAME}` removed from {}.", path.display()),
            changed: true,
        },
        Err(e) => unchanged(format!("{e:#}")),
    }
}

/// Removes `mcpServers.kioku` from the `~/.claude.json` at `path` (nothing else); returns a
/// report.
pub fn unregister_mcp(path: &Path) -> String {
    unregister_mcp_entry(path, false).message
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/opt/kioku/bin/kioku";

    fn foreign() -> Value {
        json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(cargo test:*)"]},
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Write", "hooks": [{"type": "command", "command": "prettier --write \"$FILE\""}]}
                ],
                "Notification": [
                    {"hooks": [{"type": "command", "command": "notify-send claude"}]}
                ]
            }
        })
    }

    fn kioku_entries(v: &Value, event: &str) -> Vec<Value> {
        v["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                    .filter(is_ours)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn commands_and_detection() {
        assert_eq!(
            hook_command(BIN, HookEventKind::SessionStart),
            "/opt/kioku/bin/kioku hook session-start"
        );
        let spaced = hook_command("/Users/a b/bin/kioku", HookEventKind::Stop);
        assert_eq!(spaced, "\"/Users/a b/bin/kioku\" hook stop");
        assert!(is_kioku_command(&spaced));
        assert!(is_kioku_command("C:/x/kioku.exe hook stop"));
        assert!(!is_kioku_command("prettier --write"));
        assert!(!is_kioku_command("echo kioku"));
    }

    #[test]
    fn merge_shapes_entries() {
        let v = merge_hooks(&json!({}), BIN).unwrap();
        let ss = &v["hooks"]["SessionStart"][0];
        assert_eq!(ss["matcher"], SESSION_START_MATCHER);
        assert_eq!(ss["hooks"][0]["type"], "command");
        assert_eq!(ss["hooks"][0]["timeout"], 10);
        assert_eq!(
            v["hooks"]["PostToolUse"][0]["matcher"],
            POST_TOOL_USE_MATCHER
        );
        assert!(v["hooks"]["Stop"][0].get("matcher").is_none());
        assert!(v["hooks"]["Stop"][0]["hooks"][0].get("timeout").is_none());
        for e in ALL_EVENTS {
            let entries = kioku_entries(&v, e.claude_code_name());
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["command"], hook_command(BIN, e));
        }
        assert!(merge_hooks(&json!([1]), BIN).is_err());
        assert!(merge_hooks(&json!({"hooks": {"Stop": 3}}), BIN).is_err());
    }

    #[test]
    fn install_twice_then_uninstall_on_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = serde_json::to_string_pretty(&foreign()).unwrap();
        std::fs::write(&path, &original).unwrap();

        let first = install_settings(&path, BIN).unwrap();
        assert!(first.changed);
        let bak = dir.path().join(".claude").join("settings.json.kioku-bak");
        assert_eq!(first.backup.as_deref(), Some(bak.as_path()));
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

        let second = install_settings(&path, BIN).unwrap();
        assert!(!second.changed, "second install is a no-op");
        assert!(second.backup.is_none());

        // A moved binary replaces our entry instead of adding a second one.
        let third = install_settings(&path, "/usr/local/bin/kioku").unwrap();
        assert!(third.changed);
        assert!(
            third.backup.is_none(),
            "backup only before the first modification"
        );
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for e in ALL_EVENTS {
            assert_eq!(kioku_entries(&v, e.claude_code_name()).len(), 1, "{e:?}");
        }
        // Foreign hooks and settings survive, in their original order.
        assert_eq!(v["model"], "opus");
        assert_eq!(
            v["hooks"]["Notification"],
            foreign()["hooks"]["Notification"]
        );
        let ptu = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(ptu.len(), 2);
        assert_eq!(ptu[0], foreign()["hooks"]["PostToolUse"][0]);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "permissions", "hooks"]);

        let un = uninstall_settings(&path).unwrap();
        assert_eq!(un.removed, 6);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v,
            foreign(),
            "uninstall leaves exactly the foreign settings"
        );

        let again = uninstall_settings(&path).unwrap();
        assert!(!again.changed);
    }

    #[test]
    fn install_into_missing_file_and_mixed_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let r = install_settings(&path, BIN).unwrap();
        assert!(r.changed && r.backup.is_none());
        assert!(uninstall_settings(&path).unwrap().removed == 6);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, json!({}));

        // One group holding a foreign hook and ours: only ours goes.
        let mixed = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "say done"},
            {"type": "command", "command": "/old/kioku hook stop"}
        ]}]}});
        let (v, n) = remove_hooks(&mixed).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            v,
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
        );
        let merged = merge_hooks(&mixed, BIN).unwrap();
        assert_eq!(kioku_entries(&merged, "Stop").len(), 1);
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn invalid_json_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(install_settings(&path, BIN).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    fn mcp_cfg() -> ClientConfig {
        ClientConfig {
            server_url: "http://home:7391/".into(),
            auth_token: Some("abc".into()),
            ..ClientConfig::default()
        }
    }

    #[test]
    fn mcp_registration_edits_claude_json_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let original = serde_json::to_string_pretty(&json!({
            "numStartups": 42,
            "mcpServers": {"other": {"type": "stdio", "command": "x"}},
            "projects": {"/a": {"allowedTools": []}}
        }))
        .unwrap();
        std::fs::write(&path, &original).unwrap();

        let msg = register_mcp(&path, &mcp_cfg());
        assert!(msg.contains("registered"), "{msg}");
        assert!(!msg.contains("Bearer"), "the token is never printed: {msg}");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["kioku"],
            json!({"type": "http", "url": "http://home:7391/mcp", "headers": {"Authorization": "Bearer abc"}})
        );
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["numStartups"], 42);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["numStartups", "mcpServers", "projects"]);
        let bak = dir.path().join(".claude.json.kioku-bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

        // idempotent; a changed URL replaces the entry; the backup is kept from the first run
        assert!(register_mcp(&path, &mcp_cfg()).contains("already registered"));
        let mut moved = mcp_cfg();
        moved.server_url = "https://kioku.lan".into();
        register_mcp(&path, &moved);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kioku"]["url"], "https://kioku.lan/mcp");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), original);

        // uninstall removes only our entry
        assert!(unregister_mcp(&path).contains("removed"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, serde_json::from_str::<Value>(&original).unwrap());
        assert!(unregister_mcp(&path).contains("not registered"));
    }

    #[test]
    fn mcp_registration_new_file_and_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        register_mcp(&path, &mcp_cfg());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kioku"]["url"], "http://home:7391/mcp");
        assert!(!dir.path().join(".claude.json.kioku-bak").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        std::fs::write(&path, "{ broken").unwrap();
        let msg = register_mcp(&path, &mcp_cfg());
        assert!(msg.contains("\"mcpServers\""), "prints the snippet: {msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
        std::fs::write(&path, r#"{"mcpServers": []}"#).unwrap();
        assert!(register_mcp(&path, &mcp_cfg()).contains("\"mcpServers\""));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"mcpServers": []}"#
        );
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn adding_the_token_to_a_readable_file_makes_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, r#"{"numStartups": 1}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let msg = register_mcp(&path, &mcp_cfg());
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            msg.matches("permissions were set to 0600").count(),
            1,
            "{msg}"
        );
        assert!(!msg.contains("Bearer abc"), "{msg}");

        // The token is already there (a URL change only): the user's mode is kept, no note.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut moved = mcp_cfg();
        moved.server_url = "https://kioku.lan".into();
        let msg = register_mcp(&path, &moved);
        assert!(!msg.contains("0600"), "{msg}");
        assert_eq!(mode(&path), 0o640);

        // No token: nothing secret is added, the mode stays.
        let plain = dir.path().join("plain.json");
        std::fs::write(&plain, "{}").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut cfg = mcp_cfg();
        cfg.auth_token = None;
        let msg = register_mcp(&plain, &cfg);
        assert!(!msg.contains("0600"), "{msg}");
        assert_eq!(mode(&plain), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_updated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let real = dotfiles.join("claude.json");
        let original = r#"{"numStartups": 1}"#;
        std::fs::write(&real, original).unwrap();
        let link = dir.path().join(".claude.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        register_mcp(&link, &mcp_cfg());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is kept, not replaced by a regular file"
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kioku"]["url"], "http://home:7391/mcp");
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("claude.json.kioku-bak")).unwrap(),
            original
        );
        assert!(!dir.path().join(".claude.json.kioku-bak").exists());
        // Relative link (as dotfile managers create them) and uninstall through it.
        let rel = dir.path().join("rel.json");
        std::os::unix::fs::symlink("dotfiles/claude.json", &rel).unwrap();
        assert!(unregister_mcp(&rel).contains("removed"));
        assert!(
            std::fs::symlink_metadata(&rel)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert!(v["mcpServers"].get("kioku").is_none(), "{v}");
        assert_eq!(v["numStartups"], 1);

        // Codex config.toml and a Markdown block through a link, too.
        let real_toml = dotfiles.join("config.toml");
        std::fs::write(&real_toml, "model = \"o3\"\n").unwrap();
        let toml_link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real_toml, &toml_link).unwrap();
        block::install_codex_config(
            &toml_link,
            block::CodexMcp::Http("http://h/mcp", Some("t")),
            false,
            false,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&real_toml)
                .unwrap()
                .contains("[mcp_servers.kioku]")
        );
        block::uninstall_codex_config(&toml_link, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&real_toml).unwrap(),
            "model = \"o3\"\n"
        );
        assert!(
            std::fs::symlink_metadata(&toml_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn mcp_snippet_shape() {
        let cfg = ClientConfig {
            server_url: "http://home:7391/".into(),
            auth_token: Some("abc".into()),
            ..ClientConfig::default()
        };
        let v: Value = serde_json::from_str(&mcp_snippet(&cfg)).unwrap();
        assert_eq!(
            v,
            json!({"mcpServers": {"kioku": {
                "type": "http",
                "url": "http://home:7391/mcp",
                "headers": {"Authorization": "Bearer abc"}
            }}})
        );
    }
}
