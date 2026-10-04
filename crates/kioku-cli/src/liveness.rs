//! Positive liveness (SPEC-M3.2 §2): every hook leaves `{event, at, ok}` for its agent in
//! `<kioku dir>/state/last-hook.json` — the one small write a hook may add — and `kioku
//! doctor` / `kioku status --agents` turn it into one "last successful hook" line per
//! installed agent, warning only when an agent that is present on this machine has not
//! succeeded in [`LIVENESS_WINDOW`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kioku_core::Config;
use serde::{Deserialize, Serialize};

use crate::event::{Agent, HookEnv, HookEventKind};

/// File name inside `<kioku dir>/state/`.
pub const LIVENESS_FILE: &str = "last-hook.json";
/// An installed, present agent without a successful hook for this long is a warning.
pub const LIVENESS_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

/// The last hook of one agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMark {
    /// Hook event (`session-start`, `stop`, …).
    pub event: String,
    /// RFC 3339 time it finished.
    pub at: String,
    /// Whether it reached the server and did its job.
    pub ok: bool,
    /// Time of the last successful hook (kept when a later one fails).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<String>,
}

/// `<kioku dir>/state/last-hook.json` (`None` without a home directory).
pub fn liveness_path(cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    crate::hook::client_state_root(cfg, env).map(|d| d.join("state").join(LIVENESS_FILE))
}

/// Every agent's last hook; empty when the file is missing or unreadable.
pub fn load(path: &Path) -> BTreeMap<String, HookMark> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Entry of the `kioku mcp` bridge in `last-hook.json` (SPEC-M3.4 §1): desktop apps run no
/// hooks, so their successful tool calls are the machine's sign of life.
pub const MCP_ENTRY: &str = "mcp";
/// Event recorded for the bridge's entry.
pub const MCP_EVENT: &str = "tool_call";

/// Records one hook of `agent`. Best effort and never blocking on failure: a temp file in
/// the same directory renamed over the old one (readers never see a torn file).
pub fn record(path: &Path, agent: Agent, event: HookEventKind, ok: bool, now: &str) {
    record_named(path, agent.as_str(), event.cli_name(), ok, now);
}

/// [`record`] under any entry name (`mcp` for the stdio bridge) and event name.
pub fn record_named(path: &Path, entry: &str, event: &str, ok: bool, now: &str) {
    let _ = try_record(path, entry, event, ok, now);
}

fn try_record(path: &Path, entry: &str, event: &str, ok: bool, now: &str) -> std::io::Result<()> {
    let mut marks = load(path);
    let previous_ok = marks.get(entry).and_then(|m| m.last_ok.clone());
    marks.insert(
        entry.to_string(),
        HookMark {
            event: event.to_string(),
            at: now.to_string(),
            ok,
            last_ok: if ok {
                Some(now.to_string())
            } else {
                previous_ok
            },
        },
    );
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    if !dir.is_dir() {
        kioku_core::util::create_private_dir(dir)?;
    }
    let tmp = dir.join(format!(".{LIVENESS_FILE}.{}.tmp", std::process::id()));
    let text = serde_json::to_string(&marks).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Executable names (on `PATH`), per-user install locations and app bundles that mean the
/// agent itself — not just its config directory — is on this machine.
fn agent_markers(agent: Agent) -> (&'static [&'static str], &'static [&'static str]) {
    match agent {
        Agent::ClaudeCode => (&["claude"], &["Claude.app"]),
        Agent::Codex => (&["codex"], &["Codex.app"]),
        Agent::Cursor => (&["cursor", "cursor-agent"], &["Cursor.app"]),
        Agent::GeminiCli => (&["gemini"], &[]),
        Agent::Antigravity => (&["agy", "antigravity"], &["Antigravity.app"]),
    }
}

/// True when the agent's binary is on `path_var`, in a per-user location under `home`, or
/// its app is in one of `app_dirs`.
pub fn agent_present(agent: Agent, path_var: &str, home: &Path, app_dirs: &[PathBuf]) -> bool {
    let (bins, apps) = agent_markers(agent);
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    dirs.push(home.join(".local").join("bin"));
    dirs.push(home.join(".claude").join("local"));
    let on_disk = dirs.iter().any(|d| {
        bins.iter().any(|b| {
            ["", ".exe", ".cmd"]
                .iter()
                .any(|ext| d.join(format!("{b}{ext}")).is_file())
        })
    });
    on_disk
        || app_dirs
            .iter()
            .any(|d| apps.iter().any(|a| d.join(a).exists()))
}

/// The `hooks.liveness.<agent>` check: OK with the last success, WARN when an installed
/// agent that is present here has no success within [`LIVENESS_WINDOW`] of `now` (unix s).
pub fn liveness_check(
    agent: Agent,
    mark: Option<&HookMark>,
    present: bool,
    now: i64,
) -> crate::doctor::Check {
    use crate::doctor::{Check, Status};
    let id = format!("hooks.liveness.{}", agent.as_str());
    let name = agent.as_str();
    let last_ok = mark.and_then(|m| m.last_ok.clone());
    let recent = last_ok
        .as_deref()
        .and_then(kioku_core::util::parse_ts)
        .is_some_and(|t| now - t.timestamp() <= LIVENESS_WINDOW.as_secs() as i64);
    let latest_failed = mark
        .filter(|m| !m.ok)
        .map(|m| {
            format!(
                "; the latest hook ({} at {}) failed, see logs/hook.log",
                m.event, m.at
            )
        })
        .unwrap_or_default();
    if recent {
        let event = mark.map(|m| m.event.as_str()).unwrap_or("-");
        let at = last_ok.unwrap_or_default();
        let event = if mark.is_some_and(|m| m.ok) {
            format!(" ({event})")
        } else {
            String::new()
        };
        return Check {
            id,
            status: Status::Ok,
            message: format!("{name}: last successful hook {at}{event}{latest_failed}"),
            fix: None,
            action: None,
        };
    }
    let since = match &last_ok {
        Some(at) => format!("last success {at}"),
        None => "no successful hook recorded yet".to_string(),
    };
    if present {
        Check {
            id,
            status: Status::Warn,
            message: format!(
                "{name}: no successful hook in the last 7 days ({since}{latest_failed}) although {} is installed here / 7 日以上フックが成功していません",
                agent.display_name()
            ),
            fix: Some(format!(
                "start a session in {}; if this stays, run kioku doctor --agent {name} and read logs/hook.log",
                agent.display_name()
            )),
            action: None,
        }
    } else {
        Check {
            id,
            status: Status::Ok,
            message: format!(
                "{name}: {since}{latest_failed}; {} itself is not found on this machine",
                agent.display_name()
            ),
            fix: None,
            action: None,
        }
    }
}

/// The `hooks.liveness.mcp` line (SPEC-M3.4 §1): when the `kioku mcp` bridge recorded a
/// successful tool call, say when — always OK (an app may simply not have been used).
pub fn mcp_liveness_check(mark: &HookMark, now: i64) -> crate::doctor::Check {
    let at = mark.last_ok.clone().unwrap_or_else(|| mark.at.clone());
    let stale = kioku_core::util::parse_ts(&at)
        .is_none_or(|t| now - t.timestamp() > LIVENESS_WINDOW.as_secs() as i64);
    crate::doctor::Check {
        id: format!("hooks.liveness.{MCP_ENTRY}"),
        status: crate::doctor::Status::Ok,
        message: format!(
            "{MCP_ENTRY} (kioku mcp bridge, desktop apps): last successful tool call {at}{}",
            if stale { " (more than 7 days ago)" } else { "" }
        ),
        fix: None,
        action: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn at(s: &str) -> i64 {
        kioku_core::util::parse_ts(s).unwrap().timestamp()
    }

    #[test]
    fn record_keeps_the_last_success_and_one_entry_per_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join(LIVENESS_FILE);
        record(
            &path,
            Agent::ClaudeCode,
            HookEventKind::SessionStart,
            true,
            "2026-10-01T00:00:00.000Z",
        );
        record(
            &path,
            Agent::Codex,
            HookEventKind::Stop,
            true,
            "2026-10-01T01:00:00.000Z",
        );
        record(
            &path,
            Agent::ClaudeCode,
            HookEventKind::PostToolUse,
            false,
            "2026-10-02T00:00:00.000Z",
        );
        let marks = load(&path);
        assert_eq!(marks.len(), 2);
        let claude = &marks["claude-code"];
        assert!(!claude.ok);
        assert_eq!(claude.at, "2026-10-02T00:00:00.000Z");
        assert_eq!(claude.last_ok.as_deref(), Some("2026-10-01T00:00:00.000Z"));
        assert_eq!(marks["codex"].event, "stop");
        // Only the file itself is left behind (no temp files).
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [LIVENESS_FILE]);
    }

    /// SPEC-M3.4 §1: the bridge's entry sits next to the hooks' and shows as one OK line.
    #[test]
    fn the_mcp_bridge_has_its_own_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join(LIVENESS_FILE);
        record(
            &path,
            Agent::Codex,
            HookEventKind::Stop,
            true,
            "2026-10-01T00:00:00.000Z",
        );
        record_named(
            &path,
            MCP_ENTRY,
            MCP_EVENT,
            true,
            "2026-10-02T00:00:00.000Z",
        );
        let marks = load(&path);
        assert_eq!(marks["codex"].event, "stop");
        let mcp = &marks[MCP_ENTRY];
        assert_eq!(mcp.event, "tool_call");
        assert_eq!(mcp.last_ok.as_deref(), Some("2026-10-02T00:00:00.000Z"));
        let c = mcp_liveness_check(mcp, at("2026-10-02T12:00:00.000Z"));
        assert_eq!(c.id, "hooks.liveness.mcp");
        assert_eq!(c.status, Status::Ok);
        assert!(
            c.message
                .ends_with("last successful tool call 2026-10-02T00:00:00.000Z"),
            "{c:?}"
        );
        let old = mcp_liveness_check(mcp, at("2026-10-20T00:00:00.000Z"));
        assert_eq!(old.status, Status::Ok);
        assert!(old.message.contains("more than 7 days ago"), "{old:?}");
    }

    #[test]
    fn record_never_fails_on_an_unwritable_location() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("state");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        record(
            &blocker.join(LIVENESS_FILE),
            Agent::Cursor,
            HookEventKind::Stop,
            true,
            "2026-10-02T00:00:00.000Z",
        );
        assert!(load(&blocker.join(LIVENESS_FILE)).is_empty());
    }

    /// The warn/ok matrix: recent success → OK; stale or never + present → WARN; stale or
    /// never + absent → OK.
    #[test]
    fn liveness_matrix() {
        let now = at("2026-10-02T12:00:00.000Z");
        let fresh = HookMark {
            event: "stop".into(),
            at: "2026-10-02T11:00:00.000Z".into(),
            ok: true,
            last_ok: Some("2026-10-02T11:00:00.000Z".into()),
        };
        let stale = HookMark {
            event: "post-tool-use".into(),
            at: "2026-10-01T00:00:00.000Z".into(),
            ok: false,
            last_ok: Some("2026-09-20T00:00:00.000Z".into()),
        };
        let failing_recently = HookMark {
            ok: false,
            at: "2026-10-02T11:30:00.000Z".into(),
            ..fresh.clone()
        };
        let cases = [
            (Some(&fresh), true, Status::Ok),
            (Some(&fresh), false, Status::Ok),
            (Some(&failing_recently), true, Status::Ok),
            (Some(&stale), true, Status::Warn),
            (Some(&stale), false, Status::Ok),
            (None, true, Status::Warn),
            (None, false, Status::Ok),
        ];
        for (mark, present, want) in cases {
            let c = liveness_check(Agent::Codex, mark, present, now);
            assert_eq!(c.status, want, "{mark:?} present={present}: {c:?}");
            assert_eq!(c.id, "hooks.liveness.codex");
            assert_eq!(c.fix.is_some(), want == Status::Warn);
        }
        let ok = liveness_check(Agent::Codex, Some(&fresh), true, now);
        assert!(
            ok.message
                .contains("last successful hook 2026-10-02T11:00:00.000Z (stop)")
        );
        let failing = liveness_check(Agent::Codex, Some(&failing_recently), true, now);
        assert!(
            failing.message.contains("failed, see logs/hook.log"),
            "{failing:?}"
        );
        let never = liveness_check(Agent::Codex, None, true, now);
        assert!(never.message.contains("no successful hook recorded yet"));
    }

    #[test]
    fn presence_from_path_home_and_apps() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let home = tmp.path().join("home");
        let apps = tmp.path().join("Applications");
        for d in [&bin, &home, &apps] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path_var = bin.display().to_string();
        let app_dirs = vec![apps.clone()];
        assert!(!agent_present(Agent::Codex, &path_var, &home, &app_dirs));
        std::fs::write(bin.join("codex"), "").unwrap();
        assert!(agent_present(Agent::Codex, &path_var, &home, &app_dirs));
        assert!(!agent_present(Agent::Cursor, &path_var, &home, &app_dirs));
        std::fs::create_dir_all(apps.join("Cursor.app")).unwrap();
        assert!(agent_present(Agent::Cursor, &path_var, &home, &app_dirs));
        std::fs::create_dir_all(home.join(".claude/local")).unwrap();
        std::fs::write(home.join(".claude/local/claude"), "").unwrap();
        assert!(agent_present(Agent::ClaudeCode, "", &home, &[]));
    }
}
