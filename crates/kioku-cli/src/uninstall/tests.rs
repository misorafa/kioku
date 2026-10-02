//! `kioku uninstall` on fixture homes (SPEC-M3.3 §3): agent configs back to their
//! pre-install content, the service definition removed (through a recording runner —
//! nothing here runs launchctl or systemctl), the installer's PATH lines removed and
//! nothing else, the binary and `.prev` removed (a dummy file, never this test's
//! executable), config and data only on request with the confirmations.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::install::HookPlatform;
use crate::install::agents::{InstallOptions, install_all};
use crate::service::{CmdOutput, Platform, Runner};

const ZSHRC_BEFORE: &str = "alias ll='ls -l'\nexport PATH=\"$HOME/.local/bin:$PATH\"\nexport PATH=\"$HOME/other:$PATH\" # added by the kioku installer\n";
const KIOKU_LINE: &str = "export PATH=\"$HOME/.local/bin:$PATH\" # added by the kioku installer";

fn env_for(home: &Path, bin: &Path, vars: &[(&str, &str)]) -> SetupEnv {
    let mut v: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    v.insert("USER".into(), "me".into());
    SetupEnv {
        vars: v,
        home: home.to_path_buf(),
        cwd: home.to_path_buf(),
        bin: bin.display().to_string(),
        runner: Runner::recording(|_| CmdOutput::ok("")),
        platform: Some(Platform::Systemd),
        hook_platform: HookPlatform::Unix,
        request_timeout: Duration::from_secs(1),
        poll_interval: Duration::from_millis(10),
        poll_timeout: Duration::from_millis(50),
    }
}

/// A home with Claude Code, Codex and Cursor configs the user already had, the binary and
/// its `.prev` in ~/.local/bin, and a client config.
fn fixture_home() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    for d in [".claude", ".codex", ".cursor", ".kioku", ".local/bin"] {
        std::fs::create_dir_all(h.join(d)).unwrap();
    }
    std::fs::write(
        h.join(".claude/settings.json"),
        serde_json::to_string_pretty(&json!({
            "theme": "dark",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        h.join(".claude.json"),
        serde_json::to_string_pretty(&json!({
            "numStartups": 3,
            "mcpServers": {"other": {"type": "stdio", "command": "other-mcp"}}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        h.join(".codex/config.toml"),
        "model = \"gpt-5\"\n\n[projects.\"/w\"]\ntrust_level = \"trusted\"\n",
    )
    .unwrap();
    std::fs::write(
        h.join(".codex/AGENTS.md"),
        "# 自分のメモ\n\n日本語で答えて。\n",
    )
    .unwrap();
    std::fs::write(
        h.join(".kioku/config.toml"),
        "[client]\nserver_url = \"http://127.0.0.1:9\"\nauth_token = \"fixture-token\"\n",
    )
    .unwrap();
    let bin = h.join(".local/bin/kioku");
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::write(h.join(".local/bin/kioku.prev"), "#!/bin/sh\nexit 0\n").unwrap();
    (home, bin)
}

/// Every file under `dir` (relative path → content; JSON parsed so formatting is ignored).
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
                continue;
            }
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let text = match serde_json::from_str::<Value>(&text) {
                Ok(v) if rel.ends_with(".json") => v.to_string(),
                _ => text,
            };
            out.insert(rel, text);
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn run(opts: &UninstallOptions, env: &SetupEnv, answers: &str) -> (i32, String) {
    let mut input = Cursor::new(answers.as_bytes().to_vec());
    let mut out = Vec::new();
    let code = run_uninstall(opts, env, &mut input, &mut out);
    (code, String::from_utf8(out).unwrap())
}

/// The e2e of SPEC-M3.3 §3 (macOS / Linux): install everything, uninstall everything.
#[test]
fn uninstall_restores_agent_configs_and_removes_service_path_line_and_binary() {
    let (home, bin) = fixture_home();
    let h = home.path();
    let agent_files = [".claude", ".claude.json", ".codex", ".cursor"];
    let before: BTreeMap<String, String> = snapshot(h)
        .into_iter()
        .filter(|(k, _)| agent_files.iter().any(|a| k.starts_with(a)))
        .collect();
    std::fs::write(h.join(".zshrc"), format!("{ZSHRC_BEFORE}{KIOKU_LINE}\n")).unwrap();
    std::fs::create_dir_all(h.join(".config/fish/conf.d")).unwrap();
    std::fs::write(
        h.join(".config/fish/conf.d/kioku.fish"),
        "contains \"$HOME/.local/bin\" $PATH; or set -gx PATH \"$HOME/.local/bin\" $PATH # added by the kioku installer\n",
    )
    .unwrap();

    let env = env_for(h, &bin, &[]);
    let cfg = load_config(&env);
    let ctx = env.install_ctx(&cfg.client);
    for (agent, status) in install_all(&ctx, &InstallOptions::default(), &[]) {
        assert!(
            matches!(status, AllStatus::Changed(_))
                || agent == Agent::GeminiCli
                || agent == Agent::Antigravity,
            "{}: {status:?}",
            agent.as_str()
        );
    }
    let manager = env.service_manager(&cfg.data_dir);
    manager.install().unwrap();
    let unit = manager.definition_path().unwrap();
    assert!(unit.is_file());
    assert!(
        std::fs::read_to_string(h.join(".claude/settings.json"))
            .unwrap()
            .contains("hook")
    );
    env.runner.clear_calls();

    // The plan names every step, then --yes does it.
    let (code, out) = run(
        &UninstallOptions {
            yes: true,
            ..Default::default()
        },
        &env,
        "",
    );
    assert_eq!(code, 0, "{out}");
    for want in [
        "claude-code: remove kioku's entries",
        "codex: remove kioku's entries",
        "cursor: remove kioku's entries",
        &format!("service: stop it and remove {}", unit.display()),
        &format!(
            "PATH: remove `{KIOKU_LINE}` from {}",
            h.join(".zshrc").display()
        ),
        &format!(
            "binary: remove {} and {}",
            bin.display(),
            sibling(&bin, PREV_SUFFIX).display()
        ),
        "config.toml is kept (server URL and token); --everything removes it",
        "kioku uninstall: done.",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.contains("fixture-token"), "the token is never printed");

    // Agent configs: exactly what the user had before.
    let after: BTreeMap<String, String> = snapshot(h)
        .into_iter()
        .filter(|(k, _)| agent_files.iter().any(|a| k.starts_with(a)))
        .filter(|(k, _)| !k.ends_with(".kioku-bak"))
        .collect();
    assert_eq!(after, before);
    // Service: definition gone, disabled through the manager.
    assert!(!unit.exists());
    assert!(
        env.runner
            .calls()
            .iter()
            .any(|c| c.join(" ") == "systemctl --user disable --now kioku.service"),
        "{:?}",
        env.runner.calls()
    );
    // PATH: only the installer's exact line for this binary's directory.
    assert_eq!(
        std::fs::read_to_string(h.join(".zshrc")).unwrap(),
        ZSHRC_BEFORE
    );
    assert!(!h.join(".config/fish/conf.d/kioku.fish").exists());
    // Binary and .prev gone; config kept.
    assert!(!bin.exists() && !h.join(".local/bin/kioku.prev").exists());
    assert!(h.join(".kioku/config.toml").is_file());

    // A second run finds nothing to do.
    let (code, out) = run(&UninstallOptions::default(), &env, "");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nothing to remove"), "{out}");
}

#[test]
fn the_plan_is_printed_and_nothing_happens_without_a_yes() {
    let (home, bin) = fixture_home();
    let h = home.path();
    std::fs::write(h.join(".bashrc"), format!("{KIOKU_LINE}\n")).unwrap();
    let env = env_for(h, &bin, &[]);
    let before = snapshot(h);
    for (opts, answers) in [
        (
            UninstallOptions {
                dry_run: true,
                ..Default::default()
            },
            "",
        ),
        (UninstallOptions::default(), "n\n"),
        (UninstallOptions::default(), ""),
    ] {
        let (code, out) = run(&opts, &env, answers);
        assert!(out.contains("kioku uninstall will:"), "{out}");
        assert!(out.contains("PATH: remove"), "{out}");
        if opts.dry_run {
            assert_eq!(code, 0);
            assert!(out.contains("dry run: nothing was changed"));
        } else {
            assert_eq!(code, 1);
            assert!(out.contains("aborted: nothing was changed"), "{out}");
        }
        assert_eq!(snapshot(h), before, "{out}");
    }
    let (_, out) = run(&UninstallOptions::default(), &env, "");
    assert!(out.contains("re-run with --yes"), "{out}");
    let (code, out) = run(&UninstallOptions::default(), &env, "y\n");
    assert_eq!(code, 0, "{out}");
    assert!(!bin.exists());
    assert_eq!(std::fs::read_to_string(h.join(".bashrc")).unwrap(), "");
}

#[test]
fn everything_and_purge_data_need_their_flags_and_the_typed_word() {
    let (home, bin) = fixture_home();
    let h = home.path();
    let data = h.join(".kioku");
    std::fs::create_dir_all(data.join("wiki/_global")).unwrap();
    std::fs::write(data.join("wiki/_global/メモ.md"), "# 検索のメモ\n").unwrap();
    let env = env_for(h, &bin, &[]);

    // --everything: config.toml goes, the data stays.
    let p = plan(
        &UninstallOptions {
            everything: true,
            ..Default::default()
        },
        &env,
    );
    assert!(
        p.actions
            .contains(&Action::Config(data.join("config.toml")))
    );
    assert!(p.data_dir().is_none());

    // --purge-data: the backup hint first; a wrong word aborts everything.
    let purge = UninstallOptions {
        everything: true,
        purge_data: true,
        ..Default::default()
    };
    let (code, out) = run(&purge, &env, "y\ndelete\n");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("make a backup first: kioku backup"), "{out}");
    assert!(out.contains(&format!(
        "Type DELETE to remove {} permanently",
        data.display()
    )));
    assert!(data.join("wiki/_global/メモ.md").is_file() && bin.is_file());

    let (code, out) = run(&purge, &env, "y\nDELETE\n");
    assert_eq!(code, 0, "{out}");
    assert!(!data.exists() && !bin.exists());
}

#[test]
fn package_managed_and_build_binaries_are_left_alone() {
    let (home, _) = fixture_home();
    let h = home.path();
    let keg = h.join("opt/homebrew/Cellar/kioku/0.9.3/bin/kioku");
    std::fs::create_dir_all(keg.parent().unwrap()).unwrap();
    std::fs::write(&keg, "x").unwrap();
    let p = plan(&UninstallOptions::default(), &env_for(h, &keg, &[]));
    assert!(!p.actions.iter().any(|a| matches!(a, Action::Binary { .. })));
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("installed with Homebrew") && n.contains("brew uninstall kioku")),
        "{:?}",
        p.notes
    );
    let build = h.join("src/kioku/target/debug/kioku");
    std::fs::create_dir_all(build.parent().unwrap()).unwrap();
    std::fs::write(&build, "x").unwrap();
    let p = plan(&UninstallOptions::default(), &env_for(h, &build, &[]));
    assert!(!p.actions.iter().any(|a| matches!(a, Action::Binary { .. })));
    assert!(p.notes.iter().any(|n| n.contains("cargo build output")));
}

/// The Windows user PATH entry install.ps1 added (marker file next to kioku.exe), through
/// the `KIOKU_USER_PATH_FILE` stand-in; an entry kioku did not add stays.
#[test]
fn the_user_path_entry_is_removed_only_when_the_installer_added_it() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let dir = h.join("Programs/kioku");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("kioku.exe");
    std::fs::write(&exe, "x").unwrap();
    let path_file = h.join("user-path.txt");
    let d = dir.display().to_string();
    std::fs::write(&path_file, format!(r"C:\Windows\system32;{d};C:\Tools")).unwrap();
    let pf = path_file.display().to_string();
    let env = env_for(h, &exe, &[("KIOKU_USER_PATH_FILE", &pf)]);

    let p = plan(&UninstallOptions::default(), &env);
    assert!(
        !p.actions.contains(&Action::UserPath(d.clone())),
        "no marker: {p:?}"
    );

    std::fs::write(dir.join(WINDOWS_PATH_MARKER), &d).unwrap();
    let p = plan(&UninstallOptions::default(), &env);
    assert!(p.actions.contains(&Action::UserPath(d.clone())), "{p:?}");
    let (code, out) = run(
        &UninstallOptions {
            yes: true,
            ..Default::default()
        },
        &env,
        "",
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        std::fs::read_to_string(&path_file).unwrap(),
        r"C:\Windows\system32;C:\Tools"
    );
    assert!(!dir.join(WINDOWS_PATH_MARKER).exists() && !exe.exists());

    // The default install dir counts without a marker (installs before the marker).
    let local = h.join("AppData/Local");
    let default_dir = local.join("Programs/kioku");
    std::fs::create_dir_all(&default_dir).unwrap();
    let exe = default_dir.join("kioku.exe");
    std::fs::write(&exe, "x").unwrap();
    let dd = default_dir.display().to_string();
    std::fs::write(&path_file, format!(r"{dd}\;C:\Tools")).unwrap();
    let l = local.display().to_string();
    let env = env_for(
        h,
        &exe,
        &[("KIOKU_USER_PATH_FILE", &pf), ("LOCALAPPDATA", &l)],
    );
    let p = plan(&UninstallOptions::default(), &env);
    assert!(p.actions.contains(&Action::UserPath(dd)), "{p:?}");
}

#[test]
fn line_and_entry_helpers() {
    let home = Path::new("/home/u");
    assert_eq!(
        installer_path_lines(Path::new("/home/u/.local/bin"), home)[0],
        KIOKU_LINE
    );
    assert!(
        installer_path_lines(Path::new("/opt/kioku"), home).contains(
            &"export PATH=\"/opt/kioku:$PATH\" # added by the kioku installer".to_string()
        )
    );
    let (text, n) = remove_lines(
        &format!("a\r\n{KIOKU_LINE}\r\nb"),
        &[KIOKU_LINE.to_string()],
    );
    assert_eq!((text.as_str(), n), ("a\r\nb", 1));
    assert_eq!(
        without_entry(r"C:\A;c:\users\me\kioku\;C:\B", r"C:\Users\me\kioku"),
        (r"C:\A;C:\B".to_string(), true)
    );
    assert_eq!(
        without_entry(r"C:\A", r"C:\B"),
        (r"C:\A".to_string(), false)
    );
    assert!(is_build_output(Path::new("/w/kioku/target/release/kioku")));
    assert!(!is_build_output(Path::new("/home/u/.local/bin/kioku")));
}
