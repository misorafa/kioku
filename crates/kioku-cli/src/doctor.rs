//! `kioku doctor [--json] [--agent <name>]` (M2 §12): every check of the table with its
//! OK / WARN / FAIL semantics and a `fix:` hint. Read-only; 3 s per request; the token is
//! compared, never printed.
//!
//! Like `setup`, all inputs come in through [`DoctorEnv`] so tests run on a temp HOME
//! against an in-process server with a recording command runner.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kioku_core::config::CONFIG_FILE;
use kioku_core::{ClientConfig, Config, StatusReport};
use serde_json::{Value, json};

use crate::client::{ApiClient, http_status, is_local_url};
use crate::dump::{dump_enabled, dump_path};
use crate::event::{ALL_AGENTS, Agent, HookEnv};
use crate::install::agents::{
    GEMINI_EVENTS, InstallCtx, InstallOptions, claude_desktop_configs, hook_commands, hook_specs,
    hooks_map, hooks_path, instruction_files, is_detected, mcp_path, unstable_binary_warning,
    wants_instructions,
};
use crate::install::block::{self, MD_MARKERS};
use crate::install::{mcp_url, read_settings};
use crate::service::{Health, Platform, Runner, probe_health};
use crate::setup::{VERSION, codex_trust_recorded};

/// Per-request timeout of doctor's HTTP checks.
pub const DOCTOR_TIMEOUT: Duration = Duration::from_secs(3);

/// Result of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Fine.
    Ok,
    /// Works, but something is off.
    Warn,
    /// Broken.
    Fail,
    /// Nothing wrong; worth knowing (SPEC-M3.3 §4: an agent newer than kioku's fixtures).
    Info,
}

impl Status {
    /// `ok` / `warn` / `fail` (JSON).
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Info => "info",
        }
    }

    /// `[ OK ]` / `[WARN]` / `[FAIL]` (text).
    pub fn tag(self) -> &'static str {
        match self {
            Status::Ok => "[ OK ]",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
            Status::Info => "[INFO]",
        }
    }
}

/// One check result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Check id (`binary`, `agent.codex.hooks`, …).
    pub id: String,
    /// Outcome.
    pub status: Status,
    /// One line.
    pub message: String,
    /// How to fix it.
    pub fix: Option<String>,
    /// What `kioku doctor --fix` does about it (SPEC-M3.2 §3); `None` = nothing safe to do.
    pub action: Option<FixAction>,
}

impl Check {
    /// The same check with a `--fix` action.
    pub fn with_action(mut self, action: FixAction) -> Check {
        self.action = Some(action);
        self
    }
}

/// A safe fix `kioku doctor --fix` applies (SPEC-M3.2 §3). It never rotates tokens, never
/// changes `[server] bind`, and never writes a running Claude desktop app's config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixAction {
    /// `chmod <mode> <path>` (config 0600, data dir 0700).
    Chmod {
        /// File or directory.
        path: PathBuf,
        /// New permission bits.
        mode: u32,
    },
    /// `kioku install <agent>` (re-registers hooks, MCP entry and instructions).
    Install(Agent),
    /// `kioku service install` (definition missing or without the update marker).
    ServiceInstall,
    /// `kioku reindex` (outdated or inconsistent index).
    Reindex,
    /// Turn off an expired hook dump (`[client] hook_dump = false`, marker removed).
    ClearHookDump,
    /// Nothing kioku may change itself: the instruction is printed (Local Network
    /// permission, …).
    Manual(String),
}

/// Builds a check without a `--fix` action.
pub fn check(id: &str, status: Status, message: impl Into<String>, fix: Option<String>) -> Check {
    Check {
        id: id.to_string(),
        status,
        message: message.into(),
        fix,
        action: None,
    }
}

/// The environment doctor inspects.
#[derive(Clone, Debug)]
pub struct DoctorEnv {
    /// Environment variables (`KIOKU_*`, `PATH`, `CODEX_HOME`, `XDG_CONFIG_HOME`, `USER`).
    pub vars: HashMap<String, String>,
    /// Home directory.
    pub home: PathBuf,
    /// Absolute path of the running kioku binary.
    pub bin: String,
    /// Runs `git --version`, `codex --version` and the service manager's queries.
    pub runner: Runner,
    /// Forces the service platform (tests); `None` detects it.
    pub platform: Option<Platform>,
    /// The OS kioku runs on (hook command shapes, SPEC-M2.2 §7.4).
    pub hook_platform: crate::install::HookPlatform,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Where agent apps are looked for (`/Applications`, `~/Applications` on macOS; SPEC-M3.2
    /// §2 warns only for an agent present on this machine).
    pub app_dirs: Vec<PathBuf>,
    /// This process's id: the start of the parent walk to the responsible app (§3.1).
    pub pid: u32,
}

impl DoctorEnv {
    /// The real process environment.
    pub fn from_process(bin: String) -> DoctorEnv {
        DoctorEnv {
            vars: kioku_core::util::env_vars(),
            home: kioku_core::util::home_dir(),
            bin,
            runner: Runner::real(),
            platform: None,
            hook_platform: crate::install::HookPlatform::current(),
            timeout: DOCTOR_TIMEOUT,
            app_dirs: default_app_dirs(&kioku_core::util::home_dir()),
            pid: std::process::id(),
        }
    }

    fn setup_env(&self) -> crate::setup::SetupEnv {
        crate::setup::SetupEnv {
            vars: self.vars.clone(),
            home: self.home.clone(),
            cwd: self.home.clone(),
            bin: self.bin.clone(),
            runner: self.runner.clone(),
            platform: self.platform.clone(),
            hook_platform: self.hook_platform,
            request_timeout: self.timeout,
            poll_interval: Duration::from_millis(200),
            poll_timeout: Duration::from_secs(0),
        }
    }
}

/// `/Applications` and `~/Applications` on macOS; nothing elsewhere.
pub fn default_app_dirs(home: &Path) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Applications"), home.join("Applications")]
    } else {
        Vec::new()
    }
}

/// Exit code: 1 when any check failed.
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

/// Text output: `[ OK ] id: message` (+ `       fix: …`).
pub fn render_text(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!("{} {}: {}\n", c.status.tag(), c.id, c.message));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("       fix: {fix}\n"));
        }
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    let infos = checks.iter().filter(|c| c.status == Status::Info).count();
    out.push_str(&format!(
        "{} checks: {} ok, {warns} warning(s), {fails} failure(s){}\n",
        checks.len(),
        checks.len() - fails - warns - infos,
        if infos > 0 {
            format!(", {infos} info")
        } else {
            String::new()
        }
    ));
    out
}

/// `--json` output: `{"checks":[{id, status, message, fix?}]}`.
pub fn render_json(checks: &[Check]) -> Value {
    let list: Vec<Value> = checks
        .iter()
        .map(|c| {
            let mut v = json!({"id": c.id, "status": c.status.as_str(), "message": c.message});
            if let Some(fix) = &c.fix {
                v["fix"] = json!(fix);
            }
            v
        })
        .collect();
    json!({ "checks": list })
}

/// Runs every check (agent checks restricted to `only` when given).
pub fn run_doctor(env: &DoctorEnv, only: Option<Agent>) -> Vec<Check> {
    let mut out = Vec::new();
    let senv = env.setup_env();
    let config_dir = senv.config_dir();
    let config_path = config_dir.join(CONFIG_FILE);

    out.push(binary_check(env));
    let (cfg, config_ok, server_machine) = config_check(env, &config_dir, &config_path, &mut out);

    if server_machine {
        out.push(data_dir_check(&cfg));
    }
    out.push(git_check(env));

    // Server, auth, index, MCP.
    let health = probe_health(&cfg.client, env.timeout);
    out.push(server_check(&cfg, &health, server_machine, env));
    if server_machine
        && matches!(health, Health::Kioku { .. })
        && let Some(c) = lan_check(&cfg, env)
    {
        out.push(c);
    }
    if config_ok && matches!(health, Health::Kioku { .. }) {
        match ApiClient::new(&cfg.client, env.timeout).and_then(|c| c.get(&["status"], &[])) {
            Ok(body) => {
                out.push(check(
                    "auth",
                    Status::Ok,
                    "token accepted (GET /api/v1/status)",
                    None,
                ));
                if let Ok(status) = serde_json::from_value::<StatusReport>(body) {
                    out.push(index_check(&status));
                }
                // The server hashes every wiki page for this (without holding its locks):
                // give it longer than a health probe.
                match ApiClient::new(&cfg.client, env.timeout.max(DIAGNOSTICS_TIMEOUT))
                    .and_then(|c| c.get(&["diagnostics"], &[]))
                {
                    Ok(v) => {
                        match serde_json::from_value::<kioku_core::store::ReliabilityReport>(v) {
                            Ok(r) => out.extend(reliability_checks(&r)),
                            Err(e) => out.push(check("storage", Status::Warn, e.to_string(), None)),
                        }
                    }
                    Err(e) => out.push(check(
                        "storage",
                        Status::Warn,
                        format!("diagnostics unavailable: {e}"),
                        Some("update server or retry kioku doctor".into()),
                    )),
                }
                out.push(mcp_check(&cfg.client, env.timeout));
            }
            Err(e) => {
                let msg = match http_status(&e) {
                    Some(401) => "the server rejected the token (HTTP 401)".to_string(),
                    _ => format!("GET /api/v1/status failed: {e:#}"),
                };
                out.push(check(
                    "auth",
                    Status::Fail,
                    msg,
                    Some(format!(
                        "make [client] auth_token in {} match the server's [server] auth_token: run kioku invite on the server and paste the line here (or kioku setup --client-only <url> <token>)",
                        config_path.display()
                    )),
                ));
            }
        }
    }

    if server_machine {
        out.push(service_check(env, &cfg, &health));
    }
    out.push(update_check(&cfg, env, &health));

    // Agents.
    let ctx = senv.install_ctx(&cfg.client);
    let agents: Vec<Agent> = match only {
        Some(a) => vec![a],
        None => ALL_AGENTS
            .iter()
            .copied()
            .filter(|a| is_detected(*a, &ctx))
            .collect(),
    };
    for agent in &agents {
        out.extend(agent_checks(*agent, &ctx, env));
    }
    if agents.contains(&Agent::Cursor)
        && let Some(c) = cursor_duplicate_check(&ctx)
    {
        out.push(c);
    }
    out.extend(liveness_checks(env, &cfg, &agents));

    let queue = crate::outbox::count(&cfg);
    let queue_error = crate::outbox::last_error(&cfg);
    let refused = crate::outbox::failed_count(&cfg);
    out.push(match queue {
        Ok(n) => check(
            "outbox",
            if n == 0 && queue_error.is_none() && refused == 0 {
                Status::Ok
            } else {
                Status::Warn
            },
            format!(
                "{n} observation(s) waiting to be resent{}{}",
                if refused > 0 {
                    format!(
                        "; {refused} refused by the server (kept in {})",
                        crate::outbox::directory(&cfg).join("failed").display()
                    )
                } else {
                    String::new()
                },
                queue_error
                    .map(|e| format!("; last error: {e}"))
                    .unwrap_or_default()
            ),
            (n > 0).then(|| {
                "kioku sync (runs by itself after the next hook that reaches the server)".into()
            }),
        ),
        Err(e) => check(
            "outbox",
            Status::Fail,
            e.to_string(),
            Some("check outbox permissions".into()),
        ),
    });
    out.push(hook_log_check(&cfg, env));
    out.push(hook_dump_check(&cfg, env));
    out
}

fn binary_check(env: &DoctorEnv) -> Check {
    let exe = canonical(Path::new(&env.bin));
    let mut warnings = Vec::new();
    let mut fix = None;
    match find_on_path(path_var(&env.vars)) {
        None => {
            warnings.push("`kioku` is not on PATH".to_string());
            fix = Path::new(&env.bin)
                .parent()
                .map(|d| format!("add {} to PATH", d.display()));
        }
        Some(p) if canonical(&p) != exe => {
            warnings.push(format!(
                "`kioku` on PATH is {}, not this binary",
                p.display()
            ));
            fix = Some("remove the other copy or reorder PATH".into());
        }
        Some(_) => {}
    }
    if unstable_binary_warning(&env.bin).is_some() {
        warnings.push("build or temp location: hooks break when it moves".into());
        fix = Some("install with install.sh (copies it to ~/.local/bin)".into());
    }
    if warnings.is_empty() {
        check(
            "binary",
            Status::Ok,
            format!("{} (v{VERSION}), the `kioku` on PATH", env.bin),
            None,
        )
    } else {
        check(
            "binary",
            Status::Warn,
            format!("{} (v{VERSION}): {}", env.bin, warnings.join("; ")),
            fix,
        )
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// The `PATH` value. Windows spells the variable `Path` and treats names case-insensitively,
/// so any casing matches (found on a real Windows 11: `PATH` alone was missing).
pub fn path_var(vars: &std::collections::HashMap<String, String>) -> &str {
    vars.get("PATH")
        .or_else(|| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
                .map(|(_, v)| v)
        })
        .map(String::as_str)
        .unwrap_or("")
}

/// The first `kioku` executable in `path_var` (split the platform's way: `;` and quoted
/// entries on Windows, `:` elsewhere), trying `kioku` and `kioku.exe` in each directory.
pub fn find_on_path(path_var: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|d| [d.join("kioku"), d.join("kioku.exe")])
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn mode(p: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode(_p: &Path) -> Option<u32> {
    None
}

/// Returns the effective config, whether it is usable, and whether this is a server machine.
fn config_check(
    env: &DoctorEnv,
    config_dir: &Path,
    path: &Path,
    out: &mut Vec<Check>,
) -> (Config, bool, bool) {
    let fallback = || {
        let mut c = Config::for_data_dir(config_dir);
        let _ = c.apply_env(&env.vars);
        c
    };
    let fix_setup = Some(
        "run `kioku setup` (on a client: `kioku invite` on the server, then paste its line here)"
            .to_string(),
    );
    if !path.exists() {
        out.push(check(
            "config",
            Status::Fail,
            format!("{} does not exist", path.display()),
            fix_setup,
        ));
        return (fallback(), false, false);
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(anyhow::Error::from)
        .and_then(|t| toml::from_str::<toml::Table>(&t).map_err(anyhow::Error::from))
        .and_then(|t| Ok((t, Config::load_from_dir(config_dir, &env.vars)?)));
    let (table, cfg) = match parsed {
        Ok(v) => v,
        Err(e) => {
            out.push(check(
                "config",
                Status::Fail,
                format!("{} cannot be parsed: {e:#}", path.display()),
                Some("fix the TOML syntax or move the file away and run `kioku setup`".into()),
            ));
            return (fallback(), false, false);
        }
    };
    let server_machine = table.contains_key("server");
    if !table.contains_key("client") {
        out.push(check(
            "config",
            Status::Fail,
            format!("{} has no [client] section", path.display()),
            fix_setup,
        ));
        return (cfg, false, server_machine);
    }
    let has_token = cfg
        .client
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if !has_token || cfg.client.server_url.trim().is_empty() {
        out.push(check(
            "config",
            Status::Fail,
            format!(
                "[client] in {} has no server_url or an empty auth_token",
                path.display()
            ),
            fix_setup,
        ));
        return (cfg, false, server_machine);
    }
    let role = if server_machine {
        "server machine"
    } else {
        "client only"
    };
    match mode(path) {
        Some(m) if m & 0o077 != 0 => out.push(
            check(
                "config",
                Status::Warn,
                format!(
                    "{} ({role}) is mode {m:04o}, wider than 0600 (it holds the token)",
                    path.display()
                ),
                Some(format!("chmod 600 {}", path.display())),
            )
            .with_action(FixAction::Chmod {
                path: path.to_path_buf(),
                mode: 0o600,
            }),
        ),
        _ => out.push(check(
            "config",
            Status::Ok,
            format!("{} ({role}) -> {}", path.display(), cfg.client.server_url),
            None,
        )),
    }
    (cfg, true, server_machine)
}

fn data_dir_check(cfg: &Config) -> Check {
    let dir = &cfg.data_dir;
    if !dir.is_dir() {
        return check(
            "data_dir",
            Status::Fail,
            format!("{} does not exist", dir.display()),
            Some("run `kioku init` (or start the server once: it creates the data dir)".into()),
        );
    }
    // SPEC-M2.7 §5: a database from a newer kioku makes `kioku serve` exit 78.
    if let Some(newer) = kioku_core::newer_schema_on_disk(&kioku_core::DataDir::new(dir).db_file())
    {
        return check(
            "data_dir",
            Status::Fail,
            format!("{}: {newer}", dir.display()),
            Some(format!(
                "this kioku (v{VERSION}) is older than the data: run `kioku update`, or `kioku restore <backup> --into <new dir>` and point KIOKU_DATA_DIR at it"
            )),
        );
    }
    let mut warnings = Vec::new();
    let mut fix = None;
    let mut action = None;
    if let Some(m) = mode(dir)
        && m & 0o077 != 0
    {
        warnings.push(format!("mode {m:04o} is wider than 0700"));
        fix = Some(format!("chmod 700 {}", dir.display()));
        action = Some(FixAction::Chmod {
            path: dir.clone(),
            mode: 0o700,
        });
    }
    if !dir.join("wiki").join(".git").exists() {
        warnings.push("wiki/ is not a git repository (pages are not versioned)".into());
        fix.get_or_insert_with(|| "install git and run `kioku init`".into());
    }
    if warnings.is_empty() {
        check(
            "data_dir",
            Status::Ok,
            format!("{} (0700, wiki is a git repository)", dir.display()),
            None,
        )
    } else {
        Check {
            action,
            ..check(
                "data_dir",
                Status::Warn,
                format!("{}: {}", dir.display(), warnings.join("; ")),
                fix,
            )
        }
    }
}

fn git_check(env: &DoctorEnv) -> Check {
    let out = env.runner.run(&["git", "--version"]);
    if out.success {
        check("git", Status::Ok, out.stdout.trim().to_string(), None)
    } else {
        check(
            "git",
            Status::Warn,
            "git not found: wiki pages are not versioned",
            Some("install git".into()),
        )
    }
}

/// True for a bind address only reachable from this machine (`127.*`, `::1`, `localhost`).
fn is_loopback_bind(bind: &str) -> bool {
    let b = bind.trim().trim_start_matches('[').trim_end_matches(']');
    b.eq_ignore_ascii_case("localhost")
        || b.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// macOS server with a LAN bind (SPEC-M2 §10.3.1): health must answer on the machine's LAN
/// address too — otherwise a firewall (Little Snitch, Local Network privacy) blocks LAN clients.
fn lan_check(cfg: &Config, env: &DoctorEnv) -> Option<Check> {
    if !cfg!(target_os = "macos") || is_loopback_bind(&cfg.server.bind) {
        return None;
    }
    let ip = crate::setup::first_non_loopback_ip()?;
    let mut client = cfg.client.clone();
    client.server_url = format!("http://{ip}:{}", cfg.server.port);
    let url = client.server_url.clone();
    Some(
        match probe_health(&client, env.timeout.min(Duration::from_secs(3))) {
            Health::Kioku { .. } => check(
                "server.lan",
                Status::Ok,
                format!("answers on the LAN at {url}"),
                None,
            ),
            Health::Foreign(d) | Health::Down(d) => check(
                "server.lan",
                Status::Warn,
                format!(
                    "{url} does not answer ({}): a firewall on this Mac blocks LAN clients from kioku",
                    kioku_core::util::truncate_chars(&d, 120)
                ),
                Some(
                    "allow incoming connections for kioku in your firewall (Little Snitch, LuLu, …) or in System Settings > Privacy & Security > Local Network, then kioku service stop && kioku service start".into(),
                ),
            ),
        },
    )
}

fn server_check(cfg: &Config, health: &Health, server_machine: bool, env: &DoctorEnv) -> Check {
    let url = &cfg.client.server_url;
    match health {
        // The version is only told with a valid token (SPEC-M2.7 §11; the token check
        // reports a refused one) and was dropped from /health in M2.7: never compare an
        // unknown version (an older client printed "runs v, this client is v0.7.0").
        Health::Kioku { version } if version.trim().is_empty() => check(
            "server",
            Status::Ok,
            format!("{url} is reachable; version unknown (older client)"),
            None,
        ),
        Health::Kioku { version } if version == VERSION => check(
            "server",
            Status::Ok,
            format!("{url} is reachable (v{version})"),
            None,
        ),
        Health::Kioku { version } => check(
            "server",
            Status::Warn,
            format!("{url} runs v{version}, this client is v{VERSION}"),
            Some("update the older side (install.sh), then restart the server".into()),
        ),
        Health::Foreign(why) => check(
            "server",
            Status::Fail,
            format!("{url} does not answer as kioku: {why}"),
            Some("check [client] server_url; another program may own the port".into()),
        ),
        Health::Down(why) => {
            // SPEC-M3.2 §3.1: "no route" to a LAN address that a system binary reaches is
            // this machine's per-process permission, not the server.
            if let Some(d) = reach_diagnosis(url, why, env) {
                let c = check(
                    "server",
                    Status::Fail,
                    format!("{url} is unreachable: {} ({why})", d.message),
                    Some(d.fix.clone()),
                );
                return if d.local_permission {
                    c.with_action(FixAction::Manual(d.fix))
                } else {
                    c
                };
            }
            check(
                "server",
                Status::Fail,
                format!("{url} is unreachable: {why}"),
                Some(if server_machine {
                    "kioku service start (or kioku service install)".into()
                } else {
                    format!("check that the server at {url} is running and reachable from here")
                }),
            )
        }
    }
}

/// §3.1 for an unreachable `url`: classifies the error, the address, a `/usr/bin/nc -z`
/// probe and the responsible app; `None` when the error is not "unreachable" towards a
/// private address.
fn reach_diagnosis(url: &str, why: &str, env: &DoctorEnv) -> Option<crate::reach::Diagnosis> {
    use crate::reach::{self, AddressClass, ConnectError};
    use std::net::ToSocketAddrs;
    let error = reach::connect_error(why);
    if error == ConnectError::Other {
        return None;
    }
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let resolved: Vec<std::net::IpAddr> = (bare, port)
        .to_socket_addrs()
        .map(|a| a.map(|s| s.ip()).collect())
        .unwrap_or_default();
    let class = reach::address_class(&host, &resolved);
    if class != AddressClass::Private {
        return None;
    }
    let nc = reach::probe_nc(&env.runner, bare, port);
    let app = reach::responsible_app(
        &reach::process_chain(&env.runner, env.pid),
        std::time::SystemTime::now(),
    );
    reach::classify(error, class, nc, url, app.as_ref())
}

fn index_check(s: &StatusReport) -> Check {
    if s.index_schema_expected == 0 {
        return check(
            "index",
            Status::Warn,
            "the server does not report its index schema version (older than M2)",
            Some("update the server".into()),
        );
    }
    match s.index_schema_version {
        // SPEC-M3.1 §2: the user dictionary changed after the index was built.
        Some(v) if v == s.index_schema_expected && s.user_dict_stale => check(
            "index",
            Status::Warn,
            format!(
                "index schema v{v} ({} documents), but dict/user.csv changed after the index was built / ユーザー辞書が索引より新しい",
                s.index_docs
            ),
            Some("kioku reindex".into()),
        )
        .with_action(FixAction::Reindex),
        Some(v) if v == s.index_schema_expected => check(
            "index",
            Status::Ok,
            format!("index schema v{v} ({} documents)", s.index_docs),
            None,
        ),
        other => check(
            "index",
            Status::Warn,
            format!(
                "index schema {} but the server expects v{}: the server rebuilds it in the background after it starts (SPEC-M2.8 §5); search uses the old index until then",
                other
                    .map(|v| format!("v{v}"))
                    .unwrap_or_else(|| "missing".into()),
                s.index_schema_expected
            ),
            Some("if this persists, restart the server (kioku service start on the server machine) and see kioku service logs".into()),
        )
        .with_action(FixAction::Reindex),
    }
}

/// `POST /mcp` initialize with the token; returns `<name> <version>` of the server.
pub fn mcp_initialize(client: &ClientConfig, timeout: Duration) -> anyhow::Result<String> {
    let url = mcp_url(client);
    let parsed = reqwest::Url::parse(&url)?;
    let mut builder = reqwest::blocking::Client::builder().timeout(timeout);
    if is_local_url(&parsed) {
        builder = builder.no_proxy();
    }
    let http = builder.build()?;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "kioku-doctor", "version": VERSION}
        }
    });
    let mut req = http
        .post(parsed.clone())
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(t) = client
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        req = req.bearer_auth(t);
    }
    let mut resp = req
        .send()
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status().as_u16());
    }
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Read until one JSON-RPC answer is complete (an SSE stream may stay open).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let answer = loop {
        if let Some(v) = find_rpc_answer(&String::from_utf8_lossy(&buf)) {
            break Some(v);
        }
        if buf.len() > 256 * 1024 {
            break None;
        }
        match resp.read(&mut chunk) {
            Ok(0) | Err(_) => break find_rpc_answer(&String::from_utf8_lossy(&buf)),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    drop(resp);
    if let Some(id) = session {
        let mut del = http.delete(parsed).header("mcp-session-id", id);
        if let Some(t) = client
            .auth_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            del = del.bearer_auth(t);
        }
        let _ = del.send();
    }
    let answer = answer.ok_or_else(|| anyhow::anyhow!("no initialize result in the response"))?;
    if let Some(err) = answer.get("error") {
        anyhow::bail!("initialize error: {err}");
    }
    let info = &answer["result"]["serverInfo"];
    let name = info["name"].as_str().unwrap_or_default();
    if name.is_empty() {
        anyhow::bail!("initialize returned no serverInfo");
    }
    Ok(
        format!("{name} {}", info["version"].as_str().unwrap_or_default())
            .trim()
            .to_string(),
    )
}

/// The first JSON-RPC response (`result` or `error`) in a JSON body or an SSE stream.
fn find_rpc_answer(text: &str) -> Option<Value> {
    let is_answer = |v: &Value| v.get("result").is_some() || v.get("error").is_some();
    if let Ok(v) = serde_json::from_str::<Value>(text.trim())
        && is_answer(&v)
    {
        return Some(v);
    }
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(is_answer)
}

fn mcp_check(client: &ClientConfig, timeout: Duration) -> Check {
    match mcp_initialize(client, timeout) {
        Ok(info) => check(
            "mcp",
            Status::Ok,
            format!("{} answered initialize ({info})", mcp_url(client)),
            None,
        ),
        Err(e) => check(
            "mcp",
            Status::Fail,
            format!("{}: initialize failed: {e:#}", mcp_url(client)),
            Some("check the server log: kioku service logs".into()),
        ),
    }
}

fn service_check(env: &DoctorEnv, cfg: &Config, health: &Health) -> Check {
    let manager = env.setup_env().service_manager(&cfg.data_dir);
    let up = matches!(health, Health::Kioku { .. });
    if let Platform::Unsupported(_) = manager.platform {
        return check(
            "service",
            Status::Warn,
            "no launchd / systemd --user on this machine: the server must be run by hand, a supervisor or Docker",
            Some(format!(
                "{} serve --log-file {}",
                env.bin,
                cfg.data_dir.join("logs/serve.log").display()
            )),
        );
    }
    let st = manager.state();
    let what = manager.describe();
    // SPEC-M2.8 §5: a Mac nobody is logged in to cannot run the LaunchAgent.
    if matches!(manager.platform, Platform::Launchd) && !(st.installed && st.active) {
        let daemon = Path::new(crate::service::LAUNCH_DAEMON_PATH);
        if daemon.is_file() && up {
            return check(
                "service",
                Status::Ok,
                format!("LaunchDaemon {} (the server answers)", daemon.display()),
                None,
            );
        }
        if manager.gui_session() == Some(false) {
            return check(
                "service",
                if up { Status::Warn } else { Status::Fail },
                crate::service::headless_message().replace('\n', " / "),
                Some("kioku service install --daemon".into()),
            );
        }
    }
    if !st.installed {
        let msg = if up {
            format!(
                "{what} is not installed (the server at {} answers: Docker/k3s?)",
                cfg.client.server_url
            )
        } else {
            format!("{what} is not installed")
        };
        return check(
            "service",
            Status::Warn,
            msg,
            Some("kioku service install".into()),
        )
        .with_action(FixAction::ServiceInstall);
    }
    if !st.active {
        return if up {
            check(
                "service",
                Status::Warn,
                format!(
                    "{what} is installed but not active; the server answering at {} runs elsewhere",
                    cfg.client.server_url
                ),
                Some("kioku service start (or uninstall the service)".into()),
            )
        } else {
            check(
                "service",
                Status::Fail,
                format!("{what} is installed but not running, and the server does not answer"),
                Some("kioku service start; then kioku service logs".into()),
            )
        };
    }
    let pid = st.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
    if st.linger == Some(false) {
        return check(
            "service",
            Status::Warn,
            format!("{what} is active{pid}, but lingering is off: it stops when you log out"),
            Some(
                "loginctl enable-linger (only needed if kioku must run while you are logged out)"
                    .into(),
            ),
        );
    }
    check(
        "service",
        Status::Ok,
        format!("{what} is active{pid}"),
        None,
    )
}

// ---------------------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------------------

/// Our hook commands under one event key (nested or flat format).
fn our_commands_at(agent: Agent, settings: &Value, key: &str) -> Vec<String> {
    hooks_map(agent, settings)
        .map(|m| hook_commands(m, key))
        .unwrap_or_default()
}

/// The binary part of a hook command (`"/a b/kioku" hook stop` → `/a b/kioku`). A
/// PowerShell call operator (`& "C:\…\kioku.exe" hook …`) is skipped, and a backslash only
/// escapes what `sh` quoting escapes (`"`, `\`, `$`, backtick), so Windows paths keep theirs.
pub fn command_binary(cmd: &str) -> String {
    let cmd = cmd.trim();
    let cmd = cmd.strip_prefix("& ").unwrap_or(cmd).trim_start();
    if let Some(rest) = cmd.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.peek() {
                    Some(&n) if matches!(n, '"' | '\\' | '$' | '`') => {
                        chars.next();
                        out.push(n);
                    }
                    _ => out.push('\\'),
                },
                '"' => break,
                c => out.push(c),
            }
        }
        return out;
    }
    match cmd.find(" hook ") {
        Some(i) => cmd[..i].to_string(),
        None => cmd.to_string(),
    }
}

/// The part of our hook command after the binary (` hook stop --agent codex`).
fn command_suffix(cmd: &str) -> String {
    match cmd.rfind(" hook ") {
        Some(i) => cmd[i..].to_string(),
        None => cmd.to_string(),
    }
}

fn hooks_check(agent: Agent, ctx: &InstallCtx) -> Check {
    let id = format!("agent.{}.hooks", agent.as_str());
    let path = hooks_path(agent, ctx, false);
    let fix = Some(format!("kioku install {}", agent.as_str()));
    let settings = match read_settings(&path) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return check(
                &id,
                Status::Warn,
                format!("not installed ({} does not exist)", path.display()),
                fix,
            );
        }
        Err(e) => {
            return check(&id, Status::Warn, format!("{e:#}"), fix);
        }
    };
    let specs = hook_specs(agent, &ctx.bin, ctx.platform);
    let mut missing = Vec::new();
    let mut bins = Vec::new();
    for spec in &specs {
        let handler = if spec.entry.get("command").is_some() {
            &spec.entry
        } else {
            &spec.entry["hooks"][0]
        };
        let expected = crate::install::handler_command_line(handler)
            .as_deref()
            .map(command_suffix)
            .unwrap_or_default();
        let found: Vec<String> = our_commands_at(agent, &settings, &spec.key)
            .into_iter()
            .filter(|c| command_suffix(c) == expected)
            .collect();
        if found.is_empty() {
            missing.push(spec.key.clone());
        }
        bins.extend(found.iter().map(|c| command_binary(c)));
    }
    bins.sort();
    bins.dedup();
    if missing.len() == specs.len() {
        return check(
            &id,
            Status::Warn,
            format!("not installed (no kioku hooks in {})", path.display()),
            fix,
        );
    }
    if let Some(gone) = bins.iter().find(|b| !is_executable(Path::new(b))) {
        return check(
            &id,
            Status::Fail,
            format!(
                "hooks in {} run {gone}, which does not exist (moved binary?)",
                path.display()
            ),
            Some(format!(
                "kioku install {} (re-registers this binary)",
                agent.as_str()
            )),
        );
    }
    let me = canonical(Path::new(&ctx.bin));
    let mut warnings = Vec::new();
    if !missing.is_empty() {
        warnings.push(format!("missing events: {}", missing.join(", ")));
    }
    if let Some(other) = bins.iter().find(|b| canonical(Path::new(b)) != me) {
        warnings.push(format!("hooks run {other}, not this binary"));
    }
    if warnings.is_empty() {
        check(
            &id,
            Status::Ok,
            format!("{} events in {}", specs.len(), path.display()),
            None,
        )
    } else {
        check(
            &id,
            Status::Warn,
            format!("{}: {}", path.display(), warnings.join("; ")),
            fix,
        )
    }
}

/// Our MCP entry as registered: the `kioku mcp` stdio bridge or the v0.3 URL form.
enum McpFound {
    /// `command` + `args` (M2 §20.2).
    Stdio { command: String, args: Vec<String> },
    /// URL and Authorization header.
    Url { url: String, auth: Option<String> },
}

/// Our MCP entry; `Err` = file unparseable, `Ok(None)` = entry missing.
fn mcp_entry(agent: Agent, path: &Path) -> anyhow::Result<Option<McpFound>> {
    if agent == Agent::Codex {
        let Some(text) = block::read_text(path)? else {
            return Ok(None);
        };
        let t: toml::Table = toml::from_str(&text)?;
        let Some(e) = t.get("mcp_servers").and_then(|m| m.get("kioku")) else {
            return Ok(None);
        };
        let s = |k: &str| e.get(k).and_then(toml::Value::as_str).map(str::to_string);
        if let Some(command) = s("command") {
            let args = e
                .get("args")
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Some(McpFound::Stdio { command, args }));
        }
        let auth = e
            .get("http_headers")
            .and_then(|h| h.get("Authorization"))
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        return Ok(Some(McpFound::Url {
            url: s("url").unwrap_or_default(),
            auth,
        }));
    }
    let Some(v) = read_settings(path)? else {
        return Ok(None);
    };
    let Some(e) = v.get("mcpServers").and_then(|m| m.get("kioku")) else {
        return Ok(None);
    };
    if let Some(command) = e.get("command").and_then(Value::as_str) {
        let args = e
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        return Ok(Some(McpFound::Stdio {
            command: command.to_string(),
            args,
        }));
    }
    let url_key = match agent {
        Agent::GeminiCli => "httpUrl",
        Agent::Antigravity => "serverUrl",
        _ => "url",
    };
    let url = e.get(url_key).and_then(Value::as_str).unwrap_or_default();
    let auth = e
        .get("headers")
        .and_then(|h| h.get("Authorization"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(McpFound::Url {
        url: url.to_string(),
        auth,
    }))
}

fn mcp_agent_check(agent: Agent, ctx: &InstallCtx) -> Check {
    let id = format!("agent.{}.mcp", agent.as_str());
    let path = mcp_path(agent, ctx);
    let fix = Some(format!("kioku install {}", agent.as_str()));
    let (url, auth) = match mcp_entry(agent, &path) {
        Ok(Some(McpFound::Stdio { command, args })) => {
            return stdio_mcp_check(&id, &path, &command, &args, ctx, fix);
        }
        Ok(Some(McpFound::Url { url, auth })) => (url, auth),
        Ok(None) => {
            return check(
                &id,
                Status::Fail,
                format!("no `kioku` MCP server in {}", path.display()),
                fix,
            );
        }
        Err(_) => {
            return check(
                &id,
                Status::Warn,
                format!("{} could not be parsed", path.display()),
                Some(format!(
                    "fix the file, then kioku install {}",
                    agent.as_str()
                )),
            );
        }
    };
    let expected_url = mcp_url(&ctx.client);
    let mut warnings = Vec::new();
    if url != expected_url {
        warnings.push(format!("URL is {url}, expected {expected_url}"));
    }
    if let Some(token) = ctx
        .client
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        && auth.as_deref() != Some(format!("Bearer {token}").as_str())
    {
        warnings.push("the Authorization header does not match [client] auth_token".into());
    }
    if warnings.is_empty() {
        check(
            &id,
            Status::Ok,
            format!(
                "{} -> {url} (URL form; `kioku install {}` switches to the kioku mcp bridge)",
                path.display(),
                agent.as_str()
            ),
            None,
        )
    } else {
        check(
            &id,
            Status::Warn,
            format!("{}: {}", path.display(), warnings.join("; ")),
            fix,
        )
    }
}

/// `agent.claude-code.desktop` (SPEC-M2.2 §7.3a): the Claude app's chat / Cowork config at
/// `path` registers the kioku mcp bridge.
fn desktop_check(path: &Path, ctx: &InstallCtx) -> Check {
    let id = "agent.claude-code.desktop";
    let fix = Some(
        "quit the Claude app completely, kioku install claude-code, then start the app".to_string(),
    );
    match mcp_entry(Agent::ClaudeCode, path) {
        Ok(Some(McpFound::Stdio { command, args })) => {
            stdio_mcp_check(id, path, &command, &args, ctx, fix)
        }
        Ok(Some(McpFound::Url { .. })) => check(
            id,
            Status::Warn,
            format!(
                "{}: `kioku` is not the kioku mcp bridge (the Claude app runs local commands only)",
                path.display()
            ),
            fix,
        ),
        Ok(None) => check(
            id,
            Status::Warn,
            format!(
                "no `kioku` MCP server in {}: the Claude app's chat and Cowork cannot use kioku",
                path.display()
            ),
            fix,
        ),
        Err(_) => check(
            id,
            Status::Warn,
            format!("{} could not be parsed", path.display()),
            Some("fix the file, then kioku install claude-code".to_string()),
        ),
    }
}

/// `agent.<a>.mcp` for the stdio bridge: `args == ["mcp"]`, an existing executable, and
/// this binary (M2 §20.2).
fn stdio_mcp_check(
    id: &str,
    path: &Path,
    command: &str,
    args: &[String],
    ctx: &InstallCtx,
    fix: Option<String>,
) -> Check {
    if args != ["mcp"] {
        return check(
            id,
            Status::Warn,
            format!(
                "{}: `kioku` runs {command} {}, not the kioku mcp bridge",
                path.display(),
                args.join(" ")
            ),
            fix,
        );
    }
    if !is_executable(Path::new(command)) {
        return check(
            id,
            Status::Fail,
            format!(
                "{}: runs {command}, which does not exist (moved binary?)",
                path.display()
            ),
            fix,
        );
    }
    if command != ctx.bin {
        return check(
            id,
            Status::Warn,
            format!(
                "{}: runs {command} mcp, not this binary ({})",
                path.display(),
                ctx.bin
            ),
            fix,
        );
    }
    check(
        id,
        Status::Ok,
        format!("{} -> kioku mcp (stdio bridge)", path.display()),
        None,
    )
}

fn instructions_check(agent: Agent, ctx: &InstallCtx) -> Option<Check> {
    if !wants_instructions(agent, &InstallOptions::default()) {
        return None;
    }
    let path = instruction_files(agent, ctx, false).into_iter().next()?;
    let id = format!("agent.{}.instructions", agent.as_str());
    let present = block::read_text(&path).ok().flatten().is_some_and(|t| {
        t.lines()
            .any(|l| l.trim_start().starts_with(MD_MARKERS.begin_prefix))
    });
    Some(if present {
        check(
            &id,
            Status::Ok,
            format!("snippet present in {}", path.display()),
            None,
        )
    } else {
        check(
            &id,
            Status::Warn,
            format!("no kioku snippet in {}", path.display()),
            Some(format!("kioku install {}", agent.as_str())),
        )
    })
}

fn codex_checks(ctx: &InstallCtx, env: &DoctorEnv) -> Vec<Check> {
    let path = mcp_path(Agent::Codex, ctx);
    let version = env.runner.run(&["codex", "--version"]);
    let version = if version.success {
        format!(" ({})", version.stdout.trim())
    } else {
        String::new()
    };
    let disabled = block::read_codex_config(&path)
        .map(|t| block::codex_feature_warnings(&t))
        .unwrap_or_default();
    let feature = if disabled.is_empty() {
        check(
            "agent.codex.feature",
            Status::Ok,
            format!("hooks feature not disabled{version}"),
            None,
        )
    } else {
        check(
            "agent.codex.feature",
            Status::Fail,
            format!("hooks are disabled in {}{version}", path.display()),
            Some(format!(
                "remove `hooks = false` / `codex_hooks = false` from [features] in {}",
                path.display()
            )),
        )
    };
    let trust = if codex_trust_recorded(&path) {
        check(
            "agent.codex.trust",
            Status::Ok,
            "a trusted hash is recorded for kioku's hooks (heuristic)",
            None,
        )
    } else {
        check(
            "agent.codex.trust",
            Status::Warn,
            "cannot verify that Codex trusts kioku's hooks; untrusted hooks do not run",
            Some("open Codex and run /hooks once to confirm".into()),
        )
    };
    vec![feature, trust]
}

fn gemini_enabled_check(ctx: &InstallCtx) -> Check {
    let id = "agent.gemini-cli.enabled";
    let path = hooks_path(Agent::GeminiCli, ctx, false);
    let v = read_settings(&path).ok().flatten().unwrap_or(Value::Null);
    let cfg = v.get("hooksConfig");
    if cfg.and_then(|c| c.get("enabled")).and_then(Value::as_bool) == Some(false) {
        return check(
            id,
            Status::Fail,
            format!(
                "hooks are disabled globally (hooksConfig.enabled = false in {})",
                path.display()
            ),
            Some("remove hooksConfig.enabled = false and restart Gemini CLI".into()),
        );
    }
    let disabled: Vec<String> = cfg
        .and_then(|c| c.get("disabled"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|n| GEMINI_EVENTS.iter().any(|(_, _, name)| name == n))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !disabled.is_empty() {
        return check(
            id,
            Status::Warn,
            format!("kioku hooks disabled by name: {}", disabled.join(", ")),
            Some("remove them from hooksConfig.disabled (or /hooks enable <name>)".into()),
        );
    }
    check(id, Status::Ok, "hooks enabled", None)
}

fn cursor_duplicate_check(ctx: &InstallCtx) -> Option<Check> {
    let claude = read_settings(&hooks_path(Agent::ClaudeCode, ctx, false))
        .ok()
        .flatten()?;
    let claude_installed = crate::event::ALL_EVENTS
        .iter()
        .any(|e| !our_commands_at(Agent::ClaudeCode, &claude, e.claude_code_name()).is_empty());
    if !claude_installed || !is_detected(Agent::Cursor, ctx) {
        return None;
    }
    let native = read_settings(&hooks_path(Agent::Cursor, ctx, false))
        .ok()
        .flatten()
        .is_some_and(|v| {
            v.get("hooks").and_then(Value::as_object).is_some_and(|h| {
                h.keys()
                    .any(|k| !our_commands_at(Agent::Cursor, &v, k).is_empty())
            })
        });
    Some(if native {
        check(
            "agent.cursor.duplicate",
            Status::Ok,
            "native Cursor hooks present; Cursor's runs of the Claude Code hooks are deduplicated",
            None,
        )
    } else {
        check(
            "agent.cursor.duplicate",
            Status::Ok,
            "info: Cursor also runs kioku's Claude Code hooks (third-party hooks); native hooks: kioku install cursor",
            None,
        )
    })
}

fn agent_checks(agent: Agent, ctx: &InstallCtx, env: &DoctorEnv) -> Vec<Check> {
    let mut out = vec![hooks_check(agent, ctx), mcp_agent_check(agent, ctx)];
    if agent == Agent::ClaudeCode {
        out.extend(
            claude_desktop_configs(ctx)
                .iter()
                .map(|p| desktop_check(p, ctx)),
        );
    }
    match agent {
        Agent::Codex => out.extend(codex_checks(ctx, env)),
        Agent::GeminiCli => out.push(gemini_enabled_check(ctx)),
        _ => {}
    }
    out.extend(instructions_check(agent, ctx));
    out.extend(fixture_version_check(agent, env));
    // SPEC-M3.2 §3: what `kioku install <agent>` repairs, `--fix` runs.
    for c in &mut out {
        if c.status != Status::Ok
            && c.fix
                .as_deref()
                .is_some_and(|f| f.contains(&format!("kioku install {}", agent.as_str())))
        {
            c.action = Some(FixAction::Install(agent));
        }
    }
    out
}

/// The captured fixture whose `_meta.captured_with` records the agent version kioku's hook
/// parsing was last checked against (SPEC-M3.3 §4), per agent. Gemini CLI is legacy: none.
pub fn fixture_meta(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::ClaudeCode => Some(include_str!(
            "../tests/fixtures/windows/claude-code/stop.captured.json"
        )),
        Agent::Codex => Some(include_str!("../tests/fixtures/codex/stop.captured.json")),
        Agent::Cursor => Some(include_str!(
            "../tests/fixtures/cursor/session_start.captured.json"
        )),
        Agent::Antigravity => Some(include_str!(
            "../tests/fixtures/antigravity/session_start.captured.json"
        )),
        Agent::GeminiCli => None,
    }
}

/// `_meta.captured_with` of a fixture (`"cursor-agent 2026.09.26-dd393fe"`, `"unknown"`).
pub fn captured_with(fixture: &str) -> Option<String> {
    let v: Value = serde_json::from_str(fixture).ok()?;
    v.get("_meta")?
        .get("captured_with")?
        .as_str()
        .map(str::to_string)
}

/// The command that prints an agent's version (`None`: not obtainable).
pub fn agent_version_command(agent: Agent) -> Option<&'static [&'static str]> {
    match agent {
        Agent::ClaudeCode => Some(&["claude", "--version"]),
        Agent::Codex => Some(&["codex", "--version"]),
        Agent::Cursor => Some(&["cursor-agent", "--version"]),
        Agent::Antigravity => Some(&["agy", "--version"]),
        Agent::GeminiCli => None,
    }
}

/// The first version-looking word of `text` as numbers: `codex-cli 0.46.0` → [0, 46, 0],
/// `2.0.14 (Claude Code)` → [2, 0, 14], `2026.09.26-dd393fe` → [2026, 9, 26].
pub fn version_numbers(text: &str) -> Option<Vec<u64>> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        if !word.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        let core = word.split(['-', '+', '(']).next().unwrap_or(word);
        let nums: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
        nums.filter(|n| !n.is_empty())
    })
}

/// SPEC-M3.3 §4: an INFO check when the installed agent (`installed`: its `--version`
/// output) is newer than the version its fixtures were captured with (`recorded`).
pub fn fixture_version_info(agent: Agent, installed: &str, recorded: &str) -> Option<Check> {
    let now = version_numbers(installed)?;
    let then = version_numbers(recorded)?;
    (now > then).then(|| {
        check(
            &format!("agent.{}.fixtures", agent.as_str()),
            Status::Info,
            format!(
                "{} {} is newer than the version kioku's hook fixtures were captured with ({recorded}); kioku keeps working, its payload tests may lag behind (maintainers: sh scripts/probe-agents.sh --check)",
                agent.as_str(),
                installed.lines().next().unwrap_or_default().trim()
            ),
            None,
        )
    })
}

/// [`fixture_version_info`] for an installed agent (its `--version` through the runner).
fn fixture_version_check(agent: Agent, env: &DoctorEnv) -> Option<Check> {
    let recorded = captured_with(fixture_meta(agent)?)?;
    let out = env.runner.run(agent_version_command(agent)?);
    if !out.success {
        return None;
    }
    fixture_version_info(agent, &out.stdout, &recorded)
}

/// True when any of the agent's kioku hooks is registered (user level).
pub fn hooks_registered(agent: Agent, ctx: &InstallCtx) -> bool {
    let Ok(Some(settings)) = read_settings(&hooks_path(agent, ctx, false)) else {
        return false;
    };
    hook_specs(agent, &ctx.bin, ctx.platform)
        .iter()
        .any(|spec| !our_commands_at(agent, &settings, &spec.key).is_empty())
}

/// `hooks.liveness.<agent>` for every agent in `agents` whose hooks are registered
/// (SPEC-M3.2 §2), plus `hooks.liveness.mcp` once the stdio bridge recorded a tool call
/// (SPEC-M3.4 §1), from `state/last-hook.json`.
pub fn liveness_checks(env: &DoctorEnv, cfg: &Config, agents: &[Agent]) -> Vec<Check> {
    let henv = HookEnv {
        vars: env.vars.clone(),
        home: Some(env.home.clone()),
        cwd: None,
    };
    let marks = crate::liveness::liveness_path(cfg, &henv)
        .map(|p| crate::liveness::load(&p))
        .unwrap_or_default();
    let ctx = env.setup_env().install_ctx(&cfg.client);
    let now = kioku_core::util::now().timestamp();
    let mut checks: Vec<Check> = agents
        .iter()
        .filter(|a| hooks_registered(**a, &ctx))
        .map(|a| {
            let present =
                crate::liveness::agent_present(*a, path_var(&env.vars), &env.home, &env.app_dirs);
            crate::liveness::liveness_check(*a, marks.get(a.as_str()), present, now)
        })
        .collect();
    // SPEC-M3.4 §1: the stdio bridge (desktop apps, no hooks) leaves its own entry.
    if let Some(mark) = marks.get(crate::liveness::MCP_ENTRY) {
        checks.push(crate::liveness::mcp_liveness_check(mark, now));
    }
    checks
}

// ---------------------------------------------------------------------------------------
// Hook log / dump
// ---------------------------------------------------------------------------------------

fn log_dir(cfg: &Config, env: &DoctorEnv) -> PathBuf {
    if cfg.data_dir.is_absolute() && cfg.data_dir.is_dir() {
        cfg.data_dir.join("logs")
    } else {
        env.home.join(".kioku").join("logs")
    }
}

/// SPEC-M2.5 §3.4: effective `[update]`, server vs client version (warn after 24 h of a
/// mismatch), and the last automatic update from `state/auto-update.json`.
pub fn update_check(cfg: &Config, env: &DoctorEnv, health: &Health) -> Check {
    use crate::auto_update::{AutoUpdateState, MISMATCH_WARN, unix_now, within};
    use crate::update::{is_newer, package_manager};
    let state = AutoUpdateState::load(&log_dir(cfg, env).with_file_name("state"));
    let managed = package_manager(Path::new(&env.bin));
    let mut parts = vec![format!(
        "automatic updates {} (channel {})",
        if cfg.update.auto { "on" } else { "off" },
        cfg.update.channel
    )];
    let mut status = Status::Ok;
    let mut fix = None;
    let how = match managed {
        Some(pm) => pm.upgrade_command().to_string(),
        None => "kioku update".to_string(),
    };
    if let Some(pm) = managed {
        parts.push(format!(
            "installed with {0} (updates via {0}: {1})",
            pm.name(),
            pm.upgrade_command()
        ));
    }
    if let Health::Kioku { version } = health
        && !version.is_empty()
    {
        if version == VERSION {
            parts.push(format!("server and client both v{VERSION}"));
        } else {
            if is_newer(VERSION, version) {
                parts.push(format!(
                    "this client (v{VERSION}) is newer than the server (v{version}); clients never downgrade, update the server"
                ));
            } else {
                parts.push(format!("server v{version}, this client v{VERSION}"));
            }
            let since = state.mismatch_since.as_deref();
            if since.is_some() && !within(since, MISMATCH_WARN, unix_now()) {
                status = Status::Warn;
                parts.push(format!(
                    "versions differ since {}",
                    since.unwrap_or_default()
                ));
                fix = Some(if is_newer(VERSION, version) {
                    "on the server machine: kioku update".to_string()
                } else {
                    how.clone()
                });
            }
        }
    }
    if let Some(r) = &state.last_result {
        parts.push(format!("last: {r}"));
    }
    if let Some(e) = &state.last_error {
        parts.push(format!("last error: {e}"));
        if state
            .target
            .as_deref()
            .is_some_and(|t| is_newer(t, VERSION))
        {
            status = Status::Warn;
            fix.get_or_insert(how);
        }
    }
    // A service installed before SPEC-M2.5 lacks the marker and never updates itself; the
    // `kioku update` that brought this version ran the old binary, which could not rewrite it.
    let manager = env.setup_env().service_manager(&cfg.data_dir);
    let mut action = None;
    if cfg.update.auto && manager.is_installed() && manager.lacks_service_marker() {
        status = Status::Warn;
        parts.push("the service definition predates automatic updates".into());
        fix = Some("kioku service install".into());
        action = Some(FixAction::ServiceInstall);
    }
    Check {
        action,
        ..check("update", status, parts.join("; "), fix)
    }
}

fn hook_log_check(cfg: &Config, env: &DoctorEnv) -> Check {
    let path = log_dir(cfg, env).join("hook.log");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let now = kioku_core::util::now();
    let recent: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .next()
                .and_then(kioku_core::util::parse_ts)
                .is_some_and(|ts| (now - ts).num_seconds() <= 24 * 3600)
        })
        .collect();
    match recent.last() {
        None => check(
            "hook_log",
            Status::Ok,
            "no hook errors in the last 24 h",
            None,
        ),
        Some(last) => check(
            "hook_log",
            Status::Warn,
            format!(
                "{} hook error(s) in the last 24 h; last: {}",
                recent.len(),
                kioku_core::util::truncate_chars(last, 300)
            ),
            Some(format!("see {}", path.display())),
        ),
    }
}

fn hook_dump_check(cfg: &Config, env: &DoctorEnv) -> Check {
    let henv = HookEnv {
        vars: env.vars.clone(),
        home: Some(env.home.clone()),
        cwd: None,
    };
    if crate::dump::dump_expired_at(cfg, &henv, kioku_core::util::now().timestamp()) {
        return check(
            "hook_dump",
            Status::Warn,
            "hook dump expired; re-enable to capture (capture stops 24 h after it began)",
            Some(
                "kioku hook-dump enable (another 24 h), or unset KIOKU_HOOK_DUMP / set [client] hook_dump = false"
                    .into(),
            ),
        )
        .with_action(FixAction::ClearHookDump);
    }
    if dump_enabled(cfg, &henv) {
        let path =
            dump_path(cfg).unwrap_or_else(|| log_dir(cfg, env).join(crate::dump::HOOK_DUMP_FILE));
        check(
            "hook_dump",
            Status::Warn,
            format!(
                "hook payload capture is on: raw payloads with possible secrets are being written to {}",
                path.display()
            ),
            Some("unset KIOKU_HOOK_DUMP and set [client] hook_dump = false when done".into()),
        )
    } else {
        check("hook_dump", Status::Ok, "hook payload capture is off", None)
    }
}

// ---------------------------------------------------------------------------------------
// --fix (SPEC-M3.2 §3)
// ---------------------------------------------------------------------------------------

/// What `kioku doctor --fix` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixOutcome {
    /// One line per action (`fixed: …`, `failed: …`, `manual: …`).
    pub lines: Vec<String>,
    /// Actions applied.
    pub applied: usize,
    /// Actions that failed.
    pub failed: usize,
    /// Instructions printed for the user (nothing kioku may change itself).
    pub manual: usize,
}

impl FixOutcome {
    /// 0 when every proposed fix was applied (instructions do not count), else 1.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.failed > 0)
    }
}

/// The distinct actions of `checks`, in check order.
pub fn planned_fixes(checks: &[Check]) -> Vec<FixAction> {
    let mut out: Vec<FixAction> = Vec::new();
    for a in checks.iter().filter_map(|c| c.action.clone()) {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// True unless the Claude desktop app is known not to run: `pgrep -x Claude` (macOS) /
/// `tasklist` (Windows); no such app elsewhere. An unanswerable query counts as running,
/// so its config is left alone.
pub fn claude_app_running(env: &DoctorEnv) -> bool {
    match env.hook_platform {
        crate::install::HookPlatform::Windows => {
            let out = env
                .runner
                .run(&["tasklist", "/FI", "IMAGENAME eq Claude.exe", "/NH"]);
            !out.success || out.stdout.to_ascii_lowercase().contains("claude.exe")
        }
        _ if cfg!(target_os = "macos") || env.runner.is_recording() => {
            let out = env.runner.run(&["pgrep", "-x", "Claude"]);
            out.success || !out.stderr.trim().is_empty()
        }
        _ => false,
    }
}

/// Sets `hook_dump = false` in the `[client]` table of `path`, line by line so comments and
/// the token line stay as they are; `Ok(false)` when it was not set there.
pub fn disable_hook_dump_in_config(path: &Path) -> anyhow::Result<bool> {
    use anyhow::Context;
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let mut section = String::new();
    let mut changed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let t = line.trim();
            if t.starts_with('[') {
                section = t.trim_matches(['[', ']']).trim().to_string();
            } else if section == "client"
                && let Some((k, v)) = t.split_once('=')
                && k.trim() == "hook_dump"
                && v.split('#').next().unwrap_or("").trim() == "true"
            {
                changed = true;
                return "hook_dump = false".to_string();
            }
            line.to_string()
        })
        .collect();
    if !changed {
        return Ok(false);
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    let perms = std::fs::metadata(path).map(|m| m.permissions()).ok();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    if let Some(p) = perms {
        let _ = std::fs::set_permissions(&tmp, p);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(true)
}

/// Applies the safe fixes of `checks` (SPEC-M3.2 §3) and says what it did. Never rotates a
/// token, never changes `[server] bind`, never writes a running Claude app's config.
pub fn apply_fixes(env: &DoctorEnv, checks: &[Check]) -> FixOutcome {
    let mut o = FixOutcome::default();
    let senv = env.setup_env();
    let config_dir = senv.config_dir();
    let cfg = Config::load_from_dir(&config_dir, &env.vars)
        .unwrap_or_else(|_| Config::for_data_dir(&config_dir));
    let done = |o: &mut FixOutcome, what: String, r: anyhow::Result<()>| match r {
        Ok(()) => {
            o.applied += 1;
            o.lines.push(format!("fixed: {what}"));
        }
        Err(e) => {
            o.failed += 1;
            o.lines.push(format!("failed: {what}: {e:#}"));
        }
    };
    for action in planned_fixes(checks) {
        match action {
            FixAction::Chmod { path, mode } => {
                let what = format!("chmod {mode:o} {}", path.display());
                #[cfg(unix)]
                let r = {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                        .map_err(anyhow::Error::from)
                };
                #[cfg(not(unix))]
                let r: anyhow::Result<()> = Ok(());
                done(&mut o, what, r);
            }
            FixAction::Install(agent) => {
                let ctx = senv.install_ctx(&cfg.client);
                let desktop = agent == Agent::ClaudeCode
                    && claude_desktop_configs(&ctx).iter().any(|p| p.exists())
                    && claude_app_running(env);
                let opts = InstallOptions {
                    skip_desktop: desktop,
                    ..InstallOptions::default()
                };
                let r = crate::install::agents::install_agent(agent, &ctx, &opts).map(|_| ());
                done(&mut o, format!("kioku install {}", agent.as_str()), r);
                if desktop {
                    o.manual += 1;
                    o.lines.push(
                        "manual: the Claude app is running and rewrites its config: quit it completely (also from the menu bar / tray), then run kioku doctor --fix again / Claude アプリを完全に終了してから、もう一度 kioku doctor --fix を実行してください".into(),
                    );
                }
            }
            FixAction::ServiceInstall => {
                let r = senv
                    .service_manager(&cfg.data_dir)
                    .install()
                    .map(|act| o.lines.extend(act.lines.iter().map(|l| format!("  {l}"))));
                done(&mut o, "kioku service install".into(), r);
            }
            FixAction::Reindex => {
                let r = ApiClient::new(&cfg.client, REINDEX_TIMEOUT)
                    .and_then(|c| c.post(&["reindex"], &json!({})))
                    .map(|v| {
                        let docs = v.get("docs").and_then(Value::as_u64).unwrap_or(0);
                        o.lines.push(format!("  reindexed {docs} pages"));
                    });
                done(&mut o, "kioku reindex".into(), r);
            }
            FixAction::ClearHookDump => {
                let henv = HookEnv {
                    vars: env.vars.clone(),
                    home: Some(env.home.clone()),
                    cwd: None,
                };
                if henv.var("KIOKU_HOOK_DUMP").is_some() {
                    o.manual += 1;
                    o.lines.push(
                        "manual: KIOKU_HOOK_DUMP is set in the environment: unset it (kioku cannot change your shell profile)".into(),
                    );
                }
                let r = disable_hook_dump_in_config(&config_dir.join(CONFIG_FILE)).and_then(|_| {
                    if let Some(marker) = crate::dump::marker_path(&cfg, &henv)
                        && marker.exists()
                    {
                        std::fs::remove_file(&marker)?;
                    }
                    Ok(())
                });
                done(
                    &mut o,
                    "hook dump turned off ([client] hook_dump = false)".into(),
                    r,
                );
            }
            FixAction::Manual(text) => {
                o.manual += 1;
                o.lines.push(format!("manual: {text}"));
            }
        }
    }
    if o.lines.is_empty() {
        o.lines.push("nothing to fix".into());
    }
    o
}

/// Deadline of the reindex `--fix` asks the server for.
const REINDEX_TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-M3.3 §4: every captured fixture records `_meta.captured_with`; doctor's INFO line
    /// appears only for an agent newer than its fixtures.
    #[test]
    fn fixture_versions_and_the_doctor_info_line() {
        for agent in ALL_AGENTS {
            if let Some(f) = fixture_meta(agent) {
                assert!(captured_with(f).is_some(), "{}", agent.as_str());
            }
        }
        assert_eq!(
            captured_with(fixture_meta(Agent::Cursor).unwrap()).as_deref(),
            Some("cursor-agent 2026.09.26-dd393fe")
        );
        assert_eq!(version_numbers("codex-cli 0.46.0"), Some(vec![0, 46, 0]));
        assert_eq!(
            version_numbers("2.0.14 (Claude Code)"),
            Some(vec![2, 0, 14])
        );
        assert_eq!(
            version_numbers("cursor-agent 2026.09.26-dd393fe"),
            Some(vec![2026, 9, 26])
        );
        assert_eq!(version_numbers("unknown"), None);

        let c = fixture_version_info(
            Agent::Cursor,
            "2026.10.30-abc\n",
            "cursor-agent 2026.09.26-dd393fe",
        )
        .unwrap();
        assert_eq!(c.status, Status::Info);
        assert_eq!(c.id, "agent.cursor.fixtures");
        assert!(c.message.contains("2026.10.30-abc"), "{}", c.message);
        assert!(
            render_text(std::slice::from_ref(&c)).starts_with("[INFO] agent.cursor.fixtures: ")
        );
        assert_eq!(
            render_json(std::slice::from_ref(&c))["checks"][0]["status"],
            "info"
        );
        assert_eq!(exit_code(std::slice::from_ref(&c)), 0);
        // Same, older or unknown: nothing.
        assert!(
            fixture_version_info(
                Agent::Cursor,
                "2026.09.26-dd393fe",
                "cursor-agent 2026.09.26-dd393fe"
            )
            .is_none()
        );
        assert!(fixture_version_info(Agent::Antigravity, "1.2.7", "agy 1.2.12").is_none());
        assert!(fixture_version_info(Agent::Codex, "codex-cli 9.0.0", "unknown").is_none());

        // Through the runner, as `kioku doctor` runs it.
        let home = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            vars: HashMap::new(),
            home: home.path().to_path_buf(),
            bin: home.path().join("bin/kioku").display().to_string(),
            runner: Runner::recording(|argv| {
                if argv.first().map(String::as_str) == Some("agy") {
                    crate::service::CmdOutput::ok("agy 1.3.0\n")
                } else {
                    crate::service::CmdOutput::ok("")
                }
            }),
            platform: None,
            hook_platform: crate::install::HookPlatform::current(),
            timeout: Duration::from_secs(1),
            app_dirs: Vec::new(),
            pid: 0,
        };
        let c = fixture_version_check(Agent::Antigravity, &env).unwrap();
        assert!(
            c.message.starts_with("antigravity agy 1.3.0 is newer"),
            "{}",
            c.message
        );
        assert!(fixture_version_check(Agent::Cursor, &env).is_none());
        assert!(fixture_version_check(Agent::GeminiCli, &env).is_none());
        let text = render_text(&[c, check("x", Status::Ok, "fine", None)]);
        assert!(
            text.ends_with("2 checks: 1 ok, 0 warning(s), 0 failure(s), 1 info\n"),
            "{text}"
        );
    }

    /// SPEC-M2.7 §5: a data directory written by a newer kioku is a FAIL with the way out.
    #[test]
    fn a_newer_schema_is_a_failed_data_dir_check() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let db = kioku_core::DataDir::new(tmp.path()).db_file();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", kioku_core::SCHEMA_VERSION)
            .unwrap();
        drop(conn);
        assert_ne!(data_dir_check(&cfg).status, Status::Fail);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", kioku_core::SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let c = data_dir_check(&cfg);
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.message.contains("written by a newer kioku"),
            "{}",
            c.message
        );
        let fix = c.fix.unwrap();
        assert!(
            fix.contains("kioku update") && fix.contains("kioku restore"),
            "{fix}"
        );
    }

    /// SPEC-M2.5 §3.4: the `update` check (config, 24 h version mismatch, last result).
    #[test]
    fn update_check_reports_config_mismatch_and_last_result() {
        use crate::auto_update::AutoUpdateState;
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".kioku");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = Config::for_data_dir(&data);
        let env = DoctorEnv {
            vars: HashMap::new(),
            home: home.path().to_path_buf(),
            bin: home.path().join("bin/kioku").display().to_string(),
            runner: Runner::recording(|_| crate::service::CmdOutput::ok("")),
            platform: None,
            hook_platform: crate::install::HookPlatform::current(),
            timeout: Duration::from_secs(1),
            app_dirs: Vec::new(),
            pid: 0,
        };
        let same = Health::Kioku {
            version: VERSION.to_string(),
        };
        let c = update_check(&cfg, &env, &same);
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        assert!(c.message.contains("automatic updates on (channel stable)"));
        let newer = Health::Kioku {
            version: "99.0.0".into(),
        };
        // A fresh mismatch is fine; one older than 24 h warns.
        let state = data.join("state");
        let ts = |secs: u64| {
            Some(kioku_core::util::fmt_ts(
                kioku_core::util::now() - Duration::from_secs(secs),
            ))
        };
        AutoUpdateState::update(&state, |s| s.mismatch_since = ts(3600));
        assert_eq!(update_check(&cfg, &env, &newer).status, Status::Ok);
        AutoUpdateState::update(&state, |s| s.mismatch_since = ts(25 * 3600));
        let c = update_check(&cfg, &env, &newer);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert_eq!(c.fix.as_deref(), Some("kioku update"));
        // The last failed attempt at a newer version warns and is shown.
        AutoUpdateState::update(&state, |s| {
            s.mismatch_since = None;
            s.target = Some("v99.0.0".into());
            s.last_error = Some("checksum mismatch".into());
        });
        let c = update_check(&cfg, &env, &newer);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("last error: checksum mismatch"));
        let mut off = cfg.clone();
        off.update.auto = false;
        assert!(
            update_check(&off, &env, &same)
                .message
                .contains("automatic updates off")
        );
    }

    /// Regression (Mac mini, 2026-09-30): `kioku update` 0.6.5 -> 0.7.0 ran the old binary, so
    /// the plist kept no `KIOKU_SERVICE=1` and the server could never update itself, while
    /// doctor said "automatic updates on".
    /// SPEC-M2.8 §5: doctor recognizes a Mac without a logged-in user.
    #[test]
    fn service_check_detects_a_headless_mac() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".kioku");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = Config::for_data_dir(&data);
        let env = DoctorEnv {
            vars: HashMap::new(),
            home: home.path().to_path_buf(),
            bin: home.path().join("bin/kioku").display().to_string(),
            runner: Runner::recording(|argv| match argv.join(" ").as_str() {
                "id -u" => crate::service::CmdOutput::ok("501\n"),
                s if s.starts_with("launchctl print gui/501") => {
                    crate::service::CmdOutput::fail("Could not find domain")
                }
                _ => crate::service::CmdOutput::ok(""),
            }),
            platform: Some(crate::service::Platform::Launchd),
            hook_platform: crate::install::HookPlatform::current(),
            timeout: Duration::from_secs(1),
            app_dirs: Vec::new(),
            pid: 0,
        };
        let c = service_check(&env, &cfg, &Health::Down("connection refused".into()));
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(
            c.message.contains(crate::service::HEADLESS_MAC_JA),
            "{}",
            c.message
        );
        assert!(c.message.contains("--daemon"));
        assert_eq!(c.fix.as_deref(), Some("kioku service install --daemon"));
    }

    /// SPEC-M2.8 §7: an expired capture window is reported.
    #[test]
    fn hook_dump_check_reports_expiry() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".kioku");
        std::fs::create_dir_all(data.join("state")).unwrap();
        let cfg = Config::for_data_dir(&data);
        let env = DoctorEnv {
            vars: [("KIOKU_HOOK_DUMP".to_string(), "1".to_string())].into(),
            home: home.path().to_path_buf(),
            bin: "kioku".into(),
            runner: Runner::recording(|_| crate::service::CmdOutput::ok("")),
            platform: None,
            hook_platform: crate::install::HookPlatform::current(),
            timeout: Duration::from_secs(1),
            app_dirs: Vec::new(),
            pid: 0,
        };
        std::fs::write(
            data.join("state").join(crate::dump::HOOK_DUMP_MARKER),
            "2020-01-01T00:00:00Z\n",
        )
        .unwrap();
        let c = hook_dump_check(&cfg, &env);
        assert_eq!(c.status, Status::Warn);
        assert!(
            c.message
                .contains("hook dump expired; re-enable to capture"),
            "{}",
            c.message
        );
        std::fs::write(
            data.join("state").join(crate::dump::HOOK_DUMP_MARKER),
            format!("{}\n", kioku_core::util::now_ts()),
        )
        .unwrap();
        let c = hook_dump_check(&cfg, &env);
        assert!(c.message.contains("capture is on"), "{}", c.message);
    }

    #[test]
    fn update_check_warns_about_a_service_without_the_marker() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".kioku");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = Config::for_data_dir(&data);
        let env = DoctorEnv {
            vars: HashMap::new(),
            home: home.path().to_path_buf(),
            bin: home.path().join("bin/kioku").display().to_string(),
            runner: Runner::recording(|_| crate::service::CmdOutput::ok("")),
            platform: Some(crate::service::Platform::Launchd),
            hook_platform: crate::install::HookPlatform::current(),
            timeout: Duration::from_secs(1),
            app_dirs: Vec::new(),
            pid: 0,
        };
        let same = Health::Kioku {
            version: VERSION.to_string(),
        };
        let plist = env
            .setup_env()
            .service_manager(&cfg.data_dir)
            .definition_path()
            .unwrap();
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist><dict><key>Label</key></dict></plist>").unwrap();
        let c = update_check(&cfg, &env, &same);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert_eq!(c.fix.as_deref(), Some("kioku service install"));

        std::fs::write(&plist, "<key>KIOKU_SERVICE</key><string>1</string>").unwrap();
        assert_eq!(update_check(&cfg, &env, &same).status, Status::Ok);
    }

    #[test]
    fn path_variable_is_found_in_any_casing() {
        let vars = |k: &str| {
            [(k.to_string(), "/a;/b".to_string())]
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        };
        assert_eq!(path_var(&vars("PATH")), "/a;/b");
        assert_eq!(path_var(&vars("Path")), "/a;/b", "Windows spells it Path");
        assert_eq!(path_var(&std::collections::HashMap::new()), "");
    }

    #[test]
    fn loopback_binds_skip_the_lan_check() {
        for b in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
            assert!(is_loopback_bind(b), "{b}");
        }
        for b in ["0.0.0.0", "192.168.1.240", "::", "kioku.local"] {
            assert!(!is_loopback_bind(b), "{b}");
        }
    }

    #[test]
    fn command_parsing() {
        assert_eq!(command_binary("/opt/kioku hook stop"), "/opt/kioku");
        assert_eq!(
            command_binary("\"/Users/a b/kioku\" hook stop --agent codex"),
            "/Users/a b/kioku"
        );
        assert_eq!(command_binary("\"/x/\\$y/kioku\" hook stop"), "/x/$y/kioku");
        assert_eq!(
            command_suffix("\"/a b/kioku\" hook stop --agent codex"),
            " hook stop --agent codex"
        );
        // Windows forms (SPEC-M2.2 §7): backslashes are kept, `&` is skipped.
        let win = r"C:\Users\u\AppData\Local\Programs\kioku\kioku.exe";
        assert_eq!(
            command_binary(&format!("\"{win}\" hook stop --agent codex")),
            win
        );
        assert_eq!(
            command_binary(&format!("& \"{win}\" hook stop --agent codex")),
            win
        );
    }

    #[test]
    fn hooks_check_understands_the_windows_shapes() {
        use crate::install::HookPlatform;
        use crate::install::agents::{InstallOptions, install_agent};
        let home = tempfile::tempdir().unwrap();
        // A real file standing in for C:\…\kioku.exe (hooks_check wants it to exist).
        let bin_path = home.path().join("Programs").join("kioku").join("kioku.exe");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ctx = InstallCtx {
            home: home.path().to_path_buf(),
            codex_home: home.path().join(".codex"),
            cwd: home.path().to_path_buf(),
            bin: bin_path.display().to_string(),
            client: kioku_core::ClientConfig::default(),
            platform: HookPlatform::Windows,
        };
        for agent in [Agent::ClaudeCode, Agent::Codex, Agent::Cursor] {
            install_agent(agent, &ctx, &InstallOptions::default()).unwrap();
            let c = hooks_check(agent, &ctx);
            assert_eq!(c.status, Status::Ok, "{agent:?}: {}", c.message);
        }
        // A moved binary is reported for the exec form too.
        std::fs::remove_file(&bin_path).unwrap();
        let c = hooks_check(Agent::ClaudeCode, &ctx);
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("kioku.exe"), "{}", c.message);
    }

    #[test]
    fn desktop_check_reports_the_claude_app_config() {
        use crate::install::HookPlatform;
        use crate::install::agents::{InstallOptions, install_agent};
        let home = tempfile::tempdir().unwrap();
        let bin_path = home.path().join("bin").join("kioku");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        std::fs::write(&bin_path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ctx = InstallCtx {
            home: home.path().to_path_buf(),
            codex_home: home.path().join(".codex"),
            cwd: home.path().to_path_buf(),
            bin: bin_path.display().to_string(),
            client: kioku_core::ClientConfig::default(),
            platform: HookPlatform::Unix,
        };
        // No Claude app: no check at all.
        assert!(claude_desktop_configs(&ctx).is_empty());
        let store = home
            .path()
            .join("AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
        std::fs::create_dir_all(&store).unwrap();
        let path = store.join("claude_desktop_config.json");
        std::fs::write(&path, r#"{"preferences":{}}"#).unwrap();
        // The state seen on the user's Windows 2026-09-30: the app, no kioku entry.
        let c = desktop_check(&path, &ctx);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.fix.as_deref().unwrap().contains("quit the Claude app"));
        install_agent(Agent::ClaudeCode, &ctx, &InstallOptions::default()).unwrap();
        let c = desktop_check(&path, &ctx);
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        assert_eq!(c.id, "agent.claude-code.desktop");
    }

    #[test]
    fn rpc_answers_from_json_and_sse() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"serverInfo\":{\"name\":\"kioku\"}}}\n\n";
        assert_eq!(
            find_rpc_answer(sse).unwrap()["result"]["serverInfo"]["name"],
            "kioku"
        );
        assert!(find_rpc_answer("{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{}}").is_some());
        assert!(find_rpc_answer("data: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n").is_none());
        assert!(find_rpc_answer("data: {\"jsonrpc\"").is_none());
    }
}

/// Deadline for `GET /diagnostics` (it reads and hashes the whole wiki on the server).
const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(20);

/// Storage health from `GET /diagnostics` (SPEC-M2.6 §4). Staleness is informational: an
/// idle machine records nothing and a user may keep backups elsewhere, so neither warns.
fn reliability_checks(r: &kioku_core::store::ReliabilityReport) -> Vec<Check> {
    let age = |s: &Option<String>| {
        s.as_deref()
            .and_then(kioku_core::util::parse_ts)
            .map(|t| (kioku_core::util::now() - t).num_hours())
    };
    let backup = match (&r.last_backup, age(&r.last_backup)) {
        (Some(at), Some(h)) if h < 168 => check(
            "backup",
            Status::Ok,
            format!("last completed backup: {at}"),
            None,
        ),
        (Some(at), _) => check(
            "backup",
            Status::Warn,
            format!("last completed backup: {at} (older than 7 days)"),
            Some("kioku backup; copy the snapshot off this server".into()),
        ),
        (None, _) => check(
            "backup",
            Status::Ok,
            "no kioku backup yet (optional: `kioku backup`, then copy the snapshot off this server)",
            None,
        ),
    };
    let drift = !r.inconsistent_pages.is_empty() || !r.index_count_matches;
    let mut out = vec![
        check(
            "recording",
            Status::Ok,
            format!(
                "last observation received: {}",
                r.last_received.as_deref().unwrap_or("never")
            ),
            None,
        ),
        backup,
        check(
            "wiki.git",
            if r.git_error.is_some() {
                Status::Warn
            } else {
                Status::Ok
            },
            r.git_error
                .clone()
                .unwrap_or_else(|| "no recorded commit failure".into()),
            r.git_error.as_ref().map(|_| {
                "check the wiki's git status, disk space and permissions on the server".into()
            }),
        ),
        check(
            "storage.consistency",
            if drift { Status::Warn } else { Status::Ok },
            if drift {
                format!(
                    "{} page(s) differ between wiki files, metadata and the search index{}: {}",
                    r.inconsistent_pages.len(),
                    if r.index_count_matches {
                        ""
                    } else {
                        " (index count differs)"
                    },
                    preview(&r.inconsistent_pages)
                )
            } else {
                "wiki files, metadata and search index agree".into()
            },
            drift.then(|| "kioku reindex (on the server machine)".into()),
        ),
    ];
    if drift && let Some(c) = out.last_mut() {
        c.action = Some(FixAction::Reindex);
    }
    if !r.unparseable_pages.is_empty() {
        out.push(check(
            "storage.unparseable",
            Status::Warn,
            format!(
                "{} page(s) cannot be parsed and are not searchable: {}",
                r.unparseable_pages.len(),
                preview(&r.unparseable_pages)
            ),
            Some("fix their frontmatter by hand (or remove them), then kioku reindex".into()),
        ));
    }
    out
}

/// The first few paths of a list, for one-line messages.
fn preview(paths: &[String]) -> String {
    let mut s = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 3 {
        s.push_str(&format!(", … (+{})", paths.len() - 3));
    }
    s
}

#[cfg(test)]
mod reliability_tests {
    use super::*;
    #[test]
    fn recording_backup_and_integrity_have_distinct_health_signals() {
        let mut report = kioku_core::store::ReliabilityReport {
            last_received: None,
            last_backup: None,
            git_error: None,
            inconsistent_pages: vec![],
            unparseable_pages: vec![],
            index_count_matches: true,
        };
        // A fresh or idle install is healthy: nothing recorded yet and no backup is not a
        // warning.
        let checks = reliability_checks(&report);
        assert!(checks.iter().all(|c| c.status == Status::Ok), "{checks:?}");
        report.last_backup = Some("2020-01-01T00:00:00.000Z".into());
        report.git_error = Some("git commit failed".into());
        report
            .inconsistent_pages
            .push("project/pages/記憶.md".into());
        report.unparseable_pages.push("_global/壊れた.md".into());
        let checks = reliability_checks(&report);
        let status = |name: &str| checks.iter().find(|c| c.id == name).unwrap().status;
        assert_eq!(status("backup"), Status::Warn);
        assert_eq!(status("wiki.git"), Status::Warn);
        assert_eq!(status("storage.consistency"), Status::Warn);
        assert_eq!(status("storage.unparseable"), Status::Warn);
        assert_eq!(exit_code(&checks), 0);
    }
}
