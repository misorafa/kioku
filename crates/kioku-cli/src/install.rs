//! `kioku install|uninstall claude-code` (spec §8.5): idempotent merge of our hook entries
//! into Claude Code's settings.json, plus MCP server registration via `claude mcp`.
//!
//! Our entries are recognized by the `kioku hook` command substring, so foreign hooks are
//! never touched and re-installing (even from a moved binary) leaves one entry per event.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use kioku_core::ClientConfig;
use serde_json::{Map, Value, json};

use crate::event::{ALL_EVENTS, HookEventKind};

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

fn hooks_object(settings: &mut Value) -> anyhow::Result<&mut Map<String, Value>> {
    let root = settings
        .as_object_mut()
        .context("settings.json is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    hooks
        .as_object_mut()
        .context("settings.json `hooks` is not an object")
}

/// Returns `settings` with exactly one kioku entry per Claude Code event (foreign hooks kept).
pub fn merge_hooks(settings: &Value, bin: &str) -> anyhow::Result<Value> {
    let mut out = settings.clone();
    let hooks = hooks_object(&mut out)?;
    for event in ALL_EVENTS {
        let key = event.claude_code_name();
        let existing = match hooks.get(key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(_) => anyhow::bail!("settings.json hooks.{key} is not an array"),
        };
        let (mut groups, first, _) = strip_groups(&existing);
        let at = first.unwrap_or(groups.len()).min(groups.len());
        groups.insert(at, our_group(bin, event));
        hooks.insert(key.to_string(), Value::Array(groups));
    }
    Ok(out)
}

/// Returns `settings` without any kioku hook entry and the number of entries removed.
/// Event lists (and the `hooks` object) that only held our entries are removed too.
pub fn remove_hooks(settings: &Value) -> anyhow::Result<(Value, usize)> {
    let mut out = settings.clone();
    let Some(root) = out.as_object_mut() else {
        anyhow::bail!("settings.json is not a JSON object");
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
}

fn read_settings(path: &Path) -> anyhow::Result<Option<Value>> {
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

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

fn write_settings(path: &Path, existed: bool, value: &Value) -> anyhow::Result<Option<PathBuf>> {
    let mut backup = None;
    let bak = backup_path(path);
    if existed && !bak.exists() {
        std::fs::copy(path, &bak).with_context(|| format!("backing up to {}", bak.display()))?;
        backup = Some(bak);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serializing settings")? + "\n";
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(backup)
}

/// Installs our hooks into the settings file at `path` using `bin` as the kioku binary.
pub fn install_settings(path: &Path, bin: &str) -> anyhow::Result<SettingsChange> {
    let current = read_settings(path)?;
    let existed = current.is_some();
    let before = current.unwrap_or_else(|| Value::Object(Map::new()));
    let after = merge_hooks(&before, bin)?;
    let changed = !existed || after != before;
    let backup = if changed {
        write_settings(path, existed, &after)?
    } else {
        None
    };
    Ok(SettingsChange {
        path: path.to_path_buf(),
        changed,
        backup,
        removed: 0,
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
        });
    };
    let (after, removed) = remove_hooks(&before)?;
    let backup = if removed > 0 {
        write_settings(path, true, &after)?
    } else {
        None
    };
    Ok(SettingsChange {
        path: path.to_path_buf(),
        changed: removed > 0,
        backup,
        removed,
    })
}

/// `<server_url>/mcp`.
pub fn mcp_url(cfg: &ClientConfig) -> String {
    format!("{}/mcp", cfg.server_url.trim().trim_end_matches('/'))
}

/// The `~/.claude.json` `mcpServers` snippet for manual registration.
pub fn mcp_snippet(cfg: &ClientConfig) -> String {
    let mut server = json!({ "type": "http", "url": mcp_url(cfg) });
    if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
        server["headers"] = json!({ "Authorization": format!("Bearer {}", token.trim()) });
    }
    let snippet = json!({ "mcpServers": { MCP_NAME: server } });
    serde_json::to_string_pretty(&snippet).unwrap_or_default()
}

/// Finds an executable on `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| {
            let mut c = vec![dir.join(name)];
            if cfg!(windows) {
                c.push(dir.join(format!("{name}.exe")));
                c.push(dir.join(format!("{name}.cmd")));
            }
            c
        })
        .find(|p| p.is_file())
}

fn run_quiet(cmd: &mut Command) -> anyhow::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {cmd:?}"))
}

/// Registers the MCP server with `claude mcp add` (re-adding if present); returns a report.
/// Without `claude` on PATH, returns the manual snippet instead.
pub fn register_mcp(cfg: &ClientConfig) -> String {
    let Some(claude) = find_on_path("claude") else {
        return format!(
            "`claude` not found on PATH. Add this to ~/.claude.json to register the MCP server:\n{}",
            mcp_snippet(cfg)
        );
    };
    // Idempotency: `claude mcp add` refuses an existing name, so remove ours first.
    let _ = run_quiet(Command::new(&claude).args(["mcp", "remove", MCP_NAME, "--scope", "user"]));
    let mut cmd = Command::new(&claude);
    cmd.args(["mcp", "add", "--transport", "http", MCP_NAME])
        .arg(mcp_url(cfg));
    if let Some(token) = cfg.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
        cmd.arg("--header")
            .arg(format!("Authorization: Bearer {}", token.trim()));
    }
    cmd.args(["--scope", "user"]);
    match run_quiet(&mut cmd) {
        Ok(out) if out.status.success() => format!(
            "MCP server `{MCP_NAME}` registered with Claude Code (user scope): {}",
            mcp_url(cfg)
        ),
        Ok(out) => format!(
            "`claude mcp add` failed ({}): {}\nAdd this to ~/.claude.json instead:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            mcp_snippet(cfg)
        ),
        Err(e) => format!(
            "{e:#}\nAdd this to ~/.claude.json instead:\n{}",
            mcp_snippet(cfg)
        ),
    }
}

/// Removes the MCP server with `claude mcp remove`; returns a report.
pub fn unregister_mcp() -> String {
    let Some(claude) = find_on_path("claude") else {
        return format!(
            "`claude` not found on PATH: remove `mcpServers.{MCP_NAME}` from ~/.claude.json manually."
        );
    };
    match run_quiet(Command::new(&claude).args(["mcp", "remove", MCP_NAME, "--scope", "user"])) {
        Ok(out) if out.status.success() => {
            format!("MCP server `{MCP_NAME}` removed from Claude Code.")
        }
        Ok(out) => format!(
            "`claude mcp remove {MCP_NAME}` did not succeed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => format!("{e:#}"),
    }
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
