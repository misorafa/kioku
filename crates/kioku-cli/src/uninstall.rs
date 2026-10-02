//! `kioku uninstall` without an agent (SPEC-M3.3 §3): removes what kioku put on this
//! machine — every agent's hooks / MCP entries / instruction blocks (the per-agent removal
//! of [`crate::install::agents`]), the background service, the PATH lines install.sh /
//! install.ps1 added, the binary and its `.prev`; with `--everything` also `config.toml`,
//! and with `--purge-data` the data directory (typed confirmation). It prints the whole plan
//! first, then asks once (unless `--yes`), then does it. A winget / Homebrew binary is left
//! to its package manager. All inputs come in through [`SetupEnv`], so tests run on a
//! fixture home with a recording command runner.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use kioku_core::Config;

use crate::event::Agent;
use crate::install::agents::{AllStatus, claude_desktop_configs, uninstall_agent, uninstall_all};
use crate::setup::SetupEnv;
use crate::update::{PREV_SUFFIX, package_manager, sibling};

/// The comment install.sh puts at the end of the PATH line it adds (SPEC-M2.3 §4.2).
pub const PATH_MARKER: &str = "# added by the kioku installer";

/// File install.ps1 leaves next to kioku.exe when it added that directory to the user PATH.
pub const WINDOWS_PATH_MARKER: &str = "kioku-path-entry.txt";

/// What the user typed to confirm `--purge-data`.
pub const PURGE_WORD: &str = "DELETE";

/// Flags of `kioku uninstall` (no agent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UninstallOptions {
    /// `--everything`: also remove `config.toml` (server URL and token).
    pub everything: bool,
    /// `--purge-data`: also remove the data directory (wiki, database, index, backups).
    pub purge_data: bool,
    /// `--yes`: do not ask.
    pub yes: bool,
    /// `--dry-run`: print the plan only.
    pub dry_run: bool,
}

/// One thing `kioku uninstall` removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// kioku's entries in one agent's config files (the dry-run report lines).
    Agent(Agent, Vec<String>),
    /// The launchd / systemd definition (and the running service).
    Service(PathBuf),
    /// A PATH line install.sh added to a shell rc file.
    PathLine {
        /// The rc file.
        file: PathBuf,
        /// The exact line.
        line: String,
    },
    /// The user PATH entry install.ps1 added (Windows).
    UserPath(String),
    /// `config.toml` (`--everything`).
    Config(PathBuf),
    /// The data directory (`--purge-data`).
    DataDir(PathBuf),
    /// The kioku binary and its `.prev`.
    Binary {
        /// The binary.
        path: PathBuf,
        /// `<binary>.prev`, when it exists.
        prev: Option<PathBuf>,
    },
}

impl Action {
    /// What the plan says about this action.
    pub fn describe(&self) -> Vec<String> {
        match self {
            Action::Agent(agent, lines) => {
                let mut out = vec![format!("{}: remove kioku's entries", agent.as_str())];
                out.extend(lines.iter().map(|l| format!("    {l}")));
                out
            }
            Action::Service(path) => {
                vec![format!("service: stop it and remove {}", path.display())]
            }
            Action::PathLine { file, line } => {
                vec![format!("PATH: remove `{line}` from {}", file.display())]
            }
            Action::UserPath(dir) => vec![format!("PATH: remove {dir} from the user PATH")],
            Action::Config(path) => vec![format!(
                "config: remove {} (server URL and token; --everything)",
                path.display()
            )],
            Action::DataDir(dir) => vec![format!(
                "data: remove {} and everything in it (--purge-data)",
                dir.display()
            )],
            Action::Binary { path, prev } => vec![match prev {
                Some(p) => format!("binary: remove {} and {}", path.display(), p.display()),
                None => format!("binary: remove {}", path.display()),
            }],
        }
    }
}

/// The whole plan: actions in the order they run, and notes printed with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Actions, in order.
    pub actions: Vec<Action>,
    /// Notes (package-managed binary, Claude desktop app, backup hint, errors).
    pub notes: Vec<String>,
}

impl Plan {
    /// The plan as printed before anything happens.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.actions.is_empty() {
            out.push_str("kioku uninstall: nothing to remove on this machine\n");
        } else {
            out.push_str("kioku uninstall will:\n");
            for a in &self.actions {
                for l in a.describe() {
                    out.push_str(&format!("  - {l}\n"));
                }
            }
        }
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        out
    }

    /// The data directory this plan removes, if any.
    pub fn data_dir(&self) -> Option<&Path> {
        self.actions.iter().find_map(|a| match a {
            Action::DataDir(d) => Some(d.as_path()),
            _ => None,
        })
    }
}

/// The config of the machine `env` describes (defaults when there is none).
fn load_config(env: &SetupEnv) -> Config {
    let dir = env.config_dir();
    Config::load_from_dir(&dir, &env.vars).unwrap_or_else(|_| Config::for_data_dir(&dir))
}

/// The ways install.sh spells `dir` in its PATH line: `$HOME/<rel>` under the home
/// directory, else the absolute path.
pub fn path_spellings(dir: &Path, home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rel) = dir.strip_prefix(home)
        && !rel.as_os_str().is_empty()
    {
        out.push(format!("$HOME/{}", rel.display()));
    }
    out.push(dir.display().to_string());
    out
}

/// The exact lines install.sh writes for `dir` (POSIX shells and fish).
pub fn installer_path_lines(dir: &Path, home: &Path) -> Vec<String> {
    path_spellings(dir, home)
        .into_iter()
        .flat_map(|d| {
            [
                format!("export PATH=\"{d}:$PATH\" {PATH_MARKER}"),
                format!("contains \"{d}\" $PATH; or set -gx PATH \"{d}\" $PATH {PATH_MARKER}"),
            ]
        })
        .collect()
}

/// The rc files install.sh may have written to (SPEC-M2.3 §4.2).
pub fn rc_files(env: &SetupEnv) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(z) = env.vars.get("ZDOTDIR").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(z).join(".zshrc"));
    }
    for name in [".zshrc", ".bashrc", ".bash_profile", ".profile"] {
        out.push(env.home.join(name));
    }
    out.push(env.home.join(".config/fish/conf.d/kioku.fish"));
    out.dedup();
    out
}

/// `text` without the lines equal to one of `lines` (line endings of the rest kept);
/// returns the new text and how many lines were removed.
pub fn remove_lines(text: &str, lines: &[String]) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut removed = 0;
    for l in text.split_inclusive('\n') {
        let bare = l.trim_end_matches(['\n', '\r']);
        if lines.iter().any(|x| x == bare) {
            removed += 1;
        } else {
            out.push_str(l);
        }
    }
    (out, removed)
}

/// True when `path` is a cargo build output (`…/target/{debug,release}/…`): such a binary
/// is never removed (it is a developer's build, or a test's own executable).
pub fn is_build_output(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "target")
}

/// The user PATH (Windows): `KIOKU_USER_PATH_FILE` (tests), else the registry through
/// PowerShell. `None` when it cannot be read.
fn read_user_path(env: &SetupEnv) -> Option<String> {
    if let Some(f) = env
        .vars
        .get("KIOKU_USER_PATH_FILE")
        .filter(|v| !v.is_empty())
    {
        return std::fs::read_to_string(f)
            .ok()
            .map(|t| t.trim().to_string());
    }
    if !cfg!(windows) {
        return None;
    }
    let out = env.runner.run(&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Environment]::GetEnvironmentVariable('Path', 'User')",
    ]);
    out.success.then(|| out.stdout.trim().to_string())
}

/// Writes the user PATH (see [`read_user_path`]).
fn write_user_path(env: &SetupEnv, value: &str) -> anyhow::Result<()> {
    if let Some(f) = env
        .vars
        .get("KIOKU_USER_PATH_FILE")
        .filter(|v| !v.is_empty())
    {
        return std::fs::write(f, value).with_context(|| format!("writing {f}"));
    }
    let script = format!(
        "[Environment]::SetEnvironmentVariable('Path', '{}', 'User')",
        value.replace('\'', "''")
    );
    let out = env.runner.run(&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ]);
    if !out.success {
        anyhow::bail!("setting the user PATH failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Normalised PATH entry for comparisons (Windows: case-insensitive, no trailing `\`).
fn norm_entry(e: &str) -> String {
    e.trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// True when two PATH entries name the same directory: equal after [`norm_entry`], or the
/// same once resolved (`C:\Users\RUNNER~1\…` and its long form).
fn same_dir(a: &str, b: &str) -> bool {
    if norm_entry(a) == norm_entry(b) {
        return true;
    }
    let canon = |p: &str| kioku_core::util::canonical_plain(Path::new(p.trim())).ok();
    matches!((canon(a), canon(b)), (Some(x), Some(y)) if x == y)
}

/// `path` (a `;`-separated user PATH) without the entries equal to `dir`.
pub fn without_entry(path: &str, dir: &str) -> (String, bool) {
    let kept: Vec<&str> = path
        .split(';')
        .filter(|e| !e.trim().is_empty() && !same_dir(e, dir))
        .collect();
    let changed = kept.len() != path.split(';').filter(|e| !e.trim().is_empty()).count();
    (kept.join(";"), changed)
}

/// The directory install.ps1 installs to by default: `%LOCALAPPDATA%\Programs\kioku`.
fn default_windows_dir(env: &SetupEnv) -> Option<PathBuf> {
    env.vars
        .get("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("Programs").join("kioku"))
}

/// Builds the plan (reads only; the agent part is a dry run of the per-agent removal).
pub fn plan(opts: &UninstallOptions, env: &SetupEnv) -> Plan {
    let mut plan = Plan::default();
    let cfg = load_config(env);
    let ctx = env.install_ctx(&cfg.client);

    // 1. Agents: what `kioku uninstall all` would remove.
    for (agent, status) in uninstall_all(&ctx, false, true) {
        match status {
            AllStatus::Changed(r) => plan.actions.push(Action::Agent(agent, r.lines)),
            AllStatus::Error(e) => plan
                .notes
                .push(format!("{}: cannot read its config: {e}", agent.as_str())),
            _ => {}
        }
    }
    if plan
        .actions
        .iter()
        .any(|a| matches!(a, Action::Agent(Agent::ClaudeCode, _)))
        && !claude_desktop_configs(&ctx).is_empty()
    {
        plan.notes.push(
            "quit the Claude desktop app completely first (also from the tray / menu bar): it rewrites claude_desktop_config.json while it runs and would put the kioku entry back".into(),
        );
    }

    // 2. The service (only a definition that exists in this home).
    let manager = env.service_manager(&cfg.data_dir);
    if manager.is_installed()
        && let Some(p) = manager.definition_path()
    {
        plan.actions.push(Action::Service(p));
    }

    // 3. PATH lines.
    let bin = PathBuf::from(&env.bin);
    if let Some(dir) = bin.parent() {
        let lines = installer_path_lines(dir, &env.home);
        for file in rc_files(env) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for l in text.lines() {
                let l = l.trim_end_matches('\r');
                if lines.iter().any(|x| x == l)
                    && !plan.actions.iter().any(
                        |a| matches!(a, Action::PathLine { file: f, line } if *f == file && line == l),
                    )
                {
                    plan.actions.push(Action::PathLine {
                        file: file.clone(),
                        line: l.to_string(),
                    });
                }
            }
        }
        let ours = dir.join(WINDOWS_PATH_MARKER).is_file()
            || default_windows_dir(env)
                .is_some_and(|d| same_dir(&d.display().to_string(), &dir.display().to_string()));
        if ours
            && let Some(user) = read_user_path(env)
            && without_entry(&user, &dir.display().to_string()).1
        {
            plan.actions
                .push(Action::UserPath(dir.display().to_string()));
        }
    }

    // 4. config.toml, 5. the data directory.
    if opts.everything && cfg.config_file.is_file() {
        plan.actions.push(Action::Config(cfg.config_file.clone()));
    }
    if opts.purge_data && cfg.data_dir.is_dir() {
        if cfg.data_dir.join("wiki").is_dir() {
            plan.notes.push(format!(
                "--purge-data deletes every page, session and handoff in {}; make a backup first: kioku backup (then copy the backup directory off this machine)",
                cfg.data_dir.display()
            ));
        }
        plan.actions.push(Action::DataDir(cfg.data_dir.clone()));
    }

    // 6. The binary, last.
    if let Some(pm) = package_manager(&bin) {
        plan.notes.push(format!(
            "kioku was installed with {}: the binary is left to it; remove it with: {}",
            pm.name(),
            pm.uninstall_command()
        ));
    } else if is_build_output(&bin) {
        plan.notes.push(format!(
            "{} is a cargo build output: the binary is left alone",
            bin.display()
        ));
    } else if bin.is_file() {
        let prev = sibling(&bin, PREV_SUFFIX);
        plan.actions.push(Action::Binary {
            path: bin.clone(),
            prev: prev.is_file().then_some(prev),
        });
    }
    if !opts.everything && cfg.config_file.is_file() {
        plan.notes.push(format!(
            "{} is kept (server URL and token); --everything removes it",
            cfg.config_file.display()
        ));
    }
    plan
}

/// Removes the running binary. Unix: unlink. Windows: a running exe cannot be deleted, so
/// it is renamed aside and a detached `cmd` deletes it once this process has exited.
fn remove_binary(path: &Path) -> anyhow::Result<()> {
    let running = std::env::current_exe()
        .ok()
        .and_then(|e| kioku_core::util::canonical_plain(&e).ok())
        .zip(kioku_core::util::canonical_plain(path).ok())
        .is_some_and(|(a, b)| a == b);
    if cfg!(windows) && running {
        let aside = sibling(path, &format!(".uninstalled-{}", std::process::id()));
        std::fs::rename(path, &aside)
            .with_context(|| format!("moving {} aside", path.display()))?;
        let script = format!(
            "ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"",
            aside.display()
        );
        let _ = spawn_detached_cmd(&script);
        return Ok(());
    }
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
}

/// Starts `cmd /d /c <script>` detached and windowless (Windows only).
#[cfg(windows)]
fn spawn_detached_cmd(script: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .args(["/d", "/c"])
        .raw_arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

/// Not used outside Windows.
#[cfg(not(windows))]
fn spawn_detached_cmd(_script: &str) -> std::io::Result<()> {
    Ok(())
}

/// Runs one action; returns its report line.
fn execute(action: &Action, env: &SetupEnv) -> anyhow::Result<String> {
    let cfg = load_config(env);
    match action {
        Action::Agent(agent, _) => {
            let ctx = env.install_ctx(&cfg.client);
            let r = uninstall_agent(*agent, &ctx, false, false)?;
            Ok(format!(
                "{}: removed{}",
                agent.as_str(),
                r.lines
                    .iter()
                    .map(|l| format!("\n    {l}"))
                    .collect::<String>()
            ))
        }
        Action::Service(path) => {
            env.service_manager(&cfg.data_dir).uninstall()?;
            Ok(format!("service: stopped, removed {}", path.display()))
        }
        Action::PathLine { file, line } => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            let (after, n) = remove_lines(&text, std::slice::from_ref(line));
            if n == 0 {
                return Ok(format!("PATH: {} no longer has the line", file.display()));
            }
            let fish_file = file.ends_with("conf.d/kioku.fish");
            if fish_file && after.trim().is_empty() {
                std::fs::remove_file(file)
                    .with_context(|| format!("removing {}", file.display()))?;
                return Ok(format!("PATH: removed {}", file.display()));
            }
            std::fs::write(file, after).with_context(|| format!("writing {}", file.display()))?;
            Ok(format!(
                "PATH: removed the kioku line from {}",
                file.display()
            ))
        }
        Action::UserPath(dir) => {
            let user = read_user_path(env).context("reading the user PATH")?;
            let (after, _) = without_entry(&user, dir);
            write_user_path(env, &after)?;
            let _ = std::fs::remove_file(Path::new(dir).join(WINDOWS_PATH_MARKER));
            Ok(format!(
                "PATH: removed {dir} from the user PATH (new windows only)"
            ))
        }
        Action::Config(path) => {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            Ok(format!("config: removed {}", path.display()))
        }
        Action::DataDir(dir) => {
            std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
            Ok(format!("data: removed {}", dir.display()))
        }
        Action::Binary { path, prev } => {
            if let Some(p) = prev {
                std::fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
            }
            remove_binary(path)?;
            Ok(format!("binary: removed {}", path.display()))
        }
    }
}

/// Reads one answer line (`None` at end of input).
fn ask(input: &mut dyn BufRead, out: &mut dyn Write, prompt: &str) -> Option<String> {
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// `kioku uninstall [--everything] [--purge-data] [--yes] [--dry-run]`: prints the plan,
/// asks (unless `--yes`; `--purge-data` also wants [`PURGE_WORD`] typed), runs it. Returns
/// the exit code (1 when aborted or a step failed).
pub fn run_uninstall(
    opts: &UninstallOptions,
    env: &SetupEnv,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> i32 {
    let plan = plan(opts, env);
    let _ = write!(out, "{}", plan.render());
    if plan.actions.is_empty() {
        return 0;
    }
    if opts.dry_run {
        let _ = writeln!(out, "dry run: nothing was changed");
        return 0;
    }
    if !opts.yes {
        let answer = ask(input, out, "Proceed? [y/N] ");
        if !matches!(answer.as_deref(), Some("y" | "Y" | "yes" | "YES" | "Yes")) {
            let _ = writeln!(
                out,
                "aborted: nothing was changed{}",
                if answer.is_none() {
                    " (no answer on stdin; re-run with --yes)"
                } else {
                    ""
                }
            );
            return 1;
        }
        if let Some(dir) = plan.data_dir() {
            let typed = ask(
                input,
                out,
                &format!(
                    "Type {PURGE_WORD} to remove {} permanently: ",
                    dir.display()
                ),
            );
            if typed.as_deref() != Some(PURGE_WORD) {
                let _ = writeln!(out, "aborted: nothing was changed");
                return 1;
            }
        }
    }
    let mut failed = 0;
    for action in &plan.actions {
        match execute(action, env) {
            Ok(line) => {
                let _ = writeln!(out, "{line}");
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(out, "error: {e:#}");
            }
        }
    }
    if failed > 0 {
        let _ = writeln!(out, "kioku uninstall: {failed} step(s) failed");
        return 1;
    }
    let _ = writeln!(
        out,
        "kioku uninstall: done. Restart running agents so they drop kioku's hooks and MCP server."
    );
    0
}

#[cfg(test)]
mod tests;
