//! `kioku setup` (M2 §11): one idempotent, non-interactive command that takes a machine
//! from "binary installed" to "every agent wired up" — binary → config (`init` or
//! `--client-only`) → user-level service → auth check → `install all` → summary.
//!
//! Everything the steps read from the environment comes in through [`SetupEnv`] (vars,
//! home, binary, command runner), so tests run the whole flow on a temp HOME against an
//! in-process server without touching the real environment, launchctl or systemctl.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kioku_core::config::CONFIG_FILE;
use kioku_core::{ClientConfig, Config};

use crate::client::{ApiClient, http_status};
use crate::event::Agent;
use crate::install::agents::{
    AgentReport, AllStatus, InstallCtx, InstallOptions, Instructions, detection_dir, hook_specs,
    hooks_path, install_all, instruction_files, mcp_path, unstable_binary_warning,
    wants_instructions,
};
use crate::install::block;
use crate::service::{Health, Platform, Runner, ServiceManager, ServiceSpec, probe_health};

/// GitHub repository of kioku (`KIOKU_REPO` / `--repo` default of install.sh).
pub const KIOKU_REPO: &str = "misorafa/kioku";

/// Raw URL of `install.sh` on the default branch.
pub fn install_sh_url() -> String {
    format!("https://raw.githubusercontent.com/{KIOKU_REPO}/main/install.sh")
}

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
            vars: std::env::vars().collect(),
            home: kioku_core::util::home_dir(),
            cwd: std::env::current_dir()?,
            bin,
            runner: Runner::real(),
            platform: None,
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

/// `path` with the home directory abbreviated to `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn has_server_section(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .is_some_and(|t| t.contains_key("server"))
}

/// Authenticated `GET /api/v1/status`.
fn check_status(client: &ClientConfig, timeout: Duration) -> anyhow::Result<()> {
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
    if state.client_only {
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
        if opts.dry_run {
            r.push(
                Mark::Ok,
                "config",
                format!(
                    "would write [client] to {} (client-only -> {url})",
                    config_path.display()
                ),
            );
        } else {
            match Config::write_client_only(config_path, url, token) {
                Ok(written) => {
                    cfg.client = written.client;
                    r.push(
                        Mark::Ok,
                        "config",
                        format!("{} (client-only -> {url})", config_path.display()),
                    );
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
    })
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
    let deadline = Instant::now() + env.poll_timeout;
    let version = loop {
        if let Health::Kioku { version } = probe_health(&cfg.client, env.request_timeout) {
            break Some(version);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(env.poll_interval);
    };
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
    for note in act.lines.iter().filter(|l| l.starts_with("note: ")) {
        r.push(Mark::Warn, "service", note.trim_start_matches("note: "));
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
            hook_specs(agent, &ctx.bin).len(),
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

/// True when Codex's `config.toml` records a trusted hash for an entry mentioning kioku
/// (heuristic: the key format is unverified, M2 §4.1).
pub fn codex_trust_recorded(codex_config: &Path) -> bool {
    let Some(t) = block::read_codex_config(codex_config) else {
        return false;
    };
    t.get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .is_some_and(|state| {
            state.iter().any(|(k, v)| {
                k.contains("kioku") && v.get("trusted_hash").is_some_and(|h| h.is_str())
            })
        })
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
    if !opts.agents.is_empty()
        && opts
            .agents
            .iter()
            .all(|a| !detection_dir(*a, &ctx).is_dir())
    {
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
        cfg.client.server_url.trim_end_matches('/').to_string()
    } else {
        let bind = cfg.server.bind.trim();
        let wildcard = matches!(bind, "0.0.0.0" | "::" | "[::]" | "");
        let host = if wildcard || cfg.is_loopback_bind() {
            first_non_loopback_ip().unwrap_or_else(|| "<this-host>".into())
        } else {
            bind.to_string()
        };
        if cfg.is_loopback_bind() {
            out.push(format!(
                "note: this server listens on {bind} only; set [server] bind = \"0.0.0.0\" in {} and restart it (kioku service stop && kioku service start) so other machines can reach it",
                cfg.config_file.display()
            ));
        }
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host
        };
        format!("http://{host}:{}", cfg.server.port)
    };
    out.push("On another machine (this line contains the auth token - keep it private):".into());
    out.push(format!(
        "  curl -fsSL {} | sh -s -- --client-only {url} {token}",
        install_sh_url()
    ));
    out
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
}
