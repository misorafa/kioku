//! Service tests (M2 §16.7): golden plist / unit (incl. a HOME with a space), escaping, and
//! the command lists of install / start / stop / status / uninstall through a recording
//! runner — nothing here runs launchctl or systemctl.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

fn spec(home: &str) -> ServiceSpec {
    ServiceSpec {
        bin: format!("{home}/.local/bin/kioku"),
        data_dir: PathBuf::from(format!("{home}/.kioku")),
    }
}

const SPEC_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.kioku.serve</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/.local/bin/kioku</string>
    <string>serve</string>
    <string>--log-file</string><string>/Users/me/.kioku/logs/serve.log</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>KIOKU_DATA_DIR</key><string>/Users/me/.kioku</string>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>RUST_LOG</key><string>info,tantivy=warn</string>
  </dict>
  <key>WorkingDirectory</key><string>/Users/me/.kioku</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>/Users/me/.kioku/logs/serve.stderr.log</string>
  <key>StandardErrorPath</key><string>/Users/me/.kioku/logs/serve.stderr.log</string>
</dict>
</plist>
"#;

const SPEC_UNIT: &str = "[Unit]
Description=kioku shared memory server for AI coding agents
After=network.target

[Service]
Type=simple
ExecStart=/home/me/.local/bin/kioku serve --log-file /home/me/.kioku/logs/serve.log
Environment=KIOKU_DATA_DIR=/home/me/.kioku
Environment=RUST_LOG=info,tantivy=warn
WorkingDirectory=/home/me/.kioku
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
";

#[test]
fn plist_and_unit_match_the_spec_exactly() {
    assert_eq!(render_plist(&spec("/Users/me")), SPEC_PLIST);
    assert_eq!(render_unit(&spec("/home/me")), SPEC_UNIT);
}

#[test]
fn golden_definitions_for_a_home_with_a_space() {
    let plist = render_plist(&spec("/Users/Jane Doe"));
    let expected = SPEC_PLIST.replace("/Users/me/", "/Users/Jane Doe/");
    assert_eq!(plist, expected);

    let unit = render_unit(&spec("/home/Jane Doe"));
    assert_eq!(
        unit,
        "[Unit]
Description=kioku shared memory server for AI coding agents
After=network.target

[Service]
Type=simple
ExecStart=\"/home/Jane Doe/.local/bin/kioku\" serve --log-file \"/home/Jane Doe/.kioku/logs/serve.log\"
Environment=\"KIOKU_DATA_DIR=/home/Jane Doe/.kioku\"
Environment=RUST_LOG=info,tantivy=warn
WorkingDirectory=/home/Jane Doe/.kioku
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    );
    // Every path in both definitions is absolute: no `~`, no `$HOME`.
    for text in [&plist, &unit] {
        assert!(!text.contains('~') && !text.contains("$HOME"), "{text}");
    }
}

#[test]
fn xml_and_systemd_escaping() {
    assert_eq!(
        xml_escape(r#"a&b<c>"d"'e'"#),
        "a&amp;b&lt;c&gt;&quot;d&quot;&apos;e&apos;"
    );
    let odd = ServiceSpec {
        bin: "/opt/R&D <tools>/kioku".into(),
        data_dir: PathBuf::from("/data/it's \"here\""),
    };
    let plist = render_plist(&odd);
    assert!(plist.contains("<string>/opt/R&amp;D &lt;tools&gt;/kioku</string>"));
    assert!(
        plist.contains(
            "<key>WorkingDirectory</key><string>/data/it&apos;s &quot;here&quot;</string>"
        )
    );
    assert!(!plist.contains("R&D"));

    assert_eq!(systemd_quote_arg("serve"), "serve");
    assert_eq!(systemd_quote_arg("/a b/kioku"), "\"/a b/kioku\"");
    assert_eq!(systemd_quote_arg("/a\"b\\c"), "\"/a\\\"b\\\\c\"");
    assert_eq!(systemd_quote_arg("/100%/$x"), "/100%%/$$x");
    let unit = render_unit(&ServiceSpec {
        bin: "/home/u/bin/kioku".into(),
        data_dir: PathBuf::from("/home/u/100% kioku"),
    });
    assert!(unit.contains("Environment=\"KIOKU_DATA_DIR=/home/u/100%% kioku\""));
    assert!(unit.contains("WorkingDirectory=/home/u/100%% kioku\n"));
    assert!(unit.contains("--log-file \"/home/u/100%% kioku/logs/serve.log\""));
}

fn argv(calls: &[Vec<String>]) -> Vec<String> {
    calls.iter().map(|c| c.join(" ")).collect()
}

/// A fake systemd: `is-active` follows `enable --now` / `stop`, lingering starts off.
fn fake_systemd() -> (Runner, Arc<AtomicBool>) {
    let active = Arc::new(AtomicBool::new(false));
    let linger = Arc::new(AtomicBool::new(false));
    let a = active.clone();
    let runner = Runner::recording(move |argv| {
        let cmd = argv.join(" ");
        match cmd.as_str() {
            "systemctl --user is-active kioku.service" => {
                if a.load(Ordering::SeqCst) {
                    CmdOutput::ok("active\n")
                } else {
                    CmdOutput::fail("inactive")
                }
            }
            "systemctl --user enable --now kioku.service"
            | "systemctl --user start kioku.service" => {
                a.store(true, Ordering::SeqCst);
                CmdOutput::ok("")
            }
            "systemctl --user stop kioku.service"
            | "systemctl --user disable --now kioku.service" => {
                a.store(false, Ordering::SeqCst);
                CmdOutput::ok("")
            }
            "systemctl --user show -p MainPID --value kioku.service" => CmdOutput::ok("4242\n"),
            "loginctl show-user me -p Linger" => CmdOutput::ok(if linger.load(Ordering::SeqCst) {
                "Linger=yes\n"
            } else {
                "Linger=no\n"
            }),
            "loginctl enable-linger" => {
                linger.store(true, Ordering::SeqCst);
                CmdOutput::ok("")
            }
            _ => CmdOutput::ok(""),
        }
    });
    (runner, active)
}

fn manager(home: &Path, platform: Platform, runner: Runner) -> ServiceManager {
    ServiceManager {
        platform,
        runner,
        home: home.to_path_buf(),
        config_home: home.join(".config"),
        spec: ServiceSpec {
            bin: home.join(".local/bin/kioku").display().to_string(),
            data_dir: home.join(".kioku"),
        },
        user: Some("me".into()),
    }
}

#[test]
fn systemd_install_is_idempotent_and_restarts_only_on_change() {
    let home = tempfile::tempdir().unwrap();
    let (runner, active) = fake_systemd();
    let m = manager(home.path(), Platform::Systemd, runner.clone());
    let unit = home.path().join(".config/systemd/user/kioku.service");
    assert_eq!(m.definition_path().unwrap(), unit);

    let first = m.install().unwrap();
    assert!(first.changed);
    assert_eq!(
        argv(&runner.calls()),
        [
            "systemctl --user is-active kioku.service",
            "systemctl --user daemon-reload",
            "systemctl --user enable --now kioku.service",
            "loginctl show-user me -p Linger",
            "loginctl enable-linger",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&unit).unwrap(),
        render_unit(&m.spec)
    );
    assert!(home.path().join(".kioku/logs").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    // Second install: only queries, no writes.
    runner.clear_calls();
    let before = std::fs::metadata(&unit).unwrap().modified().unwrap();
    let second = m.install().unwrap();
    assert!(!second.changed);
    assert_eq!(
        argv(&runner.calls()),
        [
            "systemctl --user is-active kioku.service",
            "loginctl show-user me -p Linger",
        ]
    );
    assert_eq!(
        std::fs::metadata(&unit).unwrap().modified().unwrap(),
        before
    );
    assert!(second.lines.iter().any(|l| l.contains("unchanged")));

    // Changed content while running → rewrite, reload, restart.
    runner.clear_calls();
    let mut moved = m.clone();
    moved.spec.bin = "/usr/local/bin/kioku".into();
    assert!(moved.install().unwrap().changed);
    assert_eq!(
        argv(&runner.calls()),
        [
            "systemctl --user is-active kioku.service",
            "systemctl --user daemon-reload",
            "systemctl --user enable --now kioku.service",
            "systemctl --user restart kioku.service",
            "loginctl show-user me -p Linger",
        ]
    );

    // status / stop / start / uninstall
    runner.clear_calls();
    let st = moved.state();
    assert_eq!(
        st,
        ServiceState {
            installed: true,
            active: true,
            pid: Some(4242),
            linger: Some(true)
        }
    );
    moved.stop().unwrap();
    assert!(!active.load(Ordering::SeqCst));
    moved.start().unwrap();
    assert!(active.load(Ordering::SeqCst));
    runner.clear_calls();
    let un = moved.uninstall().unwrap();
    assert!(un.changed && !unit.exists());
    assert_eq!(
        argv(&runner.calls()),
        [
            "systemctl --user disable --now kioku.service",
            "systemctl --user daemon-reload",
        ]
    );
    assert!(!moved.uninstall().unwrap().changed);
    assert!(moved.start().is_err(), "start needs an installed service");
}

#[test]
fn linger_failure_prints_the_sudo_hint_without_running_it() {
    let home = tempfile::tempdir().unwrap();
    let runner = Runner::recording(|argv| match argv.join(" ").as_str() {
        "loginctl show-user me -p Linger" => CmdOutput::ok("Linger=no\n"),
        "loginctl enable-linger" => CmdOutput::fail("Access denied"),
        _ => CmdOutput::ok(""),
    });
    let m = manager(home.path(), Platform::Systemd, runner.clone());
    let act = m.install().unwrap();
    assert!(
        act.lines
            .iter()
            .any(|l| l.ends_with("sudo loginctl enable-linger me")),
        "{:?}",
        act.lines
    );
    assert!(runner.calls().iter().all(|c| c[0] != "sudo"));
}

#[test]
fn launchd_commands() {
    let home = tempfile::tempdir().unwrap();
    let loaded = Arc::new(AtomicBool::new(false));
    let l = loaded.clone();
    let runner = Runner::recording(move |argv| match argv.join(" ").as_str() {
        "id -u" => CmdOutput::ok("501\n"),
        "launchctl print gui/501/dev.kioku.serve" => {
            if l.load(Ordering::SeqCst) {
                CmdOutput::ok("dev.kioku.serve = {\n\tstate = running\n\tpid = 777\n}\n")
            } else {
                CmdOutput::fail("Could not find service")
            }
        }
        s if s.starts_with("launchctl bootstrap") => {
            l.store(true, Ordering::SeqCst);
            CmdOutput::ok("")
        }
        "launchctl bootout gui/501/dev.kioku.serve" => {
            l.store(false, Ordering::SeqCst);
            CmdOutput::ok("")
        }
        _ => CmdOutput::ok(""),
    });
    let m = manager(home.path(), Platform::Launchd, runner.clone());
    let plist = home
        .path()
        .join("Library/LaunchAgents/dev.kioku.serve.plist");
    assert_eq!(m.definition_path().unwrap(), plist);
    assert!(m.install().unwrap().changed);
    assert_eq!(
        argv(&runner.calls()),
        [
            "id -u".to_string(),
            "launchctl print gui/501/dev.kioku.serve".into(),
            "launchctl bootout gui/501/dev.kioku.serve".into(),
            format!("launchctl bootstrap gui/501 {}", plist.display()),
            "launchctl enable gui/501/dev.kioku.serve".into(),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&plist).unwrap(),
        render_plist(&m.spec)
    );

    runner.clear_calls();
    assert!(!m.install().unwrap().changed, "second install is a no-op");
    assert_eq!(
        argv(&runner.calls()),
        ["id -u", "launchctl print gui/501/dev.kioku.serve"]
    );
    let st = m.state();
    assert!(st.installed && st.active);
    assert_eq!(st.pid, Some(777));
    assert_eq!(st.linger, None);

    runner.clear_calls();
    m.stop().unwrap();
    assert_eq!(
        argv(&runner.calls()),
        ["id -u", "launchctl bootout gui/501/dev.kioku.serve"]
    );
    m.uninstall().unwrap();
    assert!(!plist.exists());
}

#[test]
fn unsupported_platform_explains_the_alternatives() {
    let home = tempfile::tempdir().unwrap();
    let runner = Runner::recording(|argv| {
        if argv.join(" ") == "systemctl --user show-environment" {
            CmdOutput::fail("Failed to connect to bus")
        } else {
            CmdOutput::ok("")
        }
    });
    let m = ServiceManager::detect(
        runner,
        home.path().to_path_buf(),
        home.path().join(".config"),
        spec("/home/me"),
        None,
    );
    if cfg!(target_os = "macos") {
        assert_eq!(m.platform, Platform::Launchd);
        return;
    }
    assert!(matches!(m.platform, Platform::Unsupported(_)));
    let err = format!("{:#}", m.install().unwrap_err());
    assert!(err.contains("kioku serve --log-file /home/me/.kioku/logs/serve.log"));
    assert!(err.contains("nohup") && err.contains("Docker"));
    assert!(m.definition_path().is_none() && !m.is_installed());
    assert!(m.plan_install().is_empty());
}

#[test]
fn plan_lists_the_write_and_the_commands() {
    let home = tempfile::tempdir().unwrap();
    let (runner, _) = fake_systemd();
    let m = manager(home.path(), Platform::Systemd, runner.clone());
    let plan = m.plan_install();
    assert_eq!(
        plan[0],
        format!("write {}", m.definition_path().unwrap().display())
    );
    assert!(plan.contains(&"systemctl --user enable --now kioku.service".to_string()));
    assert!(runner.calls().is_empty(), "planning runs nothing");
    assert!(!m.definition_path().unwrap().exists());
}

#[test]
fn launchd_pid_and_tail() {
    assert_eq!(parse_launchd_pid("{\n\tpid = 12\n}"), Some(12));
    assert_eq!(parse_launchd_pid("state = waiting"), None);
    assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
    assert_eq!(last_lines("a\n", 5), "a\n");
    assert_eq!(last_lines("", 5), "");
}

#[test]
fn health_probe_tells_free_ports_from_foreign_listeners() {
    let cfg = |url: String| ClientConfig {
        server_url: url,
        ..ClientConfig::default()
    };
    // Free port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    assert!(matches!(
        probe_health(
            &cfg(format!("http://127.0.0.1:{port}")),
            Duration::from_millis(500)
        ),
        Health::Down(_)
    ));
    // A listener that never answers HTTP.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    assert!(matches!(
        probe_health(&cfg(url), Duration::from_millis(300)),
        Health::Foreign(_)
    ));
}
