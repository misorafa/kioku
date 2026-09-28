//! Implementations of the commands: the `hook` entry point (config / stdin / dump
//! plumbing around the handlers), init, serve, search, status, reindex, project id,
//! install / uninstall, `hook-dump extract`, and the machine-setup commands `setup`,
//! `service` and `doctor` (M2 §10–§12).

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use kioku_core::util::home_dir;
use kioku_core::{Config, Hit, StatusReport, Store, identify};
use serde_json::Value;

use crate::cli::{
    Cli, Command, HookDumpCommand, InstallTarget, ProjectCommand, ScopeArg, ServiceCommand,
};
use crate::client::{ApiClient, COMMAND_TIMEOUT};
use crate::dump;
use crate::event::{Agent, HookEnv, HookEventKind};
use crate::hook::log_failure;
use crate::install::agents::{
    AgentReport, AllStatus, InstallCtx, InstallOptions, Instructions, install_agent, install_all,
    post_install_notes, uninstall_agent, uninstall_all, unstable_binary_warning,
};
use crate::render::{HookResult, render};

/// Runs a parsed command line; returns the process exit code.
pub fn run(cli: Cli) -> i32 {
    let result = match cli.command {
        Command::Hook { event, agent } => return hook(event, agent),
        Command::HookDump {
            command: HookDumpCommand::Extract { agent, event, out },
        } => hook_dump_extract(agent, &event, out),
        Command::Init { client_only } => match client_only.as_deref() {
            Some([url, token]) => init_client_only(url, token),
            Some(_) => Err(anyhow::anyhow!("--client-only takes <url> <token>")),
            None => init(),
        },
        Command::Serve {
            bind,
            port,
            log_file,
        } => serve(bind, port, log_file),
        Command::Setup {
            client_only,
            no_service,
            no_agents,
            agents,
            bind,
            no_instructions,
            dry_run,
            mcp_http,
            print_client_command,
        } => {
            let opts = crate::setup::SetupOptions {
                client_only: client_only.and_then(|v| match v.as_slice() {
                    [url, token] => Some((url.clone(), token.clone())),
                    _ => None,
                }),
                no_service,
                no_agents,
                agents,
                bind,
                no_instructions,
                dry_run,
                mcp_http,
                print_client_command,
            };
            return setup(&opts);
        }
        Command::Service { command } => service(command),
        Command::Doctor { json, agent } => return doctor(json, agent),
        Command::Search {
            query,
            project,
            scope,
            limit,
        } => search(&query.join(" "), project, scope, limit),
        Command::Install {
            target,
            project,
            no_instructions,
            instructions,
            dry_run,
            agents,
            enable_hooks_feature,
            trust_mcp,
            mcp_http,
        } => {
            let opts = InstallOptions {
                project,
                instructions: if no_instructions {
                    Instructions::Skip
                } else if instructions {
                    Instructions::Force
                } else {
                    Instructions::Default
                },
                dry_run,
                enable_hooks_feature,
                trust_mcp,
                mcp_http,
            };
            install_cmd(target, &opts, &agents)
        }
        Command::Uninstall {
            target,
            project,
            dry_run,
        } => uninstall_cmd(target, project, dry_run),
        Command::Project {
            command: ProjectCommand::Id { path },
        } => project_id(path),
        Command::Update { version, check } => match crate::update::run_update(version, check) {
            Ok(code) => return code,
            Err(err) => Err(err),
        },
        Command::Reindex => reindex(),
        Command::Status => status(),
        Command::RotateToken { dry_run } => return rotate_token(dry_run),
        Command::Mcp => Config::load()
            .map_err(anyhow::Error::from)
            .and_then(|cfg| crate::bridge::run(cfg.client)),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            1
        }
    }
}

fn env_map() -> HashMap<String, String> {
    kioku_core::util::env_vars()
}

/// `kioku hook <event> --agent <a>`: stdin → server → stdout/stderr/exit code. Always
/// fail-open; with `KIOKU_HOOK_DUMP` the invocation is also captured (M2 §3.8).
fn hook(event: HookEventKind, agent: Agent) -> i32 {
    let env = HookEnv::from_process();
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut stdin = String::new();
    let stdin_err = std::io::stdin().read_to_string(&mut stdin).err();
    let outcome = match Config::load() {
        Ok(cfg) => match stdin_err {
            None => dump::run_hook_invocation(event, agent, &argv, &stdin, &cfg, &env),
            Some(err) => {
                log_failure(
                    &cfg,
                    event,
                    "-",
                    &anyhow::Error::new(err).context("reading stdin"),
                );
                let outcome = render(agent, event, HookResult::Silent);
                dump::dump_invocation(&cfg, &env, agent, event, &argv, &stdin, &outcome);
                outcome
            }
        },
        Err(err) => {
            let fallback = Config::for_data_dir(&home_dir().join(".kioku"));
            log_failure(
                &fallback,
                event,
                "-",
                &anyhow::Error::new(err).context("loading config"),
            );
            let outcome = render(agent, event, HookResult::Silent);
            dump::dump_invocation(&fallback, &env, agent, event, &argv, &stdin, &outcome);
            outcome
        }
    };
    print!("{}", outcome.stdout);
    eprint!("{}", outcome.stderr);
    outcome.exit_code
}

/// `kioku hook-dump extract <agent> <event> [--out dir]`.
fn hook_dump_extract(agent: Agent, event: &str, out: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let path = dump::dump_path(&cfg).context("no log directory (HOME is not set)")?;
    let out = match out {
        Some(o) => o,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let written = dump::extract(&path, agent, event, &out)?;
    println!("wrote {}", written.display());
    Ok(())
}

fn init() -> anyhow::Result<()> {
    let env = env_map();
    let mut cfg = Config::load_with_env(&env)?;
    if !cfg.config_file.exists() && !env.contains_key("KIOKU_SERVER_URL") {
        cfg.client.server_url = format!("http://127.0.0.1:{}", cfg.server.port);
    }
    let report = kioku_core::init(&mut cfg)?;
    println!("kioku initialized");
    println!("  data dir : {}", report.data_dir);
    println!(
        "  config   : {} ({})",
        report.config_file,
        if report.config_written {
            "written"
        } else {
            "unchanged"
        }
    );
    println!(
        "  token    : {}",
        if report.token_generated {
            "generated (auth_token in config.toml)"
        } else {
            "kept existing"
        }
    );
    println!(
        "  git      : {}",
        if report.git_enabled {
            "enabled (wiki/ is a git repository)"
        } else {
            "disabled (git not found; pages are not versioned)"
        }
    );
    println!();
    println!("Next steps:");
    println!("  1. kioku serve                    # start the server (keep it running)");
    println!("  2. kioku install all              # hooks + MCP for every agent on this machine");
    println!(
        "  3. other machines: kioku init --client-only http://<this-host>:{} <auth_token from {}>",
        cfg.server.port, report.config_file
    );
    if cfg.is_loopback_bind() {
        println!(
            "     (set [server] bind = \"0.0.0.0\" first so other machines can reach this server)"
        );
    }
    Ok(())
}

fn init_client_only(url: &str, token: &str) -> anyhow::Result<()> {
    reqwest::Url::parse(url).with_context(|| format!("invalid server URL: {url}"))?;
    if token.trim().is_empty() {
        anyhow::bail!("the auth token must not be empty");
    }
    let env = env_map();
    let path = Config::load_with_env(&env)?.config_file;
    let cfg = Config::write_client_only(&path, url, token.trim())?;
    println!("kioku client configured");
    println!("  config : {}", path.display());
    println!("  server : {}", cfg.client.server_url);
    let check =
        ApiClient::new(&cfg.client, Duration::from_secs(5)).and_then(|c| c.get(&["status"], &[]));
    match check {
        Ok(_) => println!("  check  : server reachable, token accepted"),
        Err(err) => println!("  check  : FAILED — {err:#}"),
    }
    println!();
    println!("Next step:");
    println!("  kioku install all              # hooks + MCP for every agent on this machine");
    Ok(())
}

fn serve(bind: Option<String>, port: Option<u16>, log_file: Option<PathBuf>) -> anyhow::Result<()> {
    let mut cfg = Config::load()?;
    if let Some(bind) = bind {
        cfg.server.bind = bind;
    }
    if let Some(port) = port {
        cfg.server.port = port;
    }
    let has_token = cfg
        .server
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if !has_token {
        anyhow::bail!(
            "refusing to start without an auth token: run `kioku init` (writes [server] auth_token to {}) or set KIOKU_AUTH_TOKEN",
            cfg.config_file.display()
        );
    }
    init_tracing(log_file.as_deref())?;
    let (bind, port) = (cfg.server.bind.clone(), cfg.server.port);
    let data_dir = cfg.data_dir.clone();
    let store = Arc::new(Store::open(cfg).context("opening the data directory")?);
    let host = if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]")
    } else {
        bind.clone()
    };
    println!("kioku serve: http://{host}:{port} (API /api/v1, MCP /mcp)");
    println!("data dir: {}", data_dir.display());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(kioku_server::serve(store, bind, port))
}

fn init_tracing(log_file: Option<&std::path::Path>) -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tantivy=warn"));
    match log_file {
        Some(path) => {
            let writer = crate::logfile::RotatingFile::open(path)
                .with_context(|| format!("opening log file {}", path.display()))?;
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
    Ok(())
}

fn command_client() -> anyhow::Result<(Config, ApiClient)> {
    let cfg = Config::load()?;
    let client = ApiClient::new(&cfg.client, COMMAND_TIMEOUT)?;
    Ok((cfg, client))
}

fn search(
    query: &str,
    project: Option<String>,
    scope: Option<ScopeArg>,
    limit: Option<usize>,
) -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let mut params = vec![("q", query.to_string())];
    if let Some(p) = project {
        params.push(("project", p));
    }
    if let Some(s) = scope {
        params.push(("scope", s.as_str().to_string()));
    }
    if let Some(l) = limit {
        params.push(("limit", l.to_string()));
    }
    let body = client.get(&["search"], &params)?;
    let hits: Vec<Hit> = serde_json::from_value(body.get("hits").cloned().unwrap_or(Value::Null))
        .context("unexpected search response")?;
    print!("{}", format_hits(&hits));
    Ok(())
}

/// Human-readable hit list (same shape as the `kioku_query` MCP tool).
pub fn format_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no hits\n".to_string();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let global = if h.global { " [global]" } else { "" };
        out.push_str(&format!(
            "{}. {} — {} ({:.2}){global}\n",
            i + 1,
            h.path,
            h.title,
            h.score
        ));
        if !h.snippet.trim().is_empty() {
            out.push_str(&format!("   {}\n", kioku_core::util::one_line(&h.snippet)));
        }
    }
    out
}

fn status() -> anyhow::Result<()> {
    let (cfg, client) = command_client()?;
    let version = client
        .get(&["health"], &[])
        .ok()
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let body = client.get(&["status"], &[])?;
    let s: StatusReport = serde_json::from_value(body).context("unexpected status response")?;
    println!("server       : {} (kioku {version})", cfg.client.server_url);
    println!("data dir     : {}", s.data_dir);
    println!("projects     : {}", s.projects);
    println!("pages        : {}", s.pages);
    println!("sessions     : {}", s.sessions);
    println!("observations : {}", s.observations);
    println!("handoffs     : {}", s.handoffs);
    println!("index docs   : {}", s.index_docs);
    println!(
        "git          : {}",
        if s.git_enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn reindex() -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let body = client.post(&["reindex"], &Value::Object(Default::default()))?;
    let docs = body.get("docs").and_then(Value::as_u64).unwrap_or(0);
    println!("reindexed {docs} pages");
    Ok(())
}

fn project_id(path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => p,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let project = identify(&path)?;
    println!("{}", project.id);
    Ok(())
}

fn current_binary() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("locating the kioku binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    Ok(exe.display().to_string())
}

/// `kioku setup` (M2 §11); returns the exit code.
fn setup(opts: &crate::setup::SetupOptions) -> i32 {
    let env = match current_binary().and_then(crate::setup::SetupEnv::from_process) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            return 1;
        }
    };
    let report = crate::setup::run_setup(opts, &env);
    print!("{}", report.render());
    report.exit_code()
}

/// `kioku rotate-token [--dry-run]` (M2 §21); returns the exit code.
fn rotate_token(dry_run: bool) -> i32 {
    let env = match current_binary().and_then(crate::setup::SetupEnv::from_process) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            return 1;
        }
    };
    let report = crate::rotate::run_rotate(dry_run, &env);
    for line in &report.lines {
        if line.starts_with("kioku: error") {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    report.exit_code
}

/// `kioku doctor [--json] [--agent <name>]` (M2 §12); returns the exit code.
fn doctor(json: bool, agent: Option<Agent>) -> i32 {
    let bin = match current_binary() {
        Ok(b) => b,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            return 1;
        }
    };
    let env = crate::doctor::DoctorEnv::from_process(bin);
    let checks = crate::doctor::run_doctor(&env, agent);
    if json {
        let v = crate::doctor::render_json(&checks);
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        print!("{}", crate::doctor::render_text(&checks));
    }
    crate::doctor::exit_code(&checks)
}

/// `kioku service …` (M2 §10.2). Refuses on a client-only machine.
fn service(command: ServiceCommand) -> anyhow::Result<()> {
    use crate::service::{Health, Platform, probe_health};
    let env = crate::setup::SetupEnv::from_process(current_binary()?)?;
    let config_path = env.config_dir().join(kioku_core::config::CONFIG_FILE);
    let cfg = Config::load_from_dir(&env.config_dir(), &env.vars)?;
    let server_section = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .map(|t| t.contains_key("server"));
    if server_section == Some(false) {
        anyhow::bail!(
            "this machine is configured as a client only (no [server] section in {}); the service runs on the server machine",
            config_path.display()
        );
    }
    let manager = env.service_manager(&cfg.data_dir);
    let print = |act: crate::service::ServiceAction| {
        for l in act.lines {
            println!("{l}");
        }
    };
    match command {
        ServiceCommand::Install | ServiceCommand::Start => {
            let has_token = cfg
                .server
                .auth_token
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
            if server_section.is_none() || !has_token {
                anyhow::bail!(
                    "no server config with an auth token at {}: run `kioku setup` (or `kioku init`) first",
                    config_path.display()
                );
            }
            if let Some(w) = unstable_binary_warning(&env.bin) {
                println!("{w}");
            }
            let act = if matches!(command, ServiceCommand::Install) {
                manager.install()?
            } else {
                manager.start()?
            };
            print(act);
        }
        ServiceCommand::Uninstall => print(manager.uninstall()?),
        ServiceCommand::Stop => print(manager.stop()?),
        ServiceCommand::Status => {
            if let Platform::Unsupported(why) = &manager.platform {
                println!("service : none ({why})");
            } else {
                let st = manager.state();
                let def = manager.definition_path().unwrap_or_default();
                println!(
                    "service : {} ({})",
                    manager.describe(),
                    if st.installed {
                        format!("installed: {}", def.display())
                    } else {
                        "not installed".to_string()
                    }
                );
                let pid = st.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
                println!(
                    "state   : {}",
                    if st.active {
                        format!("active{pid}")
                    } else {
                        "not active".to_string()
                    }
                );
                if let Some(l) = st.linger {
                    println!(
                        "linger  : {}",
                        if l { "on" } else { "off (stops at logout)" }
                    );
                }
            }
            let url = &cfg.client.server_url;
            match probe_health(&cfg.client, Duration::from_secs(3)) {
                Health::Kioku { version } => println!("health  : ok - {url} (v{version})"),
                Health::Foreign(why) => println!("health  : not kioku - {url}: {why}"),
                Health::Down(why) => println!("health  : unreachable - {url}: {why}"),
            }
            println!("log     : {}", manager.spec.log_file().display());
        }
        ServiceCommand::Logs { follow, lines } => {
            crate::service::tail_log(&manager.spec.log_file(), lines, follow)?;
        }
    }
    Ok(())
}

fn print_report(r: &AgentReport, indent: &str) {
    for line in &r.lines {
        for l in line.lines() {
            println!("{indent}{l}");
        }
    }
}

/// `kioku install <agent>|all …` (M2 §8).
fn install_cmd(
    target: InstallTarget,
    opts: &InstallOptions,
    agents: &[Agent],
) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let bin = current_binary()?;
    if let Some(w) = unstable_binary_warning(&bin) {
        println!("{w}");
    }
    let ctx = InstallCtx::from_process(cfg.client.clone(), bin)?;
    if opts.dry_run {
        println!("dry run: nothing is written");
    }
    let mut failed = 0;
    match target.agent() {
        Some(agent) => {
            if !agents.is_empty() {
                anyhow::bail!("--agents only applies to `kioku install all`");
            }
            let r = install_agent(agent, &ctx, opts)?;
            println!("{}:", agent.as_str());
            print_report(&r, "  ");
            for note in post_install_notes(agent, opts, &r) {
                println!("{note}");
            }
        }
        None => {
            for (agent, status) in install_all(&ctx, opts, agents) {
                let name = agent.as_str();
                match status {
                    AllStatus::Changed(r) | AllStatus::Unchanged(r) => {
                        let what = if r.changed {
                            if opts.dry_run {
                                "would change"
                            } else {
                                "installed"
                            }
                        } else {
                            "unchanged"
                        };
                        println!("{name:<12} {what}");
                        print_report(&r, "  ");
                        for note in post_install_notes(agent, opts, &r) {
                            println!("  {note}");
                        }
                    }
                    AllStatus::NotDetected(dir) => {
                        println!(
                            "{name:<12} skipped: not detected ({} missing)",
                            dir.display()
                        );
                    }
                    AllStatus::Error(e) => {
                        failed += 1;
                        println!("{name:<12} error:");
                        for l in e.lines() {
                            println!("  {l}");
                        }
                    }
                }
            }
        }
    }
    if cfg
        .client
        .auth_token
        .as_deref()
        .is_none_or(|t| t.trim().is_empty())
    {
        println!(
            "warning: no [client] auth_token in {}; run `kioku init` (or `kioku init --client-only <url> <token>`) so hooks can authenticate",
            cfg.config_file.display()
        );
    }
    if !opts.dry_run {
        println!("Restart running agents so they pick up the new hooks and MCP server.");
    }
    if failed > 0 {
        anyhow::bail!("{failed} agent(s) failed");
    }
    Ok(())
}

/// `kioku uninstall <agent>|all [--project]`.
fn uninstall_cmd(target: InstallTarget, project: bool, dry_run: bool) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let bin = current_binary()?;
    let ctx = InstallCtx::from_process(cfg.client.clone(), bin)?;
    match target.agent() {
        Some(agent) => {
            let r = uninstall_agent(agent, &ctx, project, dry_run)?;
            println!("{}:", agent.as_str());
            print_report(&r, "  ");
            Ok(())
        }
        None => {
            let mut failed = 0;
            for (agent, status) in uninstall_all(&ctx, project, dry_run) {
                let name = agent.as_str();
                match status {
                    AllStatus::Changed(r) => {
                        println!("{name:<12} removed");
                        print_report(&r, "  ");
                    }
                    AllStatus::Unchanged(_) | AllStatus::NotDetected(_) => {
                        println!("{name:<12} nothing to remove");
                    }
                    AllStatus::Error(e) => {
                        failed += 1;
                        println!("{name:<12} error: {e}");
                    }
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed} agent(s) failed");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_formatting() {
        assert_eq!(format_hits(&[]), "no hits\n");
        let hits = vec![Hit {
            path: "_global/rust.md".into(),
            title: "Rust の書き方".into(),
            kind: "page".into(),
            snippet: "【引き継ぎ】を\n自動化".into(),
            score: 1.234,
            updated: "2026-09-25T00:00:00Z".into(),
            global: true,
        }];
        assert_eq!(
            format_hits(&hits),
            "1. _global/rust.md — Rust の書き方 (1.23) [global]\n   【引き継ぎ】を 自動化\n"
        );
    }
}
