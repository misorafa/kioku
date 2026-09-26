//! `kioku setup` and `kioku doctor` on a temp HOME (M2 §16.8, §16.9), against the real
//! server (`kioku_server::build_app`) on an ephemeral loopback port. Service commands go
//! through a recording runner: nothing here runs launchctl, systemctl or loginctl.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kioku_cli::doctor::{self, Check, DoctorEnv, Status};
use kioku_cli::event::Agent;
use kioku_cli::install::agents::{InstallCtx, InstallOptions, install_agent, install_all};
use kioku_cli::service::{CmdOutput, Platform, Runner, ServiceSpec, render_unit};
use kioku_cli::setup::{SetupEnv, SetupOptions, run_setup};
use kioku_core::{Config, Store};
use serde_json::{Value, json};

const TOKEN: &str = "setup-token-0123456789abcdef";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn spawn_server(store: Arc<Store>, token: String, addr: String) -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = kioku_server::build_app(store, token);
            let listener = tokio::net::TcpListener::bind(addr.as_str()).await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

/// A server on its own data dir with `TOKEN`.
fn test_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let addr = spawn_server(store, TOKEN.into(), "127.0.0.1:0".into());
    (format!("http://{addr}"), dir)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A temp HOME with Claude Code, Codex and Gemini CLI "installed" (Cursor optional) and an
/// executable `~/.local/bin/kioku`.
fn home_with_agents(cursor: bool) -> (tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    let mut dirs = vec![".claude", ".codex", ".gemini"];
    if cursor {
        dirs.push(".cursor");
    }
    for d in dirs {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    let bin = home.path().join(".local/bin/kioku");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (home, bin.display().to_string())
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn setup_env(home: &Path, bin: &str, vars: HashMap<String, String>, runner: Runner) -> SetupEnv {
    SetupEnv {
        vars,
        home: home.to_path_buf(),
        cwd: home.to_path_buf(),
        bin: bin.to_string(),
        runner,
        platform: Some(Platform::Systemd),
        request_timeout: Duration::from_secs(3),
        poll_interval: Duration::from_millis(50),
        poll_timeout: Duration::from_secs(10),
    }
}

/// sha256-free content snapshot of every file under `dir` (relative path → bytes), skipping
/// relative prefixes in `skip`.
fn snapshot(dir: &Path, skip: &[&str]) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, skip: &[&str], out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            if skip.iter().any(|s| rel.starts_with(s)) {
                continue;
            }
            if p.is_dir() {
                out.insert(rel.join(""), Vec::new());
                walk(root, &p, skip, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, skip, &mut out);
    out
}

fn binary_line(bin: &str) -> String {
    format!(
        "  !!  binary      {bin} (build/temp location: hooks break when it moves; install with install.sh)"
    )
}

fn agent_lines() -> Vec<String> {
    [
        "  ok  claude-code hooks ~/.claude/settings.json (6), MCP ~/.claude.json",
        "  ok  codex       hooks ~/.codex/hooks.json (6), MCP ~/.codex/config.toml, AGENTS.md",
        "  !!  codex       open Codex and run /hooks once to trust kioku's hooks",
        "  --  cursor      not detected (~/.cursor missing)",
        "  ok  gemini-cli  hooks + MCP ~/.gemini/settings.json, GEMINI.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn setup_without_service_installs_agents_and_is_idempotent() {
    let (base, _server) = test_server();
    let (home, bin) = home_with_agents(false);
    let config = home.path().join(".kioku/config.toml");
    let env = setup_env(
        home.path(),
        &bin,
        vars(&[("KIOKU_SERVER_URL", &base), ("KIOKU_AUTH_TOKEN", TOKEN)]),
        Runner::recording(|_| CmdOutput::ok("")),
    );
    let opts = SetupOptions {
        no_service: true,
        ..SetupOptions::default()
    };

    let first = run_setup(&opts, &env);
    let mut expected = vec![
        binary_line(&bin),
        format!(
            "  ok  config      {} (created, token from KIOKU_AUTH_TOKEN)",
            config.display()
        ),
        "  --  service     skipped (--no-service)".to_string(),
        format!("  ok  auth        token accepted by {base}"),
    ];
    expected.extend(agent_lines());
    assert_eq!(first.summary_lines(), expected, "{}", first.render());
    assert_eq!(first.exit_code(), 0);
    let text = first.render();
    assert!(text.starts_with(&format!("kioku setup (v{VERSION})\n")));
    assert!(text.contains("Restart running agents so they pick up the new hooks and MCP server.\nCheck any time with: kioku doctor\n"));
    assert!(text.is_ascii(), "the summary is ASCII only:\n{text}");
    assert!(!text.contains(TOKEN), "the token is never printed");
    assert!(
        env.runner.calls().is_empty(),
        "--no-service runs no command"
    );

    // The agents really point at the server with the token.
    let claude: Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join(".claude.json")).unwrap())
            .unwrap();
    assert_eq!(claude["mcpServers"]["kioku"]["url"], format!("{base}/mcp"));
    assert_eq!(
        claude["mcpServers"]["kioku"]["headers"]["Authorization"],
        format!("Bearer {TOKEN}")
    );
    let cfg_text = std::fs::read_to_string(&config).unwrap();
    assert!(cfg_text.contains("[server]") && cfg_text.contains(TOKEN));

    // Second run: no file changes, same summary except the config line.
    let before = snapshot(home.path(), &[]);
    let second = run_setup(&opts, &env);
    expected[1] = format!(
        "  ok  config      {} (existing, token kept)",
        config.display()
    );
    assert_eq!(second.summary_lines(), expected, "{}", second.render());
    assert_eq!(second.exit_code(), 0);
    assert_eq!(
        snapshot(home.path(), &[]),
        before,
        "second run changes no file"
    );

    // --print-client-command adds the laptop line (with the token) after the summary.
    let third = run_setup(
        &SetupOptions {
            print_client_command: true,
            ..opts.clone()
        },
        &env,
    );
    assert_eq!(third.summary_lines(), expected);
    let text = third.render();
    let line = text
        .lines()
        .find(|l| l.contains("sh -s -- --client-only"))
        .unwrap();
    assert!(line.starts_with(
        "  curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s -- --client-only http://"
    ));
    assert!(line.ends_with(&format!(":7391 {TOKEN}")), "{line}");
    assert!(text.contains("listens on 127.0.0.1 only"));
    assert_eq!(snapshot(home.path(), &[]), before);
}

#[test]
fn setup_dry_run_writes_nothing_and_prints_the_plan() {
    let (home, bin) = home_with_agents(true);
    let closed = format!("http://127.0.0.1:{}", free_port());
    let env = setup_env(
        home.path(),
        &bin,
        vars(&[("KIOKU_SERVER_URL", &closed)]),
        Runner::recording(|_| CmdOutput::ok("")),
    );
    let before = snapshot(home.path(), &[]);
    let r = run_setup(
        &SetupOptions {
            dry_run: true,
            bind: Some("0.0.0.0".into()),
            ..SetupOptions::default()
        },
        &env,
    );
    assert_eq!(
        snapshot(home.path(), &[]),
        before,
        "--dry-run writes nothing"
    );
    assert!(env.runner.calls().is_empty(), "--dry-run runs no command");
    assert_eq!(r.exit_code(), 0, "{}", r.render());
    let lines = r.summary_lines();
    let config = home.path().join(".kioku/config.toml");
    assert_eq!(
        lines[1],
        format!(
            "  ok  config      would create {} with a new token (kioku init, bind 0.0.0.0)",
            config.display()
        )
    );
    assert_eq!(
        lines[2],
        "  ok  service     would install systemd kioku.service"
    );
    assert_eq!(
        lines[3],
        "  --  auth        skipped (dry run: config.toml not written yet)"
    );
    assert!(
        lines[4].starts_with("  ok  claude-code would change: hooks ~/.claude/settings.json (6)")
    );
    assert!(lines.iter().any(|l| l.starts_with(
        "  ok  cursor      would change: hooks ~/.cursor/hooks.json (8), MCP ~/.cursor/mcp.json"
    )));
    let text = r.render();
    let unit = home.path().join(".config/systemd/user/kioku.service");
    assert!(text.contains(&format!("        write {}\n", unit.display())));
    assert!(text.contains("        systemctl --user enable --now kioku.service\n"));
    assert!(
        text.contains("would be installed"),
        "agent details listed:\n{text}"
    );
    assert!(text.ends_with("dry run: nothing was written\nCheck any time with: kioku doctor\n"));
}

#[test]
fn setup_client_only_checks_the_token_before_touching_anything() {
    let (base, _server) = test_server();
    let (home, bin) = home_with_agents(false);
    let env = setup_env(
        home.path(),
        &bin,
        HashMap::new(),
        Runner::recording(|_| CmdOutput::ok("")),
    );
    let before = snapshot(home.path(), &[]);
    let wrong = run_setup(
        &SetupOptions {
            client_only: Some((base.clone(), "wrong-token".into())),
            ..SetupOptions::default()
        },
        &env,
    );
    assert_eq!(wrong.exit_code(), 1);
    let lines = wrong.summary_lines();
    assert_eq!(lines.len(), 2, "stops after the config step: {lines:?}");
    assert!(lines[1].starts_with("  xx  config      "));
    assert!(lines[1].contains("HTTP 401"));
    assert!(!wrong.render().contains("wrong-token"));
    assert_eq!(snapshot(home.path(), &[]), before, "nothing written");

    let ok = run_setup(
        &SetupOptions {
            client_only: Some((base.clone(), TOKEN.into())),
            ..SetupOptions::default()
        },
        &env,
    );
    let config = home.path().join(".kioku/config.toml");
    let mut expected = vec![
        binary_line(&bin),
        format!(
            "  ok  config      {} (client-only -> {base})",
            config.display()
        ),
        format!("  ok  auth        token accepted by {base}"),
        format!("  --  service     client-only machine (server at {base})"),
    ];
    expected.extend(agent_lines());
    assert_eq!(ok.summary_lines(), expected, "{}", ok.render());
    assert_eq!(ok.exit_code(), 0);
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("[client]") && !text.contains("[server]"));
    assert!(env.runner.calls().is_empty());

    // Re-running plain `kioku setup` on the client machine keeps it client-only.
    let before = snapshot(home.path(), &[]);
    let again = run_setup(&SetupOptions::default(), &env);
    assert_eq!(again.exit_code(), 0, "{}", again.render());
    assert_eq!(
        again.summary_lines()[1],
        format!(
            "  ok  config      {} (existing, client-only -> {base})",
            config.display()
        )
    );
    assert_eq!(
        again.summary_lines()[2],
        format!("  --  service     client-only machine (server at {base})")
    );
    assert_eq!(snapshot(home.path(), &[]), before);
}

/// A fake `systemd --user`: `enable --now` "starts" the service by running the real server on
/// the configured port with the data dir and token setup wrote.
fn fake_systemd_with_server(home: PathBuf, port: u16) -> Runner {
    let active = Arc::new(AtomicBool::new(false));
    Runner::recording(move |argv| match argv.join(" ").as_str() {
        "systemctl --user is-active kioku.service" => {
            if active.load(Ordering::SeqCst) {
                CmdOutput::ok("active\n")
            } else {
                CmdOutput::fail("inactive")
            }
        }
        "systemctl --user enable --now kioku.service" => {
            if !active.swap(true, Ordering::SeqCst) {
                let data = home.join(".kioku");
                let cfg = Config::load_from_dir(&data, &HashMap::new()).unwrap();
                let token = cfg.server.auth_token.clone().unwrap();
                let store = Arc::new(Store::open(cfg).unwrap());
                spawn_server(store, token, format!("127.0.0.1:{port}"));
            }
            CmdOutput::ok("")
        }
        "systemctl --user show -p MainPID --value kioku.service" => CmdOutput::ok("4242\n"),
        "loginctl show-user me -p Linger" => CmdOutput::ok("Linger=yes\n"),
        "git --version" => CmdOutput::ok("git version 2.43.0\n"),
        "codex --version" => CmdOutput::fail("not found"),
        _ => CmdOutput::ok(""),
    })
}

#[test]
fn setup_installs_the_service_and_polls_health() {
    let (home, bin) = home_with_agents(false);
    let port = free_port();
    let runner = fake_systemd_with_server(home.path().to_path_buf(), port);
    let env = setup_env(
        home.path(),
        &bin,
        vars(&[("KIOKU_PORT", &port.to_string()), ("USER", "me")]),
        runner.clone(),
    );
    let opts = SetupOptions {
        agents: vec![Agent::ClaudeCode],
        ..SetupOptions::default()
    };
    let first = run_setup(&opts, &env);
    let config = home.path().join(".kioku/config.toml");
    let base = format!("http://127.0.0.1:{port}");
    let expected = vec![
        binary_line(&bin),
        format!(
            "  ok  config      {} (created, new token)",
            config.display()
        ),
        format!("  ok  service     systemd kioku.service running at {base} (v{VERSION})"),
        format!("  ok  auth        token accepted by {base}"),
        "  ok  claude-code hooks ~/.claude/settings.json (6), MCP ~/.claude.json".to_string(),
    ];
    assert_eq!(first.summary_lines(), expected, "{}", first.render());
    assert_eq!(first.exit_code(), 0);
    let calls: Vec<String> = runner.calls().iter().map(|c| c.join(" ")).collect();
    assert_eq!(
        calls,
        [
            "systemctl --user is-active kioku.service",
            "systemctl --user daemon-reload",
            "systemctl --user enable --now kioku.service",
            "loginctl show-user me -p Linger",
        ]
    );
    let unit = home.path().join(".config/systemd/user/kioku.service");
    assert_eq!(
        std::fs::read_to_string(&unit).unwrap(),
        render_unit(&ServiceSpec {
            bin: bin.clone(),
            data_dir: home.path().join(".kioku"),
        })
    );
    let token = Config::load_file(&config)
        .unwrap()
        .server
        .auth_token
        .unwrap();
    assert!(!first.render().contains(&token));

    // Second run: the service is installed and healthy → nothing is rewritten or restarted.
    runner.clear_calls();
    // The running server owns its SQLite files; everything setup writes is compared.
    let skip = [".kioku/db"];
    let before = snapshot(home.path(), &skip);
    let second = run_setup(&opts, &env);
    let mut expected2 = expected.clone();
    expected2[1] = format!(
        "  ok  config      {} (existing, token kept)",
        config.display()
    );
    assert_eq!(second.summary_lines(), expected2, "{}", second.render());
    assert_eq!(
        snapshot(home.path(), &skip),
        before,
        "second run changes no file"
    );
    let calls: Vec<String> = runner.calls().iter().map(|c| c.join(" ")).collect();
    assert_eq!(
        calls,
        [
            "systemctl --user is-active kioku.service",
            "loginctl show-user me -p Linger",
        ]
    );

    // Something that is not kioku on the port → the service step fails, agents still run.
    let (home2, bin2) = home_with_agents(false);
    let foreign = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let fport = foreign.local_addr().unwrap().port();
    let mut env2 = setup_env(
        home2.path(),
        &bin2,
        vars(&[("KIOKU_PORT", &fport.to_string())]),
        Runner::recording(|_| CmdOutput::ok("")),
    );
    env2.request_timeout = Duration::from_millis(300);
    let r = run_setup(&opts, &env2);
    assert_eq!(r.exit_code(), 1);
    let lines = r.summary_lines();
    assert!(lines[2].starts_with(&format!(
        "  xx  service     http://127.0.0.1:{fport} is in use by something that is not kioku"
    )));
    assert!(lines[4].starts_with("  ok  claude-code"), "{lines:?}");
    assert!(env2.runner.calls().is_empty());
}

// ---------------------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------------------

struct Fixture {
    home: tempfile::TempDir,
    bin: String,
    base: String,
    runner: Runner,
}

impl Fixture {
    fn env(&self) -> DoctorEnv {
        let path = self.home.path().join(".local/bin");
        DoctorEnv {
            vars: vars(&[("PATH", path.to_str().unwrap()), ("USER", "me")]),
            home: self.home.path().to_path_buf(),
            bin: self.bin.clone(),
            runner: self.runner.clone(),
            platform: Some(Platform::Systemd),
            timeout: Duration::from_secs(3),
        }
    }

    fn ctx(&self, bin: &str) -> InstallCtx {
        let cfg = Config::load_from_dir(&self.home.path().join(".kioku"), &HashMap::new()).unwrap();
        InstallCtx {
            home: self.home.path().to_path_buf(),
            codex_home: self.home.path().join(".codex"),
            cwd: self.home.path().to_path_buf(),
            bin: bin.to_string(),
            client: cfg.client,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".kioku/config.toml")
    }
}

/// A server machine: config + data dir, the server running on that data dir, the service
/// "installed" through the fake systemd, and every agent installed.
fn server_machine() -> Fixture {
    let (home, bin) = home_with_agents(true);
    let data = home.path().join(".kioku");
    let mut cfg = Config::for_data_dir(&data);
    cfg.server.auth_token = Some(TOKEN.into());
    cfg.client.auth_token = Some(TOKEN.into());
    kioku_core::init(&mut cfg).unwrap();
    let store = Arc::new(Store::open(cfg.clone()).unwrap());
    let addr = spawn_server(store, TOKEN.into(), "127.0.0.1:0".into());
    let base = format!("http://{addr}");
    cfg.client.server_url = base.clone();
    cfg.save().unwrap();

    let runner = Runner::recording(|argv| match argv.join(" ").as_str() {
        "systemctl --user is-active kioku.service" => CmdOutput::ok("active\n"),
        "systemctl --user show -p MainPID --value kioku.service" => CmdOutput::ok("4242\n"),
        "loginctl show-user me -p Linger" => CmdOutput::ok("Linger=yes\n"),
        "git --version" => CmdOutput::ok("git version 2.43.0\n"),
        _ => CmdOutput::fail("not found"),
    });
    let fx = Fixture {
        home,
        bin: bin.clone(),
        base,
        runner,
    };
    // The installed unit (the fake systemd reports it active).
    let senv = setup_env(
        fx.home.path(),
        &bin,
        vars(&[("USER", "me")]),
        fx.runner.clone(),
    );
    let manager = senv.service_manager(&data);
    let unit = manager.definition_path().unwrap();
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, manager.render().unwrap()).unwrap();

    for (agent, status) in install_all(&fx.ctx(&bin), &InstallOptions::default(), &[]) {
        assert!(
            matches!(status, kioku_cli::install::agents::AllStatus::Changed(_)),
            "{agent:?}: {status:?}"
        );
    }
    fx
}

fn find<'a>(checks: &'a [Check], id: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no check {id}: {checks:#?}"))
}

fn statuses(checks: &[Check]) -> BTreeMap<String, Status> {
    checks.iter().map(|c| (c.id.clone(), c.status)).collect()
}

#[test]
fn doctor_all_ok_then_warn_and_fail_scenarios() {
    let fx = server_machine();
    let checks = doctor::run_doctor(&fx.env(), None);
    let mut expected: BTreeMap<String, Status> = [
        "binary",
        "config",
        "data_dir",
        "git",
        "server",
        "auth",
        "index",
        "mcp",
        "service",
        "hook_log",
        "hook_dump",
        "agent.cursor.duplicate",
        "agent.codex.feature",
        "agent.codex.instructions",
        "agent.gemini-cli.enabled",
        "agent.gemini-cli.instructions",
    ]
    .iter()
    .map(|id| (id.to_string(), Status::Ok))
    .collect();
    for a in ["claude-code", "codex", "cursor", "gemini-cli"] {
        expected.insert(format!("agent.{a}.hooks"), Status::Ok);
        expected.insert(format!("agent.{a}.mcp"), Status::Ok);
    }
    // Expected warnings: the test binary lives in a temp dir; Codex trust is unverifiable.
    expected.insert("binary".into(), Status::Warn);
    expected.insert("agent.codex.trust".into(), Status::Warn);
    assert_eq!(
        statuses(&checks),
        expected,
        "{}",
        doctor::render_text(&checks)
    );
    assert_eq!(doctor::exit_code(&checks), 0);
    assert!(find(&checks, "mcp").message.contains("answered initialize"));
    assert!(find(&checks, "service").message.contains("pid 4242"));
    let text = doctor::render_text(&checks);
    assert!(text.contains("[ OK ] server: "));
    assert!(text.contains("[WARN] agent.codex.trust: "));
    assert!(text.contains("       fix: open Codex and run /hooks once to confirm\n"));
    assert!(!text.contains(TOKEN));

    // --json shape.
    let v = doctor::render_json(&checks);
    let list = v["checks"].as_array().unwrap();
    assert_eq!(list.len(), checks.len());
    for c in list {
        let obj = c.as_object().unwrap();
        assert!(
            obj.keys()
                .all(|k| ["id", "status", "message", "fix"].contains(&k.as_str()))
        );
        assert!(obj["id"].is_string() && obj["message"].is_string());
        assert!(["ok", "warn", "fail"].contains(&obj["status"].as_str().unwrap()));
        if let Some(f) = obj.get("fix") {
            assert!(f.is_string());
        }
    }
    assert!(!v.to_string().contains(TOKEN));
    assert_eq!(
        list.iter()
            .find(|c| c["id"] == "agent.codex.trust")
            .unwrap()["status"],
        "warn"
    );

    // --agent restricts the agent checks.
    let only = doctor::run_doctor(&fx.env(), Some(Agent::Codex));
    assert!(only.iter().any(|c| c.id == "agent.codex.hooks"));
    assert!(!only.iter().any(|c| c.id.starts_with("agent.claude-code")));
    assert!(!only.iter().any(|c| c.id == "agent.cursor.duplicate"));

    // Token mismatch in an agent's MCP entry → WARN, and no token text anywhere.
    let cursor_mcp = fx.home.path().join(".cursor/mcp.json");
    let original = std::fs::read_to_string(&cursor_mcp).unwrap();
    let mut v: Value = serde_json::from_str(&original).unwrap();
    v["mcpServers"]["kioku"]["headers"]["Authorization"] = json!("Bearer not-the-token-XYZ");
    std::fs::write(&cursor_mcp, v.to_string()).unwrap();
    let checks = doctor::run_doctor(&fx.env(), None);
    let c = find(&checks, "agent.cursor.mcp");
    assert_eq!(c.status, Status::Warn);
    assert!(c.message.contains("does not match"));
    let text = doctor::render_text(&checks) + &doctor::render_json(&checks).to_string();
    assert!(!text.contains(TOKEN) && !text.contains("not-the-token-XYZ"));
    assert_eq!(doctor::exit_code(&checks), 0);
    std::fs::write(&cursor_mcp, original).unwrap();

    // Old index version → WARN with the reindex hint.
    let version_file = fx.home.path().join(".kioku/index/schema-version");
    let original = std::fs::read_to_string(&version_file).unwrap();
    std::fs::write(&version_file, "1\n").unwrap();
    let checks = doctor::run_doctor(&fx.env(), None);
    let c = find(&checks, "index");
    assert_eq!(c.status, Status::Warn);
    assert!(c.message.contains("run `kioku reindex`"), "{}", c.message);
    std::fs::write(&version_file, original).unwrap();

    // Moved binary → FAIL agent.claude-code.hooks.
    install_agent(
        Agent::ClaudeCode,
        &fx.ctx("/nonexistent/old/kioku"),
        &InstallOptions::default(),
    )
    .unwrap();
    let checks = doctor::run_doctor(&fx.env(), None);
    let c = find(&checks, "agent.claude-code.hooks");
    assert_eq!(c.status, Status::Fail, "{c:?}");
    assert!(c.message.contains("/nonexistent/old/kioku"));
    assert_eq!(
        c.fix.as_deref(),
        Some("kioku install claude-code (re-registers this binary)")
    );
    assert_eq!(doctor::exit_code(&checks), 1);
    install_agent(
        Agent::ClaudeCode,
        &fx.ctx(&fx.bin),
        &InstallOptions::default(),
    )
    .unwrap();

    // Gemini hooks disabled globally → FAIL; Codex features.hooks = false → FAIL.
    let gemini = fx.home.path().join(".gemini/settings.json");
    let mut g: Value = serde_json::from_str(&std::fs::read_to_string(&gemini).unwrap()).unwrap();
    g["hooksConfig"] = json!({"enabled": false});
    std::fs::write(&gemini, serde_json::to_string_pretty(&g).unwrap()).unwrap();
    let codex = fx.home.path().join(".codex/config.toml");
    let mut t = std::fs::read_to_string(&codex).unwrap();
    t.push_str("\n[features]\nhooks = false\n");
    std::fs::write(&codex, t).unwrap();
    let checks = doctor::run_doctor(&fx.env(), None);
    assert_eq!(
        find(&checks, "agent.gemini-cli.enabled").status,
        Status::Fail
    );
    assert_eq!(find(&checks, "agent.codex.feature").status, Status::Fail);
    assert_eq!(find(&checks, "agent.claude-code.hooks").status, Status::Ok);

    // Server down → FAIL server; auth / index / mcp are not attempted.
    let mut cfg = Config::load_file(&fx.config_path()).unwrap();
    cfg.client.server_url = format!("http://127.0.0.1:{}", free_port());
    cfg.save().unwrap();
    let checks = doctor::run_doctor(&fx.env(), None);
    let c = find(&checks, "server");
    assert_eq!(c.status, Status::Fail);
    assert!(c.message.contains("unreachable"));
    assert!(
        !checks
            .iter()
            .any(|c| ["auth", "index", "mcp"].contains(&c.id.as_str()))
    );
    assert_eq!(doctor::exit_code(&checks), 1);
    assert!(!doctor::render_text(&checks).contains(TOKEN));
    let _ = &fx.base;
}

#[test]
fn doctor_without_config_fails() {
    let (home, bin) = home_with_agents(false);
    let env = DoctorEnv {
        vars: vars(&[(
            "KIOKU_SERVER_URL",
            &format!("http://127.0.0.1:{}", free_port()),
        )]),
        home: home.path().to_path_buf(),
        bin,
        runner: Runner::recording(|argv| {
            if argv[0] == "git" {
                CmdOutput::ok("git version 2.43.0\n")
            } else {
                CmdOutput::fail("")
            }
        }),
        platform: Some(Platform::Systemd),
        timeout: Duration::from_secs(1),
    };
    let checks = doctor::run_doctor(&env, None);
    let c = find(&checks, "config");
    assert_eq!(c.status, Status::Fail);
    assert!(c.fix.as_deref().unwrap().contains("kioku setup"));
    assert_eq!(find(&checks, "binary").status, Status::Warn);
    assert_eq!(find(&checks, "server").status, Status::Fail);
    // Not a server machine as far as doctor can tell: no data_dir / service checks.
    assert!(
        !checks
            .iter()
            .any(|c| c.id == "data_dir" || c.id == "service")
    );
    // Agents detected but not installed → WARN hooks, FAIL mcp.
    assert_eq!(
        find(&checks, "agent.claude-code.hooks").status,
        Status::Warn
    );
    assert_eq!(find(&checks, "agent.claude-code.mcp").status, Status::Fail);
    assert_eq!(doctor::exit_code(&checks), 1);
    let v = doctor::render_json(&checks);
    assert_eq!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "config")
            .unwrap()["status"],
        "fail"
    );
}

#[test]
fn doctor_on_a_client_only_machine_skips_data_dir_and_service() {
    let (base, _server) = test_server();
    let (home, bin) = home_with_agents(false);
    let env = setup_env(
        home.path(),
        &bin,
        HashMap::new(),
        Runner::recording(|_| CmdOutput::ok("")),
    );
    let r = run_setup(
        &SetupOptions {
            client_only: Some((base, TOKEN.into())),
            ..SetupOptions::default()
        },
        &env,
    );
    assert_eq!(r.exit_code(), 0, "{}", r.render());
    let denv = DoctorEnv {
        vars: HashMap::new(),
        home: home.path().to_path_buf(),
        bin,
        runner: Runner::recording(|argv| {
            if argv[0] == "git" {
                CmdOutput::ok("git version 2.43.0\n")
            } else {
                CmdOutput::fail("")
            }
        }),
        platform: Some(Platform::Systemd),
        timeout: Duration::from_secs(3),
    };
    let checks = doctor::run_doctor(&denv, None);
    assert!(
        !checks
            .iter()
            .any(|c| c.id == "data_dir" || c.id == "service")
    );
    for id in [
        "config",
        "server",
        "auth",
        "index",
        "mcp",
        "agent.claude-code.hooks",
        "agent.gemini-cli.mcp",
    ] {
        assert_eq!(find(&checks, id).status, Status::Ok, "{id}: {checks:#?}");
    }
    assert!(find(&checks, "config").message.contains("(client only)"));
    // `kioku` is not on PATH here (no PATH var) → binary WARN only.
    assert_eq!(find(&checks, "binary").status, Status::Warn);
    assert_eq!(doctor::exit_code(&checks), 0);
}
