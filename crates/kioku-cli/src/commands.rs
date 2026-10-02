//! Implementations of the commands: the `hook` entry point (config / stdin / dump
//! plumbing around the handlers), init, serve, search, status, reindex, project id / merge,
//! install / uninstall, `hook-dump extract`, and the machine-setup commands `setup`,
//! `service` and `doctor` (M2 §10–§12).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use kioku_core::util::home_dir;
use kioku_core::{Config, Hit, MergeReport, StatusReport, Store, identify};
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
        Command::Backup => (|| -> anyhow::Result<()> {
            let cfg = Config::load()?;
            let body = ApiClient::new(&cfg.client, COMMAND_TIMEOUT)?
                .post(&["backup"], &serde_json::json!({}))?;
            let m: kioku_core::store::BackupManifest =
                serde_json::from_value(body).context("unexpected backup response")?;
            let bytes: u64 = m.files.values().map(|f| f.bytes).sum();
            println!(
                "backup {} complete: {} files, {:.1} MiB, {} sessions, {} observations, {} pages\n  {}\n(on the server machine; copy it elsewhere, test with: kioku restore <dir> --into <new dir>)",
                m.created,
                m.files.len(),
                bytes as f64 / (1024.0 * 1024.0),
                m.counts.get("sessions").copied().unwrap_or(0),
                m.counts.get("observations").copied().unwrap_or(0),
                m.counts.get("pages").copied().unwrap_or(0),
                m.path
            );
            Ok(())
        })(),
        Command::Restore {
            backup,
            into,
            verify: _,
        } => (|| -> anyhow::Result<()> {
            let manifest = kioku_core::store::restore_backup(&backup, &into)?;
            println!(
                "restored {} to {} (checksums, SQLite and index verified; run KIOKU_DATA_DIR=<restored-directory> kioku init to create fresh credentials)",
                manifest.created,
                into.display()
            );
            Ok(())
        })(),
        Command::Sync => (|| -> anyhow::Result<()> {
            let cfg = Config::load()?;
            let r = crate::outbox::flush(&cfg)?;
            println!(
                "synced {} queued observation(s); {} refused by the server (kept in {})",
                r.delivered,
                r.refused,
                crate::outbox::directory(&cfg).join("failed").display()
            );
            Ok(())
        })(),
        Command::Hook { event, agent } => return hook(event, agent),
        Command::HookDump {
            command: HookDumpCommand::Extract { agent, event, out },
        } => hook_dump_extract(agent, &event, out),
        Command::HookDump {
            command: HookDumpCommand::Enable,
        } => hook_dump_enable(),
        Command::Init { client_only } => match client_only.as_deref() {
            Some([url, token]) => init_client_only(url, token),
            Some(_) => Err(anyhow::anyhow!("--client-only takes <url> <token>")),
            None => init(),
        },
        Command::Serve {
            bind,
            port,
            log_file,
        } => match serve(bind, port, log_file) {
            Ok(code) => return code,
            Err(err) => Err(err),
        },
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
            let client_only = match client_only.as_deref() {
                None => None,
                Some(args) => {
                    let stdin = std::io::stdin();
                    let tty = std::io::IsTerminal::is_terminal(&stdin);
                    match crate::setup::client_token(
                        args.get(1).map(String::as_str),
                        &env_map(),
                        &mut stdin.lock(),
                        tty,
                    ) {
                        Ok((token, warning)) => {
                            if let Some(w) = warning {
                                eprintln!("{w}");
                            }
                            Some((args[0].clone(), token))
                        }
                        Err(err) => {
                            eprintln!("kioku: error: {err:#}");
                            return 1;
                        }
                    }
                }
            };
            let opts = crate::setup::SetupOptions {
                client_only,
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
        Command::Invite { ttl, uses, host } => {
            let opts = crate::invite::InviteOptions {
                ttl_minutes: ttl,
                uses,
                host,
            };
            return with_setup_env(|env| crate::invite::run_invite(&opts, env));
        }
        Command::Join {
            url,
            code,
            agents,
            no_agents,
            no_instructions,
            mcp_http,
        } => {
            let opts = crate::invite::JoinOptions {
                agents,
                no_agents,
                no_instructions,
                mcp_http,
            };
            return with_setup_env(|env| crate::invite::run_join(&url, &code, &opts, env));
        }
        Command::Service { command } => service(command),
        Command::Doctor { json, agent } => return doctor(json, agent),
        Command::Search {
            query,
            project,
            scope,
            limit,
            since,
            kinds,
            path_prefix,
        } => search(
            &query.join(" "),
            project,
            scope,
            limit,
            SearchFilters {
                since,
                kinds,
                path_prefix,
            },
        ),
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
        Command::Project {
            command:
                ProjectCommand::Merge {
                    from,
                    into,
                    dry_run,
                },
        } => project_merge(&from, &into, dry_run),
        Command::Update {
            version,
            check,
            background,
            require_signature,
            rollback,
        } => {
            let args = crate::update::UpdateArgs {
                version,
                check,
                background,
                require_signature,
                rollback,
            };
            match crate::update::run_update(args) {
                Ok(code) => return code,
                Err(err) => Err(err),
            }
        }
        Command::Reindex => reindex(),
        Command::Prune { dry_run } => prune(dry_run),
        Command::Forget {
            session,
            project,
            purge_history,
            yes,
        } => forget(session, project, purge_history, yes),
        Command::Status => status(),
        Command::RotateToken {
            dry_run,
            show_token,
        } => return rotate_token(dry_run, show_token),
        Command::Mcp => crate::bridge::run(kioku_core::util::env_vars()),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            1
        }
    }
}

/// True when `argv` (with the program name first) runs `kioku hook …`.
pub fn is_hook_invocation(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|a| a == "hook")
}

/// Appends a hook command line that did not parse to `hook.log` (agent `unknown`), as one
/// line; never fails and prints nothing (SPEC-M2.7 §1).
pub fn log_hook_parse_failure(message: &str) {
    let cfg = Config::load().unwrap_or_else(|_| Config::for_data_dir(&home_dir().join(".kioku")));
    if let Some(path) = crate::hook::hook_log_path(&cfg) {
        let line = format!(
            "{} hook agent=unknown session=- error: {}\n",
            kioku_core::util::now_ts(),
            kioku_core::util::one_line(message)
        );
        let _ = crate::hook::append_line(&path, &line, crate::hook::HOOK_LOG_MAX_BYTES, false);
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
    // Bytes, not read_to_string: a BOM or one invalid byte must not lose the payload.
    let mut raw = Vec::new();
    let stdin_err = std::io::stdin().read_to_end(&mut raw).err();
    let stdin = crate::hook::decode_stdin(&raw);
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
    // UTF-8 bytes straight to the pipes (never an ANSI code page; SPEC-M2.2 §4.4).
    let _ = std::io::stdout().write_all(outcome.stdout.as_bytes());
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(outcome.stderr.as_bytes());
    let _ = std::io::stderr().flush();
    // SPEC-M2.5 §3.3: after the context is out, follow the server's version in a detached
    // updater (the hook never waits for it).
    if let Ok(cfg) = Config::load() {
        if let Some(tag) = &outcome.spawn_update
            && let Err(err) = crate::auto_update::spawn_background(tag)
        {
            log_failure(&cfg, event, "-", &err);
        }
        // SPEC-M2.6 §3: replay failed deliveries in the background (backs off after a spawn
        // or a failed replay; a no-op when nothing is queued).
        if let Err(err) = crate::outbox::spawn_sync_if_due(&cfg) {
            log_failure(&cfg, event, "-", &err);
        }
    }
    outcome.exit_code
}

/// `kioku hook-dump extract <agent> <event> [--out dir]`; the default output directory is
/// `~/.kioku/captures/<date>/`, never the current directory (SPEC-M2.8 §7).
fn hook_dump_extract(agent: Agent, event: &str, out: Option<PathBuf>) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let env = HookEnv::from_process();
    let path = dump::dump_path(&cfg).context("no log directory (HOME is not set)")?;
    let out = match out {
        Some(o) => o,
        None => dump::default_capture_dir(&cfg, &env, &kioku_core::util::now_ts())
            .context("no kioku directory (HOME is not set)")?,
    };
    let written = dump::extract(&path, agent, event, &out)?;
    println!("wrote {}", written.display());
    Ok(())
}

/// `kioku hook-dump enable`: restarts the 24-hour capture window (SPEC-M2.8 §7).
fn hook_dump_enable() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let env = HookEnv::from_process();
    let marker = dump::reset_window(&cfg, &env)?;
    println!(
        "hook payload capture runs for 24 h from now when KIOKU_HOOK_DUMP=1 or [client] hook_dump = true (marker {})",
        marker.display()
    );
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
    println!("  3. other machines: run `kioku invite` here and paste the line it prints there");
    println!(
        "     (manual: kioku setup --client-only http://<this-host>:{} <auth_token from {}>)",
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

/// Exit code of `kioku serve` for a data directory written by a newer kioku (EX_CONFIG).
pub const SCHEMA_EXIT_CODE: i32 = 78;

/// `kioku serve`; returns the exit code (75 after an automatic update, SPEC-M2.5 §3.1, or
/// a rollback, SPEC-M2.7 §7; 78 for a newer data directory, SPEC-M2.7 §5).
fn serve(
    bind: Option<String>,
    port: Option<u16>,
    log_file: Option<PathBuf>,
) -> anyhow::Result<i32> {
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
    let auto = cfg.update.auto;
    // Only a server started by the service manager updates itself (§3.1 step 5): a
    // foreground `kioku serve` would just exit.
    let managed = std::env::var(crate::service::SERVICE_MARKER_ENV).is_ok_and(|v| v == "1");
    let state_dir = data_dir.join("state");
    let exe = std::env::current_exe().context("locating the kioku binary")?;
    let exe = kioku_core::util::canonical_plain(&exe).unwrap_or(exe);
    // SPEC-M2.7 §7: count this start before anything can fail; after repeated failed starts
    // of this version, go back to the previous binary.
    if managed
        && let Some(version) = crate::auto_update::boot_check(&exe, &state_dir, kioku_core::VERSION)
    {
        let msg = format!(
            "kioku: rolled back to v{version} after {} failed starts",
            crate::auto_update::BOOT_FAILURE_LIMIT
        );
        tracing::error!("{msg}");
        eprintln!("{msg}");
        return Ok(crate::service::UPDATE_EXIT_CODE);
    }
    let (base, mirror_warning) = match crate::update::release_base(&cfg) {
        Ok((b, w)) => (Some(b), w),
        Err(e) => (
            None,
            Some(format!("kioku: warning: {e:#}; automatic updates are off")),
        ),
    };
    let update_interval = cfg.update.interval();
    let store = match Store::open(cfg) {
        Ok(s) => Arc::new(s),
        // SPEC-M2.7 §5: an older binary never touches a newer data directory.
        Err(kioku_core::Error::Internal(e))
            if e.downcast_ref::<kioku_core::NewerSchema>().is_some() =>
        {
            // Not this binary's fault: a rollback would not help (SPEC-M2.7 §5, §7).
            if managed {
                crate::auto_update::boot_succeeded(&state_dir, kioku_core::VERSION);
            }
            tracing::error!("{e:#}");
            eprintln!("kioku: error: {e:#}");
            return Ok(SCHEMA_EXIT_CODE);
        }
        Err(e) => {
            let locked = matches!(&e, kioku_core::Error::Internal(i)
                if i.downcast_ref::<kioku_core::store::DataDirLocked>().is_some());
            if managed && locked {
                crate::auto_update::boot_succeeded(&state_dir, kioku_core::VERSION);
            }
            return Err(anyhow::Error::from(e).context("opening the data directory"));
        }
    };
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
    let update = kioku_server::UpdateStatus::shared_for(&store);
    update.lock().managed = managed;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let opts = kioku_server::ServeOptions {
        update: update.clone(),
        shutdown: Some(rx.clone()),
    };
    if let Some(w) = &mirror_warning {
        tracing::warn!("{w}");
    }
    let boot_dir = state_dir.clone();
    let prune_store = store.clone();
    let retention_auto = store.config().retention.auto;
    runtime.block_on(async move {
        // SPEC-M2.8 §3: the daily retention run, on the update check's schedule.
        if retention_auto {
            tokio::spawn(crate::auto_update::prune_task(
                prune_store,
                crate::auto_update::FIRST_PRUNE,
            ));
        }
        if managed {
            // SPEC-M2.7 §7: a start that survives a minute is a good one.
            let dir = state_dir.clone();
            tokio::spawn(async move {
                tokio::time::sleep(crate::auto_update::BOOT_OK_AFTER).await;
                let _ = tokio::task::spawn_blocking(move || {
                    crate::auto_update::boot_succeeded(&dir, kioku_core::VERSION)
                })
                .await;
            });
        }
        match (managed, base) {
            (true, Some(base)) => {
                let check = crate::auto_update::ServerCheck {
                    base,
                    exe,
                    current: kioku_core::VERSION.to_string(),
                    auto,
                    verify: crate::update::Verify::automatic(),
                    state_dir: Some(state_dir),
                };
                tokio::spawn(crate::auto_update::server_update_task(
                    check,
                    update,
                    tx,
                    crate::auto_update::FIRST_CHECK,
                    update_interval,
                ));
            }
            // No update task: nothing will request a shutdown.
            _ => std::mem::drop(tx),
        }
        kioku_server::serve_with(store, bind, port, opts).await
    })?;
    // A graceful stop (service restart, signal, update) is not a failed start (SPEC-M2.7
    // §7): without this, a few quick restarts in a row would look like a crash loop.
    if managed {
        crate::auto_update::boot_succeeded(&boot_dir, kioku_core::VERSION);
    }
    if *rx.borrow() {
        tracing::info!(
            "exiting with {} so the service manager starts the new binary",
            crate::service::UPDATE_EXIT_CODE
        );
        return Ok(crate::service::UPDATE_EXIT_CODE);
    }
    Ok(0)
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

/// `kioku search --since / --kind / --path-prefix` (SPEC-M3.1 §2–§3).
#[derive(Clone, Debug, Default)]
struct SearchFilters {
    since: Option<String>,
    kinds: Vec<String>,
    path_prefix: Option<String>,
}

fn search(
    query: &str,
    project: Option<String>,
    scope: Option<ScopeArg>,
    limit: Option<usize>,
    filters: SearchFilters,
) -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let options = kioku_core::SearchOptions::parse(filters.since.as_deref(), &filters.kinds)
        .map_err(|e| anyhow::anyhow!(e))?;
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
    if let Some(since) = options.since {
        params.push(("since", since.format("%Y-%m-%d").to_string()));
    }
    if !options.kinds.is_empty() {
        params.push(("kinds", options.kinds.join(",")));
    }
    if let Some(prefix) = &filters.path_prefix {
        params.push(("path_prefix", prefix.clone()));
    }
    let body = client.get(&["search"], &params)?;
    if let Some(prefix) = &filters.path_prefix {
        let Some(sessions) = body.get("sessions") else {
            anyhow::bail!("{}", crate::bridge::PATH_PREFIX_UNSUPPORTED);
        };
        let sessions: Vec<kioku_core::PathSession> =
            serde_json::from_value(sessions.clone()).context("unexpected search response")?;
        println!(
            "{}",
            kioku_server::mcp::path_sessions_body(prefix, &sessions)
        );
        if query.trim().is_empty() {
            return Ok(());
        }
        println!();
    }
    let hits: Vec<Hit> = serde_json::from_value(body.get("hits").cloned().unwrap_or(Value::Null))
        .context("unexpected search response")?;
    print!("{}", format_hits(&hits));
    Ok(())
}

/// Human-readable hit list (same shape as the `kioku_query` MCP tool):
/// `N. <path> — <title> (<kind>, YYYY-MM-DD[, @machine]) [global]` (SPEC-M3.0 §5,
/// SPEC-M3.1 §2), after a partial-match note when the hits only matched by characters.
pub fn format_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no hits\n".to_string();
    }
    let mut out = String::new();
    if hits.iter().any(|h| h.partial) {
        out.push_str(kioku_server::mcp::PARTIAL_MATCH_NOTE);
        out.push('\n');
    }
    for (i, h) in hits.iter().enumerate() {
        let global = if h.global { " [global]" } else { "" };
        out.push_str(&format!(
            "{}. {} — {} ({}){global}\n",
            i + 1,
            h.path,
            h.title,
            kioku_server::mcp::hit_meta(h)
        ));
        if !h.snippet.trim().is_empty() {
            out.push_str(&format!("   {}\n", kioku_core::util::one_line(&h.snippet)));
        }
    }
    out
}

fn status() -> anyhow::Result<()> {
    let (cfg, client) = command_client()?;
    let body = client.get(&["status"], &[])?;
    let version = body
        .get("version")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("?")
        .to_string();
    let s: StatusReport =
        serde_json::from_value(body.clone()).context("unexpected status response")?;
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
    if !s.aliases.is_empty() {
        println!(
            "aliases      : {}",
            kioku_server::mcp::format_aliases(&s.aliases)
        );
    }
    if let Some(st) = &s.storage {
        print!("{}", format_storage(st));
    }
    if let Some(u) = body.get("update") {
        println!("update       : {}", format_update_status(u));
    }
    Ok(())
}

/// The disk usage lines of `kioku status` (SPEC-M2.8 §3).
pub fn format_storage(st: &kioku_core::store::StorageReport) -> String {
    format!(
        "storage      : db {}, raw {}, wiki {}, backups {}, index {}\n\
         oldest raw   : {}\n\
         last prune   : {}\n",
        human_bytes(st.db_bytes),
        human_bytes(st.raw_bytes),
        human_bytes(st.wiki_bytes),
        human_bytes(st.backups_bytes),
        human_bytes(st.index_bytes),
        st.oldest_raw.as_deref().unwrap_or("-"),
        st.last_prune.as_deref().unwrap_or("never"),
    )
}

/// One line for the server's `update` status block (SPEC-M2.5 §3.4).
pub fn format_update_status(u: &Value) -> String {
    let s = |k: &str| u.get(k).and_then(Value::as_str);
    let auto = u.get("auto").and_then(Value::as_bool).unwrap_or(true);
    let managed = u.get("managed").and_then(Value::as_bool).unwrap_or(false);
    let mut out = match (auto, managed) {
        (true, true) => "automatic".to_string(),
        (false, _) => "automatic updates off".to_string(),
        (true, false) => "automatic (inactive: not running under `kioku service`)".to_string(),
    };
    if let Some(tag) = s("latest_seen") {
        out.push_str(&format!(", latest release {tag}"));
    }
    if let Some(t) = s("last_check") {
        out.push_str(&format!(", last check {t}"));
    }
    if let Some(e) = s("last_error") {
        out.push_str(&format!(", last error: {e}"));
    }
    out
}

/// `kioku prune [--dry-run]` → `POST /api/v1/prune` (SPEC-M2.8 §3).
fn prune(dry_run: bool) -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let body = client.post(&["prune"], &serde_json::json!({"dry_run": dry_run}))?;
    let report: kioku_core::store::PruneReport =
        serde_json::from_value(body).context("unexpected prune response")?;
    print!("{}", format_prune(&report));
    Ok(())
}

/// Bytes as `12.3 MiB` / `456 KiB` / `78 B`.
pub fn human_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let f = n as f64;
    if f >= KIB * KIB * KIB {
        format!("{:.1} GiB", f / (KIB * KIB * KIB))
    } else if f >= KIB * KIB {
        format!("{:.1} MiB", f / (KIB * KIB))
    } else if f >= KIB {
        format!("{:.0} KiB", f / KIB)
    } else {
        format!("{n} B")
    }
}

/// Human-readable `kioku prune` report.
pub fn format_prune(r: &kioku_core::store::PruneReport) -> String {
    let mut out = format!(
        "{} (on the server, [retention] policy):\n",
        if r.dry_run {
            "would prune (dry run: nothing changed)"
        } else {
            "pruned"
        }
    );
    let line = |label: &str, c: &kioku_core::store::PruneCount, what: &str| {
        format!(
            "  {label:<19}: {} ({} {what})\n",
            c.count,
            human_bytes(c.bytes)
        )
    };
    out.push_str(&line(
        "raw logs gzipped",
        &r.raw_gzipped,
        if r.dry_run { "to compress" } else { "saved" },
    ));
    out.push_str(&line("raw logs deleted", &r.raw_deleted, "freed"));
    out.push_str(&format!(
        "  {:<19}: {} ({} observations, {} of payloads)\n",
        "sessions reduced",
        r.sessions_reduced.count,
        r.observations_reduced,
        human_bytes(r.sessions_reduced.bytes)
    ));
    out.push_str(&line("backups removed", &r.backups_removed, "freed"));
    out.push_str(&line("hook dumps removed", &r.hook_dumps_removed, "freed"));
    out
}

/// `kioku forget --session <id> | --project <id>` → `POST /api/v1/forget` (SPEC-M2.8 §3).
/// A project is forgotten only after a confirmation (or `--yes`).
fn forget(
    session: Option<String>,
    project: Option<String>,
    purge_history: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let target = match (&session, &project) {
        (Some(s), None) => serde_json::json!({"session": s}),
        (None, Some(p)) => serde_json::json!({"project": p}),
        _ => anyhow::bail!("give exactly one of --session or --project"),
    };
    if project.is_some() && !yes {
        let mut dry = target.clone();
        dry["dry_run"] = serde_json::json!(true);
        let report: kioku_core::store::ForgetReport =
            serde_json::from_value(client.post(&["forget"], &dry)?)
                .context("unexpected forget response")?;
        print!("{}", format_forget(&report));
        let stdin = std::io::stdin();
        if !std::io::IsTerminal::is_terminal(&stdin) {
            anyhow::bail!("not forgetting project {} without --yes", report.project);
        }
        print!(
            "Forget project {} and everything above? This cannot be undone. [y/N] ",
            report.project
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        stdin.read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing changed");
            return Ok(());
        }
    }
    let report: kioku_core::store::ForgetReport =
        serde_json::from_value(client.post(&["forget"], &target)?)
            .context("unexpected forget response")?;
    print!("{}", format_forget(&report));
    if purge_history {
        println!(
            "The pages are gone from the wiki, but its git history still has them. To remove them from the history, on the server machine run:"
        );
        for c in kioku_core::store::purge_history_commands(&report.wiki_dir, &report.pages) {
            println!("  {c}");
        }
    }
    Ok(())
}

/// Human-readable `kioku forget` report.
pub fn format_forget(r: &kioku_core::store::ForgetReport) -> String {
    let mut out = format!(
        "{} in project {}: {} session(s), {} observation(s), {} receipt(s), {} handoff(s), {} page(s), {} raw log(s)\n",
        if r.dry_run { "would forget" } else { "forgot" },
        r.project,
        r.sessions.len(),
        r.observations,
        r.receipts,
        r.handoffs,
        r.pages.len(),
        r.raw_files.len()
    );
    for p in &r.pages {
        out.push_str(&format!("  {p}\n"));
    }
    out
}

fn reindex() -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let body = client.post(&["reindex"], &Value::Object(Default::default()))?;
    let docs = body.get("docs").and_then(Value::as_u64).unwrap_or(0);
    println!("reindexed {docs} pages");
    Ok(())
}

/// `kioku project merge <from> <into> [--dry-run]` → `POST /projects/merge`.
fn project_merge(from: &str, into: &str, dry_run: bool) -> anyhow::Result<()> {
    let (_, client) = command_client()?;
    let body = client.post(
        &["projects", "merge"],
        &serde_json::json!({"from": from, "into": into, "dry_run": dry_run}),
    )?;
    let report: MergeReport =
        serde_json::from_value(body).context("unexpected projects/merge response")?;
    print!("{}", format_merge(&report));
    Ok(())
}

/// Human-readable `kioku project merge` report.
pub fn format_merge(r: &MergeReport) -> String {
    if r.already_merged {
        return format!(
            "{} is already merged into {} (it is an alias); nothing to do\n",
            r.from, r.into
        );
    }
    let mut out = format!(
        "{} {} into {}: {} sessions, {} observations, {} handoffs, {} pages\n",
        if r.dry_run { "would merge" } else { "merged" },
        r.from,
        r.into,
        r.sessions,
        r.observations,
        r.handoffs,
        r.pages.len()
    );
    for (old, new) in &r.pages {
        out.push_str(&format!("  {old} → {new}\n"));
    }
    if r.dry_run {
        out.push_str("(dry run: nothing changed)\n");
    } else {
        out.push_str(&format!(
            "{} now resolves to {} everywhere (alias)\n",
            r.from, r.into
        ));
    }
    out
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
    // Plain `C:\…` on Windows, not `\\?\C:\…`: this path is written into hook configs.
    let exe = kioku_core::util::canonical_plain(&exe).unwrap_or(exe);
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

/// Runs `f` with the process's [`crate::setup::SetupEnv`] and prints its report; returns
/// the exit code (`invite` / `join`).
fn with_setup_env(f: impl FnOnce(&crate::setup::SetupEnv) -> crate::invite::CommandReport) -> i32 {
    let env = match current_binary().and_then(crate::setup::SetupEnv::from_process) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            return 1;
        }
    };
    let report = f(&env);
    print!("{}", report.stdout);
    eprint!("{}", report.stderr);
    report.exit_code
}

/// `kioku rotate-token [--dry-run] [--show-token]` (M2 §21, SPEC-M2.7 §10); returns the
/// exit code.
fn rotate_token(dry_run: bool, show_token: bool) -> i32 {
    let env = match current_binary().and_then(crate::setup::SetupEnv::from_process) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("kioku: error: {err:#}");
            return 1;
        }
    };
    let report = crate::rotate::run_rotate(dry_run, show_token, &env);
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
        ServiceCommand::Install { daemon: true } => {
            let (plist, lines) = manager.daemon_definition()?;
            print!("{plist}");
            for l in lines {
                eprintln!("{l}");
            }
        }
        ServiceCommand::Install { daemon: false } | ServiceCommand::Start => {
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
            let act = if matches!(command, ServiceCommand::Install { .. }) {
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
    fn merge_report_text() {
        let r = MergeReport {
            from: "kioku-71002b89".into(),
            into: "ai-agents-shared-memory-02036d30".into(),
            dry_run: true,
            sessions: 3,
            observations: 40,
            handoffs: 2,
            pages: vec![(
                "kioku-71002b89/pages/設計.md".into(),
                "ai-agents-shared-memory-02036d30/pages/設計.md".into(),
            )],
            ..MergeReport::default()
        };
        let out = format_merge(&r);
        assert!(out.starts_with(
            "would merge kioku-71002b89 into ai-agents-shared-memory-02036d30: 3 sessions, 40 observations, 2 handoffs, 1 pages\n"
        ));
        assert!(out.contains(
            "  kioku-71002b89/pages/設計.md → ai-agents-shared-memory-02036d30/pages/設計.md\n"
        ));
        assert!(out.ends_with("(dry run: nothing changed)\n"));
        let done = format_merge(&MergeReport {
            dry_run: false,
            ..r.clone()
        });
        assert!(done.starts_with("merged "));
        assert!(
            done.ends_with("now resolves to ai-agents-shared-memory-02036d30 everywhere (alias)\n")
        );
        let again = format_merge(&MergeReport {
            already_merged: true,
            ..r
        });
        assert!(again.contains("already merged"));
    }

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
            machine: None,
            partial: false,
        }];
        assert_eq!(
            format_hits(&hits),
            "1. _global/rust.md — Rust の書き方 (page, 2026-09-25) [global]\n   【引き継ぎ】を 自動化\n"
        );
        // a hit without a date (older server shape) shows its kind only
        let mut old = hits[0].clone();
        old.updated = String::new();
        old.kind = "session".into();
        assert!(
            format_hits(&[old.clone()])
                .starts_with("1. _global/rust.md — Rust の書き方 (session) [global]\n")
        );
        // SPEC-M3.1 §2: the machine of a session, and the partial-match note
        let session = Hit {
            machine: Some("mini".into()),
            partial: true,
            updated: "2026-10-01T09:00:00Z".into(),
            path: "p/sessions/2026-10-01-abc.md".into(),
            global: false,
            ..old
        };
        assert_eq!(
            format_hits(&[session]),
            "（部分一致）/ (partial match)\n1. p/sessions/2026-10-01-abc.md — Rust の書き方 (session, 2026-10-01, @mini)\n   【引き継ぎ】を 自動化\n"
        );
    }
}
