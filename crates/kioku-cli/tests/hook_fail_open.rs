//! SPEC-M2.7 §1: `kioku hook …` is fail-open even when its own arguments do not parse — the
//! real binary exits 0 with empty stdout and stderr and logs one line to hook.log. Other
//! commands keep clap's usage errors.

use std::process::{Command, Stdio};

fn kioku(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIOKU_DATA_DIR", home.join(".kioku"))
        // Nothing listens here: no hook may reach a real server.
        .env("KIOKU_SERVER_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn hook_argument_errors_exit_0_silently_and_are_logged() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".kioku")).unwrap();
    let log = home.path().join(".kioku").join("logs").join("hook.log");
    let cases: [&[&str]; 3] = [
        &["hook", "stop", "--agent", "bogus"],
        &["hook", "nonsense"],
        &[
            "hook",
            "session-start",
            "--agent",
            "claude-code",
            "--unknown-flag",
        ],
    ];
    for (i, args) in cases.iter().enumerate() {
        let out = kioku(home.path(), args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {:?}", out.stdout);
        assert!(out.stderr.is_empty(), "{args:?}: {:?}", out.stderr);
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), i + 1, "{text}");
        assert!(lines[i].contains("hook agent=unknown"), "{text}");
    }
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("bogus") && text.contains("nonsense"),
        "{text}"
    );

    // Any other command still fails loudly on bad arguments.
    let out = kioku(home.path(), &["search"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!out.stderr.is_empty());
    // And `hook --help` is help, not an error.
    let out = kioku(home.path(), &["hook", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!out.stdout.is_empty());
}
