//! `kioku service …` (M2 §10): the user-level background service that runs `kioku serve` —
//! a launchd LaunchAgent on macOS, a `systemd --user` unit on Linux, a clear fallback
//! message elsewhere. Never needs sudo.
//!
//! Rendering ([`render_plist`], [`render_unit`]) is pure so both formats are tested on any
//! OS. Every external command goes through a [`Runner`], which either executes or — in
//! tests — records the argv and answers from a closure, so tests never touch launchctl or
//! systemctl. The same module owns the server health probe used by `service status`,
//! `setup` and `doctor`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use kioku_core::ClientConfig;
use parking_lot::Mutex;
use serde_json::Value;

use crate::client::{ApiClient, HttpError, http_status};

/// launchd label (and plist file stem).
pub const LAUNCHD_LABEL: &str = "dev.kioku.serve";
/// systemd user unit name.
pub const SYSTEMD_UNIT: &str = "kioku.service";
/// `PATH` given to the LaunchAgent so `kioku serve` finds Homebrew's `git`.
pub const LAUNCHD_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// `RUST_LOG` given to the service.
pub const SERVICE_RUST_LOG: &str = "info,tantivy=warn";
/// tracing log of the service (`--log-file`), inside `<data_dir>/logs/`.
pub const SERVE_LOG: &str = "serve.log";
/// stdout/stderr of the service process (panics, pre-logging failures).
pub const SERVE_STDERR_LOG: &str = "serve.stderr.log";
/// `launchctl bootstrap` attempts after a `bootout` (launchd tears the old job down
/// asynchronously, so an immediate bootstrap can fail with an I/O error).
pub const BOOTSTRAP_ATTEMPTS: usize = 5;
/// Pause between two `launchctl bootstrap` attempts.
pub const BOOTSTRAP_RETRY_DELAY: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------------------
// Definitions (pure rendering)
// ---------------------------------------------------------------------------------------

/// What a service definition runs: the binary and the data directory (both absolute).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Absolute path of the kioku binary.
    pub bin: String,
    /// Absolute data directory (`KIOKU_DATA_DIR`, working directory, logs).
    pub data_dir: PathBuf,
}

impl ServiceSpec {
    /// `<data_dir>/logs/serve.log`.
    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("logs").join(SERVE_LOG)
    }

    /// `<data_dir>/logs/serve.stderr.log`.
    pub fn stderr_file(&self) -> PathBuf {
        self.data_dir.join("logs").join(SERVE_STDERR_LOG)
    }
}

/// XML text with `& < > " '` escaped.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The LaunchAgent plist (M2 §10.3), byte for byte.
pub fn render_plist(spec: &ServiceSpec) -> String {
    let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
    let p = |v: &Path| s(&v.display().to_string());
    let data = spec.data_dir.as_path();
    let stderr = spec.stderr_file();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>{label}
  <key>ProgramArguments</key>
  <array>
    {bin}
    <string>serve</string>
    <string>--log-file</string>{log}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>KIOKU_DATA_DIR</key>{data}
    <key>PATH</key>{path}
    <key>RUST_LOG</key>{rust_log}
  </dict>
  <key>WorkingDirectory</key>{data}
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
  </dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key>{stderr}
  <key>StandardErrorPath</key>{stderr}
</dict>
</plist>
"#,
        label = s(LAUNCHD_LABEL),
        bin = s(&spec.bin),
        log = p(&spec.log_file()),
        data = p(data),
        path = s(LAUNCHD_PATH),
        rust_log = s(SERVICE_RUST_LOG),
        stderr = p(&stderr),
    )
}

/// One systemd value with specifiers (`%`) escaped.
fn systemd_escape_percent(s: &str) -> String {
    s.replace('%', "%%")
}

/// One `ExecStart=` argument: `%` → `%%`, `$` → `$$`, and double-quoted (with `\"` / `\\`)
/// when it holds whitespace, quotes, backslashes or `;`.
pub fn systemd_quote_arg(arg: &str) -> String {
    let escaped = systemd_escape_percent(arg).replace('$', "$$");
    let needs = escaped.is_empty()
        || escaped
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'));
    if !needs {
        return escaped;
    }
    let mut out = String::from("\"");
    for c in escaped.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// An `Environment=` line, quoted as a whole when the assignment holds whitespace or quotes.
fn systemd_env(key: &str, value: &str) -> String {
    let assignment = systemd_escape_percent(&format!("{key}={value}"));
    if assignment
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\\'))
    {
        let mut out = String::from("Environment=\"");
        for c in assignment.chars() {
            if matches!(c, '"' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        format!("Environment={assignment}")
    }
}

/// The systemd user unit (M2 §10.4), byte for byte.
pub fn render_unit(spec: &ServiceSpec) -> String {
    let log = spec.log_file().display().to_string();
    let data = spec.data_dir.display().to_string();
    let exec = [spec.bin.as_str(), "serve", "--log-file", log.as_str()]
        .iter()
        .map(|a| systemd_quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]
Description=kioku shared memory server for AI coding agents
After=network.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
ExecStart={exec}
{data_env}
{log_env}
WorkingDirectory={workdir}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
",
        data_env = systemd_env("KIOKU_DATA_DIR", &data),
        log_env = systemd_env("RUST_LOG", SERVICE_RUST_LOG),
        workdir = systemd_escape_percent(&data),
    )
}

/// Instructions printed where no supported service manager exists (M2 §10.5).
pub fn fallback_instructions(spec: &ServiceSpec) -> String {
    format!(
        "no launchd and no working `systemctl --user` here (WSL without systemd, a container, another OS).\n\
         Run the server under your own supervisor instead, e.g.:\n  \
         {bin} serve --log-file {log}\n  \
         nohup {bin} serve --log-file {log} >/dev/null 2>&1 &\n\
         or use the Docker image (see docker-compose.yml in the kioku repository).",
        bin = spec.bin,
        log = spec.log_file().display(),
    )
}

// ---------------------------------------------------------------------------------------
// Command runner (real or recording)
// ---------------------------------------------------------------------------------------

/// Result of one external command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit status 0.
    pub success: bool,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr (or the spawn error).
    pub stderr: String,
}

impl CmdOutput {
    /// A successful result with `stdout`.
    pub fn ok(stdout: &str) -> CmdOutput {
        CmdOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    /// A failed result with `stderr`.
    pub fn fail(stderr: &str) -> CmdOutput {
        CmdOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }
}

type Responder = Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>;

struct Recorder {
    calls: Mutex<Vec<Vec<String>>>,
    respond: Responder,
}

/// Runs external commands — for real, or (tests, dry runs) recording each argv and
/// answering from a closure instead of executing anything.
#[derive(Clone)]
pub struct Runner {
    recorder: Option<Arc<Recorder>>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.recorder.is_some() {
            "Runner(recording)"
        } else {
            "Runner(real)"
        })
    }
}

impl Runner {
    /// Executes commands.
    pub fn real() -> Runner {
        Runner { recorder: None }
    }

    /// Records commands and answers each with `respond(argv)`; executes nothing.
    pub fn recording(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Runner {
        Runner {
            recorder: Some(Arc::new(Recorder {
                calls: Mutex::new(Vec::new()),
                respond: Box::new(respond),
            })),
        }
    }

    /// True for a recording runner.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Every argv run so far (recording runner; empty for a real one).
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.recorder
            .as_ref()
            .map(|r| r.calls.lock().clone())
            .unwrap_or_default()
    }

    /// Clears the recorded calls.
    pub fn clear_calls(&self) {
        if let Some(r) = &self.recorder {
            r.calls.lock().clear();
        }
    }

    /// Runs `argv` (program first) and captures its output; a spawn failure is a failed
    /// result, never an error.
    pub fn run(&self, argv: &[&str]) -> CmdOutput {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        if let Some(r) = &self.recorder {
            r.calls.lock().push(argv.clone());
            return (r.respond)(&argv);
        }
        let Some((program, args)) = argv.split_first() else {
            return CmdOutput::fail("empty command");
        };
        match std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(out) => CmdOutput {
                success: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => CmdOutput::fail(&format!("{program}: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Service manager
// ---------------------------------------------------------------------------------------

/// Which service manager this machine has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Platform {
    /// macOS launchd (LaunchAgent in the GUI domain).
    Launchd,
    /// `systemd --user`.
    Systemd,
    /// Neither (reason for the message).
    Unsupported(String),
}

/// Observed state of the service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceState {
    /// The definition file exists.
    pub installed: bool,
    /// launchd: loaded; systemd: `is-active`.
    pub active: bool,
    /// Main process id, when the manager reports one.
    pub pid: Option<u32>,
    /// systemd lingering for the user (`None` elsewhere or unknown).
    pub linger: Option<bool>,
}

/// What an install / uninstall / start / stop did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceAction {
    /// True when the definition was written or removed, or the service (re)started/stopped.
    pub changed: bool,
    /// Report lines.
    pub lines: Vec<String>,
}

/// The user-level service of this machine: platform, paths and the runner used to drive it.
#[derive(Clone, Debug)]
pub struct ServiceManager {
    /// Detected (or, in tests, given) platform.
    pub platform: Platform,
    /// Command runner.
    pub runner: Runner,
    /// Home directory (`~/Library/LaunchAgents`).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, default `~/.config` (systemd user units).
    pub config_home: PathBuf,
    /// What the service runs.
    pub spec: ServiceSpec,
    /// Login name for `loginctl` (`$USER`; `id -un` when unknown).
    pub user: Option<String>,
}

impl ServiceManager {
    /// Detects the platform: launchd on macOS, `systemd --user` when `systemctl --user
    /// show-environment` works, else unsupported (M2 §10.5).
    pub fn detect(
        runner: Runner,
        home: PathBuf,
        config_home: PathBuf,
        spec: ServiceSpec,
        user: Option<String>,
    ) -> ServiceManager {
        let platform = if cfg!(target_os = "macos") {
            Platform::Launchd
        } else if cfg!(target_os = "windows") {
            Platform::Unsupported("Windows is not supported".into())
        } else if runner
            .run(&["systemctl", "--user", "show-environment"])
            .success
        {
            Platform::Systemd
        } else {
            Platform::Unsupported("`systemctl --user show-environment` failed".into())
        };
        ServiceManager {
            platform,
            runner,
            home,
            config_home,
            spec,
            user,
        }
    }

    /// Manager for the real machine: platform detected, `$XDG_CONFIG_HOME` / `$USER` from
    /// `env`.
    pub fn from_env(
        runner: Runner,
        home: &Path,
        env: &std::collections::HashMap<String, String>,
        spec: ServiceSpec,
    ) -> ServiceManager {
        let mut m = ServiceManager::with_platform(
            Platform::Unsupported(String::new()),
            runner,
            home,
            env,
            spec,
        );
        m.platform = ServiceManager::detect(
            m.runner.clone(),
            m.home.clone(),
            m.config_home.clone(),
            m.spec.clone(),
            None,
        )
        .platform;
        m
    }

    /// [`ServiceManager::from_env`] with a given platform (no detection command runs).
    pub fn with_platform(
        platform: Platform,
        runner: Runner,
        home: &Path,
        env: &std::collections::HashMap<String, String>,
        spec: ServiceSpec,
    ) -> ServiceManager {
        let config_home = env
            .get("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty() && Path::new(v).is_absolute())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let user = env
            .get("USER")
            .or_else(|| env.get("LOGNAME"))
            .filter(|v| !v.is_empty())
            .cloned();
        ServiceManager {
            platform,
            runner,
            home: home.to_path_buf(),
            config_home,
            spec,
            user,
        }
    }

    /// Where the definition lives (`None` on an unsupported platform).
    pub fn definition_path(&self) -> Option<PathBuf> {
        match self.platform {
            Platform::Launchd => Some(
                self.home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{LAUNCHD_LABEL}.plist")),
            ),
            Platform::Systemd => Some(
                self.config_home
                    .join("systemd")
                    .join("user")
                    .join(SYSTEMD_UNIT),
            ),
            Platform::Unsupported(_) => None,
        }
    }

    /// The definition this manager writes.
    pub fn render(&self) -> Option<String> {
        match self.platform {
            Platform::Launchd => Some(render_plist(&self.spec)),
            Platform::Systemd => Some(render_unit(&self.spec)),
            Platform::Unsupported(_) => None,
        }
    }

    /// `launchd dev.kioku.serve` / `systemd kioku.service` / `no service manager`.
    pub fn describe(&self) -> String {
        match self.platform {
            Platform::Launchd => format!("launchd {LAUNCHD_LABEL}"),
            Platform::Systemd => format!("systemd {SYSTEMD_UNIT}"),
            Platform::Unsupported(_) => "no service manager".to_string(),
        }
    }

    /// True when the definition file exists.
    pub fn is_installed(&self) -> bool {
        self.definition_path().is_some_and(|p| p.is_file())
    }

    fn unsupported(&self) -> anyhow::Error {
        anyhow::anyhow!("{}", fallback_instructions(&self.spec))
    }

    fn uid(&self) -> anyhow::Result<String> {
        let out = self.runner.run(&["id", "-u"]);
        let uid = out.stdout.trim().to_string();
        if !out.success || uid.is_empty() {
            anyhow::bail!("`id -u` failed: {}", out.stderr.trim());
        }
        Ok(uid)
    }

    fn user_name(&self) -> Option<String> {
        self.user.clone().or_else(|| {
            let out = self.runner.run(&["id", "-un"]);
            let u = out.stdout.trim().to_string();
            (out.success && !u.is_empty()).then_some(u)
        })
    }

    fn systemctl(&self, args: &[&str]) -> CmdOutput {
        let mut argv = vec!["systemctl", "--user"];
        argv.extend_from_slice(args);
        self.runner.run(&argv)
    }

    /// Current state (installed / active / pid / linger).
    pub fn state(&self) -> ServiceState {
        let installed = self.is_installed();
        match self.platform {
            Platform::Launchd => {
                let Ok(uid) = self.uid() else {
                    return ServiceState {
                        installed,
                        ..ServiceState::default()
                    };
                };
                let out =
                    self.runner
                        .run(&["launchctl", "print", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
                ServiceState {
                    installed,
                    active: out.success,
                    pid: out
                        .success
                        .then(|| parse_launchd_pid(&out.stdout))
                        .flatten(),
                    linger: None,
                }
            }
            Platform::Systemd => {
                let active = self.systemctl(&["is-active", SYSTEMD_UNIT]).success;
                let pid = if active {
                    let out = self.systemctl(&["show", "-p", "MainPID", "--value", SYSTEMD_UNIT]);
                    out.stdout.trim().parse().ok().filter(|p: &u32| *p > 0)
                } else {
                    None
                };
                ServiceState {
                    installed,
                    active,
                    pid,
                    linger: self.linger(),
                }
            }
            Platform::Unsupported(_) => ServiceState {
                installed,
                ..ServiceState::default()
            },
        }
    }

    /// systemd lingering of the user (`loginctl show-user <user> -p Linger`).
    pub fn linger(&self) -> Option<bool> {
        let user = self.user_name()?;
        let out = self
            .runner
            .run(&["loginctl", "show-user", &user, "-p", "Linger"]);
        if !out.success {
            return None;
        }
        match out.stdout.trim() {
            "Linger=yes" => Some(true),
            "Linger=no" => Some(false),
            _ => None,
        }
    }

    /// Writes the definition when its content differs; returns whether it was written.
    fn write_definition(&self) -> anyhow::Result<bool> {
        let (Some(path), Some(content)) = (self.definition_path(), self.render()) else {
            return Err(self.unsupported());
        };
        if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
            return Ok(false);
        }
        let logs = self.spec.data_dir.join("logs");
        kioku_core::util::create_private_dir(&logs)
            .with_context(|| format!("creating {}", logs.display()))?;
        let existed = path.exists();
        crate::install::write_definition_file(&path, existed, &content)?;
        Ok(true)
    }

    /// Commands `install` would run on a machine where the service is not installed yet
    /// (the `--dry-run` plan).
    pub fn plan_install(&self) -> Vec<String> {
        let Some(path) = self.definition_path() else {
            return Vec::new();
        };
        let unchanged = self
            .render()
            .is_some_and(|c| std::fs::read_to_string(&path).ok().as_deref() == Some(c.as_str()));
        let mut out = Vec::new();
        if !unchanged {
            out.push(format!("write {}", path.display()));
        }
        match self.platform {
            Platform::Launchd => {
                let svc = format!("gui/<uid>/{LAUNCHD_LABEL}");
                out.push(format!("launchctl bootout {svc} (if loaded)"));
                out.push(format!("launchctl bootstrap gui/<uid> {}", path.display()));
                out.push(format!("launchctl enable {svc}"));
            }
            Platform::Systemd => {
                out.push("systemctl --user daemon-reload".into());
                out.push(format!("systemctl --user enable --now {SYSTEMD_UNIT}"));
                out.push("loginctl enable-linger (if lingering is off)".into());
            }
            Platform::Unsupported(_) => {}
        }
        out
    }

    /// Writes the definition, enables and starts the service. Idempotent: rewrites and
    /// restarts only when the content changed; starts it when it is not running.
    pub fn install(&self) -> anyhow::Result<ServiceAction> {
        let mut act = ServiceAction::default();
        match self.platform {
            Platform::Unsupported(_) => return Err(self.unsupported()),
            Platform::Launchd => {
                let uid = self.uid()?;
                let path = self.definition_path().unwrap_or_default();
                let written = self.write_definition()?;
                let svc = format!("gui/{uid}/{LAUNCHD_LABEL}");
                let loaded = self.runner.run(&["launchctl", "print", &svc]).success;
                if written {
                    act.lines.push(format!("wrote {}", path.display()));
                }
                if written || !loaded {
                    // A changed plist is only read at bootstrap: unload the old job first.
                    if loaded {
                        let _ = self.runner.run(&["launchctl", "bootout", &svc]);
                    }
                    self.launchd_bootstrap(&uid, &path)?;
                    let _ = self.runner.run(&["launchctl", "enable", &svc]);
                    act.lines.push(if loaded {
                        format!("reloaded and restarted {svc}")
                    } else {
                        format!("loaded and started {svc}")
                    });
                    act.changed = true;
                }
                act.changed |= written;
            }
            Platform::Systemd => {
                let path = self.definition_path().unwrap_or_default();
                let was_active = self.systemctl(&["is-active", SYSTEMD_UNIT]).success;
                let written = self.write_definition()?;
                if written {
                    act.lines.push(format!("wrote {}", path.display()));
                    let out = self.systemctl(&["daemon-reload"]);
                    if !out.success {
                        anyhow::bail!(
                            "systemctl --user daemon-reload failed: {}",
                            out.stderr.trim()
                        );
                    }
                }
                if written || !was_active {
                    let out = self.systemctl(&["enable", "--now", SYSTEMD_UNIT]);
                    if !out.success {
                        anyhow::bail!(
                            "systemctl --user enable --now {SYSTEMD_UNIT} failed: {}",
                            out.stderr.trim()
                        );
                    }
                    if written && was_active {
                        let out = self.systemctl(&["restart", SYSTEMD_UNIT]);
                        if !out.success {
                            anyhow::bail!(
                                "systemctl --user restart {SYSTEMD_UNIT} failed: {}",
                                out.stderr.trim()
                            );
                        }
                        act.lines.push(format!("restarted {SYSTEMD_UNIT}"));
                    } else {
                        act.lines
                            .push(format!("enabled and started {SYSTEMD_UNIT}"));
                    }
                    act.changed = true;
                }
                act.lines.extend(self.ensure_linger());
            }
        }
        if !act.changed {
            act.lines
                .push(format!("{} unchanged and running", self.describe()));
        }
        Ok(act)
    }

    /// Turns on lingering when it is off; a failure yields the optional sudo hint (the only
    /// place kioku mentions sudo, M2 §10.4).
    fn ensure_linger(&self) -> Vec<String> {
        if self.linger() != Some(false) {
            return Vec::new();
        }
        if self.runner.run(&["loginctl", "enable-linger"]).success {
            return vec!["enabled lingering (the service keeps running after logout)".into()];
        }
        let user = self.user_name().unwrap_or_else(|| "$USER".into());
        vec![format!(
            "note: lingering is off, so the service stops when you log out. Only needed if kioku must run while you are logged out (e.g. on a home server): sudo loginctl enable-linger {user}"
        )]
    }

    /// Stops, disables and removes the definition.
    pub fn uninstall(&self) -> anyhow::Result<ServiceAction> {
        let mut act = ServiceAction::default();
        let Some(path) = self.definition_path() else {
            return Err(self.unsupported());
        };
        match self.platform {
            Platform::Launchd => {
                let uid = self.uid()?;
                let _ = self.runner.run(&[
                    "launchctl",
                    "bootout",
                    &format!("gui/{uid}/{LAUNCHD_LABEL}"),
                ]);
            }
            Platform::Systemd => {
                let _ = self.systemctl(&["disable", "--now", SYSTEMD_UNIT]);
            }
            Platform::Unsupported(_) => {}
        }
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            act.changed = true;
            act.lines.push(format!("removed {}", path.display()));
            if self.platform == Platform::Systemd {
                let _ = self.systemctl(&["daemon-reload"]);
            }
        } else {
            act.lines
                .push(format!("not installed ({} does not exist)", path.display()));
        }
        Ok(act)
    }

    /// `launchctl bootstrap gui/<uid> <plist>`, retried up to [`BOOTSTRAP_ATTEMPTS`] times
    /// [`BOOTSTRAP_RETRY_DELAY`] apart (no sleeping with a recording runner).
    fn launchd_bootstrap(&self, uid: &str, path: &Path) -> anyhow::Result<()> {
        let domain = format!("gui/{uid}");
        let plist = path.display().to_string();
        let mut last = CmdOutput::default();
        for attempt in 0..BOOTSTRAP_ATTEMPTS {
            if attempt > 0 && !self.runner.is_recording() {
                std::thread::sleep(BOOTSTRAP_RETRY_DELAY);
            }
            last = self
                .runner
                .run(&["launchctl", "bootstrap", &domain, &plist]);
            if last.success {
                return Ok(());
            }
        }
        anyhow::bail!(
            "launchctl bootstrap failed ({BOOTSTRAP_ATTEMPTS} attempts): {}",
            last.stderr.trim()
        )
    }

    /// launchd: `kickstart -k` a loaded job (kills and restarts it in place), bootstraps an
    /// unloaded one. Returns true when the job was already loaded. A job still being torn
    /// down by a just-issued `bootout` (`service stop` then `start`) still answers `print`
    /// but refuses `kickstart`: it is bootstrapped again instead (with the retries).
    fn launchd_start_or_restart(&self) -> anyhow::Result<bool> {
        let uid = self.uid()?;
        let svc = format!("gui/{uid}/{LAUNCHD_LABEL}");
        if self.runner.run(&["launchctl", "print", &svc]).success {
            let out = self.runner.run(&["launchctl", "kickstart", "-k", &svc]);
            if out.success {
                return Ok(true);
            }
            let path = self.definition_path().unwrap_or_default();
            self.launchd_bootstrap(&uid, &path).with_context(|| {
                format!(
                    "launchctl kickstart -k {svc} failed ({})",
                    out.stderr.trim()
                )
            })?;
            let _ = self.runner.run(&["launchctl", "enable", &svc]);
            return Ok(false);
        }
        let path = self.definition_path().unwrap_or_default();
        self.launchd_bootstrap(&uid, &path)?;
        let _ = self.runner.run(&["launchctl", "enable", &svc]);
        Ok(false)
    }

    /// Starts an installed service (launchd: a loaded job is restarted with `kickstart -k`).
    pub fn start(&self) -> anyhow::Result<ServiceAction> {
        self.require_installed()?;
        let verb = match self.platform {
            Platform::Launchd => {
                if self.launchd_start_or_restart()? {
                    "restarted"
                } else {
                    "started"
                }
            }
            _ => {
                let out = self.systemctl(&["start", SYSTEMD_UNIT]);
                if !out.success {
                    anyhow::bail!("systemctl --user start failed: {}", out.stderr.trim());
                }
                "started"
            }
        };
        Ok(ServiceAction {
            changed: true,
            lines: vec![format!("{verb} {}", self.describe())],
        })
    }

    /// Restarts an installed service so it runs the current binary: `launchctl kickstart -k`
    /// (bootstrap when not loaded) / `systemctl --user restart`.
    pub fn restart(&self) -> anyhow::Result<ServiceAction> {
        self.require_installed()?;
        match self.platform {
            Platform::Launchd => {
                self.launchd_start_or_restart()?;
            }
            _ => {
                let out = self.systemctl(&["restart", SYSTEMD_UNIT]);
                if !out.success {
                    anyhow::bail!(
                        "systemctl --user restart {SYSTEMD_UNIT} failed: {}",
                        out.stderr.trim()
                    );
                }
            }
        }
        Ok(ServiceAction {
            changed: true,
            lines: vec![format!("restarted {}", self.describe())],
        })
    }

    /// Stops the service (launchd: unloads it; it returns at next login unless uninstalled).
    pub fn stop(&self) -> anyhow::Result<ServiceAction> {
        self.require_installed()?;
        match self.platform {
            Platform::Launchd => {
                let uid = self.uid()?;
                let _ = self.runner.run(&[
                    "launchctl",
                    "bootout",
                    &format!("gui/{uid}/{LAUNCHD_LABEL}"),
                ]);
            }
            _ => {
                let out = self.systemctl(&["stop", SYSTEMD_UNIT]);
                if !out.success {
                    anyhow::bail!("systemctl --user stop failed: {}", out.stderr.trim());
                }
            }
        }
        let mut lines = vec![format!("stopped {}", self.describe())];
        if self.platform == Platform::Launchd {
            lines.push("note: a stopped LaunchAgent starts again at the next login (RunAtLoad) unless you run `kioku service uninstall`".into());
        }
        Ok(ServiceAction {
            changed: true,
            lines,
        })
    }

    fn require_installed(&self) -> anyhow::Result<()> {
        let Some(path) = self.definition_path() else {
            return Err(self.unsupported());
        };
        if !path.is_file() {
            anyhow::bail!(
                "the service is not installed ({} does not exist); run `kioku service install`",
                path.display()
            );
        }
        Ok(())
    }
}

/// `pid = N` from `launchctl print` (optional; the format is not a stable API).
pub fn parse_launchd_pid(text: &str) -> Option<u32> {
    text.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("pid = "))
        .and_then(|v| v.trim().parse().ok())
}

// ---------------------------------------------------------------------------------------
// Health probe
// ---------------------------------------------------------------------------------------

/// What answers at the configured server URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// A kioku server (its version).
    Kioku {
        /// Server version.
        version: String,
    },
    /// Something that is not kioku owns the port.
    Foreign(String),
    /// Nothing is listening (or the host is unreachable).
    Down(String),
}

/// `GET <server_url>/api/v1/health`; when it fails, a plain TCP connect tells a foreign
/// listener from a free port.
pub fn probe_health(client: &ClientConfig, timeout: Duration) -> Health {
    let api = match ApiClient::new(client, timeout) {
        Ok(a) => a,
        Err(e) => return Health::Down(format!("{e:#}")),
    };
    match api.get(&["health"], &[]) {
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Health::Kioku {
            version: v
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Ok(_) => Health::Foreign("GET /api/v1/health did not return kioku's answer".into()),
        Err(e) if e.downcast_ref::<HttpError>().is_some() => Health::Foreign(format!(
            "GET /api/v1/health returned HTTP {}",
            http_status(&e).unwrap_or(0)
        )),
        Err(e) => {
            if port_open(&client.server_url, timeout) {
                Health::Foreign(format!("the port answers, but not as kioku ({e:#})"))
            } else {
                Health::Down(format!("{e:#}"))
            }
        }
    }
}

/// True when a TCP connection to the URL's host:port succeeds.
pub fn port_open(url: &str, timeout: Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(url) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, timeout.min(Duration::from_secs(2))).is_ok())
}

// ---------------------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------------------

/// The last `n` lines of `text`.
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `kioku service logs [-f] [-n N]`: prints the tail of `path`, then (with `follow`) new
/// lines as they arrive, reopening the file after a rotation.
pub fn tail_log(path: &Path, n: usize, follow: bool) -> anyhow::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    if !path.exists() && !follow {
        anyhow::bail!(
            "{} does not exist yet (the service writes it once it starts)",
            path.display()
        );
    }
    let text = std::fs::read(path).unwrap_or_default();
    let mut out = std::io::stdout();
    out.write_all(last_lines(&String::from_utf8_lossy(&text), n).as_bytes())?;
    out.flush()?;
    if !follow {
        return Ok(());
    }
    let mut pos = text.len() as u64;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(mut f) = std::fs::File::open(path) else {
            continue;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0; // rotated
        }
        if len == pos {
            continue;
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        pos += buf.len() as u64;
        out.write_all(&buf)?;
        out.flush()?;
    }
}

#[cfg(test)]
mod tests;
