//! `kioku setup` (M2 §11): one idempotent, non-interactive command that takes a machine
//! from "binary installed" to "every agent wired up" — binary → config (`init` or
//! `--client-only`) → user-level service → auth check → `install all` → summary.
//!
//! Everything the steps read from the environment comes in through [`SetupEnv`] (vars,
//! home, binary, command runner), so tests run the whole flow on a temp HOME against an
//! in-process server without touching the real environment, launchctl or systemctl.

use anyhow::Context;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kioku_core::config::CONFIG_FILE;
use kioku_core::{ClientConfig, Config};

use crate::client::{ApiClient, http_status};
use crate::event::Agent;
use crate::install::agents::{
    AgentReport, AllStatus, InstallCtx, InstallOptions, Instructions, hook_specs, hooks_path,
    install_all, instruction_files, is_detected, mcp_path, unstable_binary_warning,
    wants_instructions,
};
use crate::install::{HookPlatform, block};
use crate::service::{Health, Platform, Runner, ServiceManager, ServiceSpec, probe_health};

/// GitHub repository of kioku (`KIOKU_REPO` / `--repo` default of install.sh).
pub const KIOKU_REPO: &str = "misorafa/kioku";

/// Raw URL of `install.sh` on the default branch.
pub fn install_sh_url() -> String {
    format!("https://raw.githubusercontent.com/{KIOKU_REPO}/main/install.sh")
}

/// Raw URL of `install.ps1` (the Windows installer) on the default branch.
pub fn install_ps1_url() -> String {
    format!("https://raw.githubusercontent.com/{KIOKU_REPO}/main/install.ps1")
}

/// Why `kioku setup` without `--client-only` stops on Windows (SPEC-M2.2 §4.7).
pub const WINDOWS_CLIENT_ONLY: &str = "Windows runs kioku as a client: kioku setup --client-only <url> <token> (the server runs on macOS/Linux; easiest: run kioku invite there and paste its Windows line here)";

/// Version of this binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Flags of `kioku setup`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupOptions {
    /// `--client-only <url> <token>`.
    pub client_only: Option<(String, String)>,
    /// `--no-service`.
    pub no_service: bool,
    /// `--no-agents`.
    pub no_agents: bool,
    /// `--agents a,b`.
    pub agents: Vec<Agent>,
    /// `--bind <addr>` (written to `[server] bind` when the config is created).
    pub bind: Option<String>,
    /// `--no-instructions`.
    pub no_instructions: bool,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--mcp-http`: URL + token MCP entries instead of the stdio bridge (M2 §20.2).
    pub mcp_http: bool,
    /// `--print-client-command` (prints the token).
    pub print_client_command: bool,
}

/// The environment setup runs in.
#[derive(Clone, Debug)]
pub struct SetupEnv {
    /// Environment variables (`KIOKU_*`, `CODEX_HOME`, `XDG_CONFIG_HOME`, `USER`).
    pub vars: HashMap<String, String>,
    /// Home directory.
    pub home: PathBuf,
    /// Working directory.
    pub cwd: PathBuf,
    /// Absolute path of the kioku binary.
    pub bin: String,
    /// Runs launchctl / systemctl / loginctl (a recording runner in tests).
    pub runner: Runner,
    /// Forces the service platform (tests); `None` detects it.
    pub platform: Option<Platform>,
    /// The OS kioku runs on: hook command shapes, and Windows is client-only (SPEC-M2.2).
    pub hook_platform: HookPlatform,
    /// Timeout of one health / status request.
    pub request_timeout: Duration,
    /// Health polling after `service install`: interval and total.
    pub poll_interval: Duration,
    /// See `poll_interval`.
    pub poll_timeout: Duration,
}

impl SetupEnv {
    /// The real process environment.
    pub fn from_process(bin: String) -> anyhow::Result<SetupEnv> {
        Ok(SetupEnv {
            vars: kioku_core::util::env_vars(),
            home: kioku_core::util::home_dir(),
            cwd: std::env::current_dir()?,
            bin,
            runner: Runner::real(),
            platform: None,
            hook_platform: HookPlatform::current(),
            request_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(200),
            poll_timeout: Duration::from_secs(15),
        })
    }

    /// Directory holding config.toml: `$KIOKU_DATA_DIR`, else `~/.kioku`.
    pub fn config_dir(&self) -> PathBuf {
        match self.vars.get("KIOKU_DATA_DIR").filter(|v| !v.is_empty()) {
            Some(d) if d == "~" => self.home.clone(),
            Some(d) => match d.strip_prefix("~/") {
                Some(rest) => self.home.join(rest),
                None => PathBuf::from(d),
            },
            None => self.home.join(".kioku"),
        }
    }

    /// `$CODEX_HOME`, default `~/.codex`.
    pub fn codex_home(&self) -> PathBuf {
        self.vars
            .get("CODEX_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".codex"))
    }

    /// Installer context for `client`.
    pub fn install_ctx(&self, client: &ClientConfig) -> InstallCtx {
        InstallCtx {
            home: self.home.clone(),
            codex_home: self.codex_home(),
            cwd: self.cwd.clone(),
            bin: self.bin.clone(),
            client: client.clone(),
            platform: self.hook_platform,
        }
    }

    /// The service manager for a data directory.
    pub fn service_manager(&self, data_dir: &Path) -> ServiceManager {
        let spec = ServiceSpec {
            bin: self.bin.clone(),
            data_dir: data_dir.to_path_buf(),
        };
        match &self.platform {
            Some(p) => ServiceManager::with_platform(
                p.clone(),
                self.runner.clone(),
                &self.home,
                &self.vars,
                spec,
            ),
            None => ServiceManager::from_env(self.runner.clone(), &self.home, &self.vars, spec),
        }
    }
}

/// Summary marker of one step (ASCII only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Done.
    Ok,
    /// Skipped.
    Skip,
    /// Done with a warning.
    Warn,
    /// Failed.
    Fail,
}

impl Mark {
    /// `ok` / `--` / `!!` / `xx`.
    pub fn as_str(self) -> &'static str {
        match self {
            Mark::Ok => "ok",
            Mark::Skip => "--",
            Mark::Warn => "!!",
            Mark::Fail => "xx",
        }
    }
}

/// One summary line (plus indented detail lines, dry run only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepLine {
    /// Marker.
    pub mark: Mark,
    /// Step or agent name.
    pub step: String,
    /// Text.
    pub text: String,
    /// Indented details (the dry-run plan).
    pub details: Vec<String>,
}

/// Everything `setup` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupReport {
    /// Summary lines in step order.
    pub lines: Vec<StepLine>,
    /// Lines after the summary.
    pub footer: Vec<String>,
}

impl SetupReport {
    fn push(&mut self, mark: Mark, step: &str, text: impl Into<String>) {
        self.lines.push(StepLine {
            mark,
            step: step.to_string(),
            text: text.into(),
            details: Vec::new(),
        });
    }

    fn details(&mut self, details: Vec<String>) {
        if let Some(last) = self.lines.last_mut() {
            last.details.extend(details);
        }
    }

    /// 0 when no step failed, else 1.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.lines.iter().any(|l| l.mark == Mark::Fail))
    }

    /// The summary lines only (`  ok  binary      /path`), without header and footer.
    pub fn summary_lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| format!("  {}  {:<11} {}", l.mark.as_str(), l.step, l.text))
            .collect()
    }

    /// The full text printed by `kioku setup`.
    pub fn render(&self) -> String {
        let mut out = format!("kioku setup (v{VERSION})\n");
        for (line, text) in self.lines.iter().zip(self.summary_lines()) {
            out.push_str(&text);
            out.push('\n');
            for d in &line.details {
                for l in d.lines() {
                    out.push_str(&format!("        {l}\n"));
                }
            }
        }
        for f in &self.footer {
            out.push_str(f);
            out.push('\n');
        }
        out
    }
}

/// `path` with the home directory abbreviated to `~`; the rest is joined with `/` on every
/// OS, so Windows shows `~/.claude/settings.json`, not `~/.claude\settings.json`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => {
            let parts: Vec<String> = rest
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            format!("~/{}", parts.join("/"))
        }
        Err(_) => path.display().to_string(),
    }
}

pub(crate) fn has_server_section(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .is_some_and(|t| t.contains_key("server"))
}

/// Authenticated `GET /api/v1/status`.
pub(crate) fn check_status(client: &ClientConfig, timeout: Duration) -> anyhow::Result<()> {
    ApiClient::new(client, timeout)?.get(&["status"], &[])?;
    Ok(())
}

fn auth_failure(err: &anyhow::Error, config_file: &Path) -> String {
    match http_status(err) {
        Some(401) => format!(
            "the server rejected the token (HTTP 401): [client] auth_token in {} does not match the server's",
            config_file.display()
        ),
        _ => format!("GET /api/v1/status failed: {err:#}"),
    }
}

/// Runs `kioku setup`; nothing is printed (the caller prints [`SetupReport::render`]).
pub fn run_setup(opts: &SetupOptions, env: &SetupEnv) -> SetupReport {
    let mut r = SetupReport {
        lines: Vec::new(),
        footer: Vec::new(),
    };

    // SPEC-M2.2 §4.7: Windows is a client only.
    if env.hook_platform == HookPlatform::Windows && opts.client_only.is_none() {
        r.push(Mark::Fail, "setup", WINDOWS_CLIENT_ONLY);
        return r;
    }

    // 1. Binary
    match unstable_binary_warning(&env.bin) {
        None => r.push(Mark::Ok, "binary", env.bin.clone()),
        Some(_) => r.push(
            Mark::Warn,
            "binary",
            format!(
                "{} (build/temp location: hooks break when it moves; install with install.sh)",
                env.bin
            ),
        ),
    }

    // 2. Config
    let config_dir = env.config_dir();
    let config_path = config_dir.join(CONFIG_FILE);
    let Some(state) = config_step(opts, env, &config_dir, &config_path, &mut r) else {
        return finish(r, opts, None);
    };

    // 3. Service
    if state.client_only && state.retired_server {
        retire_service_step(opts, env, &state.cfg, &mut r);
    } else if state.client_only {
        r.push(
            Mark::Skip,
            "service",
            format!(
                "client-only machine (server at {})",
                state.cfg.client.server_url
            ),
        );
    } else if opts.no_service {
        r.push(Mark::Skip, "service", "skipped (--no-service)");
    } else {
        service_step(opts, env, &state.cfg, &mut r);
    }

    // 4. Auth check (client-only mode checked it before writing the config)
    if !state.client_only {
        if opts.dry_run && !state.exists {
            r.push(
                Mark::Skip,
                "auth",
                "skipped (dry run: config.toml not written yet)",
            );
        } else {
            match check_status(&state.cfg.client, env.request_timeout) {
                Ok(()) => r.push(
                    Mark::Ok,
                    "auth",
                    format!("token accepted by {}", state.cfg.client.server_url),
                ),
                // --no-service on a server machine (a container, a host without launchd /
                // systemd): nobody has started `kioku serve` yet, which is not a failure.
                // An HTTP answer (401, 5xx) still is.
                Err(e) if opts.no_service && http_status(&e).is_none() => r.push(
                    Mark::Skip,
                    "auth",
                    format!(
                        "no server answers at {} yet (--no-service): start it with `kioku serve`, then check with `kioku doctor`",
                        state.cfg.client.server_url
                    ),
                ),
                Err(e) => r.push(Mark::Fail, "auth", auth_failure(&e, &config_path)),
            }
        }
    }

    // 5. Agents
    if opts.no_agents {
        r.push(Mark::Skip, "agents", "skipped (--no-agents)");
    } else {
        agents_step(opts, env, &state.cfg.client, &mut r);
    }

    finish(r, opts, Some(&state))
}

struct ConfigState {
    cfg: Config,
    client_only: bool,
    /// config.toml exists (false only in a dry run that would create it).
    exists: bool,
    /// `--client-only` dropped this machine's own `[server]` (it had been a server).
    retired_server: bool,
}

/// Backup of a former server machine's config.toml, next to it (SPEC-M2 §11 step 2).
pub const SERVER_CONFIG_BACKUP: &str = "config.toml.server-bak";

/// Copies config.toml (it holds the server token) to `backup`, 0600.
fn backup_config(config_path: &Path, backup: &Path) -> anyhow::Result<()> {
    let old = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    kioku_core::util::write_private_file(backup, &old)
        .with_context(|| format!("writing {}", backup.display()))
}

/// True when `url` is this machine's own server: a loopback host on `[server] port`.
pub(crate) fn points_at_own_server(url: &str, cfg: &Config) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    loopback && u.port_or_known_default() == Some(cfg.server.port)
}

fn config_step(
    opts: &SetupOptions,
    env: &SetupEnv,
    config_dir: &Path,
    config_path: &Path,
    r: &mut SetupReport,
) -> Option<ConfigState> {
    if let Some((url, token)) = &opts.client_only {
        let token = token.trim();
        if reqwest::Url::parse(url).is_err() {
            r.push(Mark::Fail, "config", format!("invalid server URL: {url}"));
            return None;
        }
        if token.is_empty() {
            r.push(Mark::Fail, "config", "the auth token must not be empty");
            return None;
        }
        let mut cfg = match Config::load_from_dir(config_dir, &env.vars) {
            Ok(c) => c,
            Err(e) => {
                r.push(Mark::Fail, "config", format!("{e:#}"));
                return None;
            }
        };
        cfg.client.server_url = url.clone();
        cfg.client.auth_token = Some(token.to_string());
        // Verify before writing: a wrong token never replaces a working config.
        if let Err(e) = check_status(&cfg.client, env.request_timeout) {
            r.push(
                Mark::Fail,
                "config",
                format!(
                    "{} not written: {}",
                    config_path.display(),
                    auth_failure(&e, config_path)
                ),
            );
            return None;
        }
        // A former server machine switching to another server: retire its [server].
        let retire_server = config_path.exists()
            && has_server_section(config_path)
            && !points_at_own_server(url, &cfg);
        let backup = config_dir.join(SERVER_CONFIG_BACKUP);
        if opts.dry_run {
            r.push(
                Mark::Ok,
                "config",
                format!(
                    "would write [client] to {} (client-only -> {url})",
                    config_path.display()
                ),
            );
            if retire_server {
                r.push(
                    Mark::Ok,
                    "config",
                    format!("would drop [server] (backup: {})", backup.display()),
                );
            }
        } else {
            let written = if retire_server {
                backup_config(config_path, &backup).and_then(|()| {
                    Config::write_client_only_dropping_server(config_path, url, token)
                        .map_err(anyhow::Error::from)
                })
            } else {
                Config::write_client_only(config_path, url, token).map_err(anyhow::Error::from)
            };
            match written {
                Ok(written) => {
                    cfg.client = written.client;
                    r.push(
                        Mark::Ok,
                        "config",
                        format!("{} (client-only -> {url})", config_path.display()),
                    );
                    if retire_server {
                        r.push(
                            Mark::Ok,
                            "config",
                            format!(
                                "dropped this machine's [server] (backup: {})",
                                backup.display()
                            ),
                        );
                    }
                }
                Err(e) => {
                    r.push(Mark::Fail, "config", format!("{e:#}"));
                    return None;
                }
            }
        }
        r.push(Mark::Ok, "auth", format!("token accepted by {url}"));
        return Some(ConfigState {
            cfg,
            client_only: true,
            exists: !opts.dry_run,
            retired_server: retire_server,
        });
    }

    let mut cfg = match Config::load_from_dir(config_dir, &env.vars) {
        Ok(c) => c,
        Err(e) => {
            r.push(Mark::Fail, "config", format!("{e:#}"));
            return None;
        }
    };
    if !config_path.exists() {
        if let Some(bind) = &opts.bind {
            cfg.server.bind = bind.clone();
        }
        if !env.vars.contains_key("KIOKU_SERVER_URL") {
            cfg.client.server_url = format!("http://127.0.0.1:{}", cfg.server.port);
        }
        if opts.dry_run {
            r.push(
                Mark::Ok,
                "config",
                format!(
                    "would create {} with a new token (kioku init, bind {})",
                    config_path.display(),
                    cfg.server.bind
                ),
            );
            return Some(ConfigState {
                cfg,
                client_only: false,
                exists: false,
                retired_server: false,
            });
        }
        return match kioku_core::init(&mut cfg) {
            Ok(rep) => {
                let token = if rep.token_generated {
                    "created, new token"
                } else {
                    "created, token from KIOKU_AUTH_TOKEN"
                };
                r.push(
                    Mark::Ok,
                    "config",
                    format!("{} ({token})", config_path.display()),
                );
                Some(ConfigState {
                    cfg,
                    client_only: false,
                    exists: true,
                    retired_server: false,
                })
            }
            Err(e) => {
                r.push(Mark::Fail, "config", format!("kioku init failed: {e:#}"));
                None
            }
        };
    }

    if !has_server_section(config_path) {
        r.push(
            Mark::Ok,
            "config",
            format!(
                "{} (existing, client-only -> {})",
                config_path.display(),
                cfg.client.server_url
            ),
        );
        if opts.bind.is_some() {
            r.push(
                Mark::Warn,
                "config",
                "--bind ignored: this machine is configured as a client only",
            );
        }
        return Some(ConfigState {
            cfg,
            client_only: true,
            exists: true,
            retired_server: false,
        });
    }
    let has_token = cfg
        .server
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if has_token {
        r.push(
            Mark::Ok,
            "config",
            format!("{} (existing, token kept)", config_path.display()),
        );
    } else if opts.dry_run {
        r.push(
            Mark::Ok,
            "config",
            format!(
                "would add a token to {} (kioku init)",
                config_path.display()
            ),
        );
    } else {
        match kioku_core::init(&mut cfg) {
            Ok(_) => r.push(
                Mark::Ok,
                "config",
                format!("{} (existing, token added)", config_path.display()),
            ),
            Err(e) => {
                r.push(Mark::Fail, "config", format!("kioku init failed: {e:#}"));
                return None;
            }
        }
    }
    if let Some(bind) = &opts.bind
        && bind != &cfg.server.bind
    {
        r.push(
            Mark::Warn,
            "config",
            format!(
                "--bind {bind} ignored: the existing config keeps bind = \"{}\" (edit [server] bind in {})",
                cfg.server.bind,
                config_path.display()
            ),
        );
    }
    Some(ConfigState {
        cfg,
        client_only: false,
        exists: true,
        retired_server: false,
    })
}

/// A former server machine that became a client: its own kioku service would keep serving
/// stale data (and make doctor treat the machine as a server) — remove it; data stays.
fn retire_service_step(opts: &SetupOptions, env: &SetupEnv, cfg: &Config, r: &mut SetupReport) {
    let manager = env.service_manager(&cfg.data_dir);
    let server = &cfg.client.server_url;
    if !manager.is_installed() {
        r.push(
            Mark::Skip,
            "service",
            format!("client-only machine (server at {server})"),
        );
        return;
    }
    if opts.dry_run {
        r.push(
            Mark::Ok,
            "service",
            format!(
                "would remove this machine's {} (now a client of {server})",
                manager.describe()
            ),
        );
        return;
    }
    match manager.uninstall() {
        Ok(_) => r.push(
            Mark::Ok,
            "service",
            format!(
                "removed this machine's {} (now a client of {server}; data in {} kept)",
                manager.describe(),
                cfg.data_dir.display()
            ),
        ),
        Err(e) => r.push(
            Mark::Warn,
            "service",
            format!(
                "could not remove this machine's kioku service: {e:#}; run kioku service uninstall"
            ),
        ),
    }
}

fn service_step(opts: &SetupOptions, env: &SetupEnv, cfg: &Config, r: &mut SetupReport) {
    let url = cfg.client.server_url.clone();
    let manager = env.service_manager(&cfg.data_dir);
    let installed = manager.is_installed();
    match probe_health(&cfg.client, env.request_timeout) {
        Health::Kioku { version } if !installed => {
            r.push(
                Mark::Ok,
                "service",
                format!(
                    "server already running at {url} (v{version}; Docker/k3s?), not installing a service"
                ),
            );
            return;
        }
        Health::Foreign(why) => {
            r.push(
                Mark::Fail,
                "service",
                format!("{url} is in use by something that is not kioku ({why})"),
            );
            return;
        }
        _ => {}
    }
    if let Platform::Unsupported(_) = manager.platform {
        r.push(
            Mark::Warn,
            "service",
            "no launchd / systemd --user here: run `kioku serve` under your own supervisor (see `kioku service install`)",
        );
        return;
    }
    if opts.dry_run {
        r.push(
            Mark::Ok,
            "service",
            format!("would install {}", manager.describe()),
        );
        r.details(manager.plan_install());
        return;
    }
    let act = match manager.install() {
        Ok(a) => a,
        Err(e) => {
            r.push(
                Mark::Fail,
                "service",
                format!("{} install failed: {e:#}", manager.describe()),
            );
            return;
        }
    };
    let mut version = wait_for_health(env, &cfg.client, |_| true);
    // A healthy service still running an older (or other) binary — typically after the
    // binary was replaced — is restarted so it serves this version.
    if let Some(old) = version.clone().filter(|v| v != VERSION) {
        if let Err(e) = manager.restart() {
            r.push(
                Mark::Fail,
                "service",
                format!(
                    "{} runs v{old}, not v{VERSION}, and the restart failed: {e:#}",
                    manager.describe()
                ),
            );
            return;
        }
        version = wait_for_health(env, &cfg.client, |v| v == VERSION);
        match version.as_deref() {
            Some(new) if new == VERSION => r.push(
                Mark::Ok,
                "service",
                format!(
                    "{} running at {url}, restarted (v{old} -> v{new})",
                    manager.describe()
                ),
            ),
            Some(other) => r.push(
                Mark::Warn,
                "service",
                format!(
                    "{} restarted but still reports v{other} (this binary is v{VERSION}); check the binary path in {}",
                    manager.describe(),
                    manager.definition_path().unwrap_or_default().display()
                ),
            ),
            None => r.push(
                Mark::Fail,
                "service",
                format!(
                    "{} restarted, but {url} did not answer within {} s; see `kioku service logs`",
                    manager.describe(),
                    env.poll_timeout.as_secs()
                ),
            ),
        }
    } else {
        push_health_line(&manager, &url, env, version, r);
    }
    for note in act.lines.iter().filter(|l| l.starts_with("note: ")) {
        r.push(Mark::Warn, "service", note.trim_start_matches("note: "));
    }
}

/// Polls `GET /health` until it reports a kioku server whose version satisfies `accept`
/// (or `poll_timeout` passes); returns the last version seen.
fn wait_for_health(
    env: &SetupEnv,
    client: &ClientConfig,
    accept: impl Fn(&str) -> bool,
) -> Option<String> {
    let deadline = Instant::now() + env.poll_timeout;
    let mut seen = None;
    loop {
        if let Health::Kioku { version } = probe_health(client, env.request_timeout) {
            if accept(&version) {
                return Some(version);
            }
            seen = Some(version);
        }
        if Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(env.poll_interval);
    }
}

fn push_health_line(
    manager: &ServiceManager,
    url: &str,
    env: &SetupEnv,
    version: Option<String>,
    r: &mut SetupReport,
) {
    match version {
        Some(v) => r.push(
            Mark::Ok,
            "service",
            format!("{} running at {url} (v{v})", manager.describe()),
        ),
        None => r.push(
            Mark::Fail,
            "service",
            format!(
                "{} installed, but {url} did not answer within {} s; see `kioku service logs`",
                manager.describe(),
                env.poll_timeout.as_secs()
            ),
        ),
    }
}

/// Summary text of an installed agent: its hook file (with the number of registrations),
/// MCP file and instruction file name.
fn agent_text(agent: Agent, ctx: &InstallCtx, iopts: &InstallOptions) -> String {
    let hooks = hooks_path(agent, ctx, false);
    let mcp = mcp_path(agent, ctx);
    let mut text = if hooks == mcp {
        format!("hooks + MCP {}", tilde(&hooks, &ctx.home))
    } else {
        format!(
            "hooks {} ({}), MCP {}",
            tilde(&hooks, &ctx.home),
            hook_specs(agent, &ctx.bin, ctx.platform).len(),
            tilde(&mcp, &ctx.home)
        )
    };
    if wants_instructions(agent, iopts)
        && let Some(file) = instruction_files(agent, ctx, false).first()
    {
        text.push_str(&format!(
            ", {}",
            file.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    text
}

/// True when Codex's `config.toml` records a trusted hash for kioku's hooks (heuristic; see
/// [`is_kioku_trust_key`] for the key formats).
pub fn codex_trust_recorded(codex_config: &Path) -> bool {
    let Some(t) = block::read_codex_config(codex_config) else {
        return false;
    };
    let hooks_json = codex_config.with_file_name("hooks.json");
    t.get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .is_some_and(|state| {
            state.iter().any(|(k, v)| {
                is_kioku_trust_key(k, &hooks_json)
                    && v.get("trusted_hash").is_some_and(|h| h.is_str())
            })
        })
}

/// True when a `[hooks.state."<key>"]` key names kioku's hooks. Codex 0.159 keys entries as
/// `<path of hooks.json>:<event>:<group>:<handler>` (Mac, 2026-09-30); kioku owns the
/// `session_start` entry of the user-level `hooks.json` next to `config.toml`. Older keys that
/// mention kioku directly are still accepted.
fn is_kioku_trust_key(key: &str, hooks_json: &Path) -> bool {
    if key.contains("kioku") {
        return true;
    }
    // rsplitn: the path itself may contain ':' (`C:\Users\...`).
    let mut parts = key.rsplitn(4, ':');
    let (_, _, event, path) = (parts.next(), parts.next(), parts.next(), parts.next());
    let (Some(event), Some(path)) = (event, path) else {
        return false;
    };
    let norm = |p: &Path| {
        kioku_core::util::plain_path(p)
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase()
    };
    event == "session_start" && norm(Path::new(path)) == norm(hooks_json)
}

fn agents_step(opts: &SetupOptions, env: &SetupEnv, client: &ClientConfig, r: &mut SetupReport) {
    let ctx = env.install_ctx(client);
    let iopts = InstallOptions {
        project: false,
        instructions: if opts.no_instructions {
            Instructions::Skip
        } else {
            Instructions::Default
        },
        dry_run: opts.dry_run,
        enable_hooks_feature: false,
        trust_mcp: false,
        mcp_http: opts.mcp_http,
        skip_desktop: false,
    };
    for (agent, status) in install_all(&ctx, &iopts, &opts.agents) {
        let name = agent.as_str();
        match status {
            AllStatus::Changed(rep) | AllStatus::Unchanged(rep) => {
                let mut text = agent_text(agent, &ctx, &iopts);
                if opts.dry_run {
                    text = if rep.changed {
                        format!("would change: {text}")
                    } else {
                        format!("{text} (unchanged)")
                    };
                }
                r.push(Mark::Ok, name, text);
                if opts.dry_run {
                    r.details(rep.lines.clone());
                }
                agent_warnings(agent, &ctx, &rep, r);
            }
            AllStatus::NotDetected(dir) => r.push(
                Mark::Skip,
                name,
                format!("not detected ({} missing)", tilde(&dir, &env.home)),
            ),
            AllStatus::Error(e) => r.push(
                Mark::Fail,
                name,
                e.lines().next().unwrap_or("failed").to_string(),
            ),
        }
    }
    // Detection happens per agent; mention a filter that matched nothing.
    if !opts.agents.is_empty() && opts.agents.iter().all(|a| !is_detected(*a, &ctx)) {
        r.push(
            Mark::Warn,
            "agents",
            "none of the agents given with --agents is installed on this machine",
        );
    }
}

fn agent_warnings(agent: Agent, ctx: &InstallCtx, rep: &AgentReport, r: &mut SetupReport) {
    let name = agent.as_str();
    for line in &rep.lines {
        if let Some(w) = line.strip_prefix("warning: ") {
            r.push(Mark::Warn, name, w.to_string());
        } else if line.contains("yourself") {
            r.push(
                Mark::Warn,
                name,
                format!(
                    "{} could not be edited; run `kioku install {name}` to see what to add by hand",
                    line.lines()
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches(['.', ':'])
                ),
            );
        }
    }
    if agent == Agent::Codex && !codex_trust_recorded(&mcp_path(agent, ctx)) {
        r.push(
            Mark::Warn,
            name,
            "open Codex and run /hooks once to trust kioku's hooks",
        );
    }
}

fn finish(mut r: SetupReport, opts: &SetupOptions, state: Option<&ConfigState>) -> SetupReport {
    if opts.dry_run {
        r.footer.push("dry run: nothing was written".into());
    } else if !opts.no_agents && state.is_some() && r.exit_code() == 0 {
        r.footer
            .push("Restart running agents so they pick up the new hooks and MCP server.".into());
    }
    if state.is_some() {
        r.footer.push("Check any time with: kioku doctor".into());
    }
    if opts.print_client_command
        && let Some(s) = state
    {
        r.footer.extend(client_command(s));
    }
    r
}

/// The laptop command (includes the token; only with `--print-client-command`).
fn client_command(state: &ConfigState) -> Vec<String> {
    let cfg = &state.cfg;
    let token = cfg
        .client
        .auth_token
        .clone()
        .or_else(|| cfg.server.auth_token.clone())
        .unwrap_or_else(|| "<token>".into());
    let mut out = vec![String::new()];
    let url = if state.client_only {
        ClientUrl {
            url: cfg.client.server_url.trim_end_matches('/').to_string(),
            mdns_alternative: None,
            loopback_note: None,
        }
    } else {
        client_url(cfg)
    };
    out.extend(url.loopback_note.clone());
    if !state.client_only {
        // SPEC-M2.3 §5: the invite line is the recommended way; this one is the manual path.
        out.push(
            "Easiest: run `kioku invite` here and paste the line it prints on the other machine (no token to copy)."
                .into(),
        );
    }
    out.push(
        "Manually, on another machine (this line contains the auth token - keep it private):"
            .into(),
    );
    out.push(format!(
        "  curl -fsSL {} | sh -s -- --client-only {} {token}",
        install_sh_url(),
        url.url
    ));
    out.extend(url.alternative_line());
    out.push("On Windows, in PowerShell (also contains the token):".into());
    out.push(format!("  {}", install_ps1_command(&url.url, &token)));
    out
}

/// The PowerShell line that installs a Windows client (SPEC-M2.2 §6, §8).
pub fn install_ps1_command(url: &str, token: &str) -> String {
    format!(
        "& ([scriptblock]::Create((irm {}))) -ClientOnly {url} {token}",
        install_ps1_url()
    )
}

/// The token of `kioku setup --client-only <url> [<token>]` (SPEC-M2.7 §10): the deprecated
/// argument (with a warning to print), else `KIOKU_CLIENT_TOKEN`, else one line of `stdin`
/// when it is not a terminal.
pub fn client_token(
    arg: Option<&str>,
    vars: &std::collections::HashMap<String, String>,
    stdin: &mut dyn std::io::BufRead,
    stdin_is_tty: bool,
) -> anyhow::Result<(String, Option<String>)> {
    if let Some(t) = arg.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok((
            t.to_string(),
            Some("kioku: warning: passing the token as an argument is deprecated (it shows up in the process list and shell history); use KIOKU_CLIENT_TOKEN=… or pipe it on stdin, or better `kioku invite` on the server".into()),
        ));
    }
    if let Some(t) = vars
        .get("KIOKU_CLIENT_TOKEN")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
    {
        return Ok((t.to_string(), None));
    }
    if !stdin_is_tty {
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("reading the token from stdin")?;
        let t = line.trim();
        if !t.is_empty() {
            return Ok((t.to_string(), None));
        }
    }
    anyhow::bail!(
        "no token: set KIOKU_CLIENT_TOKEN or pipe the token on stdin (`kioku invite` on the server avoids handling it at all)"
    )
}

/// The URL other machines should use for this server (SPEC-M2 §11, §19.3).
#[derive(Clone, Debug)]
pub struct ClientUrl {
    /// `http://<LAN IP or bind>:<port>`.
    pub url: String,
    /// `http://<host>.local:<port>` when the server binds every interface.
    pub mdns_alternative: Option<String>,
    /// A note when the server only listens on loopback.
    pub loopback_note: Option<String>,
}

impl ClientUrl {
    /// The "(the IP also works over a VPN …)" line, if there is an mDNS alternative.
    pub fn alternative_line(&self) -> Option<String> {
        // SPEC-M2 §19.3: the IP also works over a VPN that routes this LAN; the mDNS name
        // survives address changes but only resolves on the LAN itself.
        self.mdns_alternative.as_ref().map(|alt| {
            format!(
                "  (the IP also works over a VPN that routes this network; machines that stay on this LAN can use {alt} instead, which survives an address change)"
            )
        })
    }
}

/// [`ClientUrl`] for a server machine's config.
pub fn client_url(cfg: &Config) -> ClientUrl {
    let bind = cfg.server.bind.trim();
    let wildcard = matches!(bind, "0.0.0.0" | "::" | "[::]" | "");
    let mut mdns_alternative = None;
    let host = if wildcard || cfg.is_loopback_bind() {
        if wildcard {
            mdns_alternative = local_host_name().map(|n| format!("http://{n}:{}", cfg.server.port));
        }
        first_non_loopback_ip().unwrap_or_else(|| "<this-host>".into())
    } else {
        bind.to_string()
    };
    let loopback_note = cfg.is_loopback_bind().then(|| {
        format!(
            "note: this server listens on {bind} only; set [server] bind = \"0.0.0.0\" in {} and restart it (kioku service stop && kioku service start) so other machines can reach it",
            cfg.config_file.display()
        )
    });
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    ClientUrl {
        url: format!("http://{host}:{}", cfg.server.port),
        mdns_alternative,
        loopback_note,
    }
}

/// This machine's mDNS name (`<host>.local`), from `hostname`; `None` if unavailable.
fn local_host_name() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    let short = name.split('.').next().filter(|s| !s.is_empty())?;
    Some(format!("{short}.local"))
}

/// This machine's first non-loopback IP address (from the route to a documentation
/// address; no packet is sent).
pub fn first_non_loopback_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

#[cfg(test)]
mod tests {

    /// SPEC-M2.7 §10: the token from the deprecated argument (with a warning), the
    /// environment, or stdin; never from a terminal.
    #[test]
    fn client_token_sources() {
        use std::collections::HashMap;
        let none = HashMap::new();
        let env: HashMap<String, String> =
            [("KIOKU_CLIENT_TOKEN".to_string(), " env-tok \n".to_string())].into();
        let mut empty: &[u8] = b"";
        let (t, w) = client_token(Some("arg-tok"), &env, &mut empty, false).unwrap();
        assert_eq!(t, "arg-tok");
        assert!(w.unwrap().contains("deprecated"));
        let (t, w) = client_token(None, &env, &mut empty, false).unwrap();
        assert_eq!((t.as_str(), w), ("env-tok", None));
        let mut piped: &[u8] = b"stdin-tok\nignored\n";
        let (t, w) = client_token(None, &none, &mut piped, false).unwrap();
        assert_eq!((t.as_str(), w), ("stdin-tok", None));
        let mut piped: &[u8] = b"stdin-tok\n";
        assert!(
            client_token(None, &none, &mut piped, true).is_err(),
            "a terminal is never read"
        );
        let mut blank: &[u8] = b"\n";
        let err = client_token(None, &none, &mut blank, false).unwrap_err();
        assert!(err.to_string().contains("KIOKU_CLIENT_TOKEN"));
    }

    /// Regression (Mac and Windows, 2026-09-30): Codex 0.159 keys trust entries by the
    /// hooks.json path and event, never mentioning kioku, so doctor warned although the hooks
    /// were trusted and running.
    #[test]
    fn codex_trust_is_found_under_the_hooks_json_path_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let hooks = dir.path().join("hooks.json").display().to_string();
        let write = |key: &str| {
            std::fs::write(
                &cfg,
                format!(
                    "[hooks.state]\n\n[hooks.state.{}]\ntrusted_hash = \"sha256:00\"\n",
                    toml::Value::String(key.to_string())
                ),
            )
            .unwrap();
        };
        write(&format!("{hooks}:session_start:0:0"));
        assert!(codex_trust_recorded(&cfg));
        write(&format!("{hooks}:post_tool_use:0:0"));
        assert!(!codex_trust_recorded(&cfg));
        write("/elsewhere/hooks.json:session_start:0:0");
        assert!(!codex_trust_recorded(&cfg));
        write("kioku hook session-start --agent codex");
        assert!(codex_trust_recorded(&cfg));
        assert!(is_kioku_trust_key(
            r"C:\Users\Room\.codex\hooks.json:session_start:0:0",
            Path::new(r"\\?\C:\Users\Room\.codex\hooks.json")
        ));
    }
    use super::*;

    #[test]
    fn summary_format_and_tilde() {
        let mut r = SetupReport {
            lines: Vec::new(),
            footer: vec!["Check any time with: kioku doctor".into()],
        };
        r.push(Mark::Ok, "binary", "/Users/me/.local/bin/kioku");
        r.push(
            Mark::Warn,
            "codex",
            "open Codex and run /hooks once to trust kioku's hooks",
        );
        r.push(Mark::Skip, "cursor", "not detected (~/.cursor missing)");
        r.push(
            Mark::Ok,
            "gemini-cli",
            "hooks + MCP ~/.gemini/settings.json, GEMINI.md",
        );
        r.push(
            Mark::Ok,
            "claude-code",
            "hooks ~/.claude/settings.json (6), MCP ~/.claude.json",
        );
        assert_eq!(
            r.render(),
            format!(
                "kioku setup (v{VERSION})
  ok  binary      /Users/me/.local/bin/kioku
  !!  codex       open Codex and run /hooks once to trust kioku's hooks
  --  cursor      not detected (~/.cursor missing)
  ok  gemini-cli  hooks + MCP ~/.gemini/settings.json, GEMINI.md
  ok  claude-code hooks ~/.claude/settings.json (6), MCP ~/.claude.json
Check any time with: kioku doctor
"
            )
        );
        assert!(r.render().is_ascii());
        assert_eq!(r.exit_code(), 0);
        r.push(Mark::Fail, "auth", "x");
        assert_eq!(r.exit_code(), 1);
        let home = Path::new("/home/me");
        assert_eq!(
            tilde(Path::new("/home/me/.codex/hooks.json"), home),
            "~/.codex/hooks.json"
        );
        assert_eq!(tilde(Path::new("/etc/x"), home), "/etc/x");
        assert!(install_sh_url().starts_with("https://raw.githubusercontent.com/misorafa/kioku/"));
    }

    /// A fake kioku health endpoint reporting `0.0.1` until `restarted` is set, then VERSION.
    fn fake_health(restarted: std::sync::Arc<std::sync::atomic::AtomicBool>) -> String {
        use std::sync::atomic::Ordering;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = axum::Router::new().route(
                    "/api/v1/health",
                    axum::routing::get(move || {
                        let v = if restarted.load(Ordering::SeqCst) {
                            VERSION
                        } else {
                            "0.0.1"
                        };
                        async move { axum::Json(serde_json::json!({"ok": true, "version": v})) }
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://{}", rx.recv().unwrap())
    }

    #[test]
    fn a_healthy_service_on_another_version_is_restarted() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let home = tempfile::tempdir().unwrap();
        let restarted = Arc::new(AtomicBool::new(false));
        let url = fake_health(restarted.clone());
        let flag = restarted.clone();
        let runner = Runner::recording(move |argv| match argv.join(" ").as_str() {
            "systemctl --user restart kioku.service" => {
                flag.store(true, Ordering::SeqCst);
                crate::service::CmdOutput::ok("")
            }
            "loginctl show-user me -p Linger" => crate::service::CmdOutput::ok("Linger=yes\n"),
            _ => crate::service::CmdOutput::ok(""),
        });
        let env = SetupEnv {
            vars: [("USER".to_string(), "me".to_string())].into(),
            home: home.path().to_path_buf(),
            cwd: home.path().to_path_buf(),
            bin: home.path().join("bin/kioku").display().to_string(),
            runner: runner.clone(),
            platform: Some(Platform::Systemd),
            hook_platform: HookPlatform::Unix,
            request_timeout: Duration::from_secs(3),
            poll_interval: Duration::from_millis(20),
            poll_timeout: Duration::from_secs(5),
        };
        let mut cfg = Config::for_data_dir(&home.path().join(".kioku"));
        cfg.client.server_url = url.clone();
        // Installed, active and unchanged: install() itself does nothing.
        let manager = env.service_manager(&cfg.data_dir);
        let unit = manager.definition_path().unwrap();
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, manager.render().unwrap()).unwrap();

        let mut r = SetupReport {
            lines: Vec::new(),
            footer: Vec::new(),
        };
        service_step(&SetupOptions::default(), &env, &cfg, &mut r);
        assert_eq!(
            r.summary_lines(),
            [format!(
                "  ok  service     systemd kioku.service running at {url}, restarted (v0.0.1 -> v{VERSION})"
            )]
        );
        assert!(restarted.load(Ordering::SeqCst));
        let calls: Vec<String> = runner.calls().iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.as_str() == "systemctl --user restart kioku.service")
                .count(),
            1,
            "{calls:?}"
        );

        // Same version: no restart.
        runner.clear_calls();
        let mut r = SetupReport {
            lines: Vec::new(),
            footer: Vec::new(),
        };
        service_step(&SetupOptions::default(), &env, &cfg, &mut r);
        assert_eq!(
            r.summary_lines(),
            [format!(
                "  ok  service     systemd kioku.service running at {url} (v{VERSION})"
            )]
        );
        assert!(
            runner
                .calls()
                .iter()
                .all(|c| c.join(" ") != "systemctl --user restart kioku.service")
        );
    }
}
