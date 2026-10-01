//! Automatic-update tests (SPEC-M2.5 §6): the client decision (newer / equal / older
//! server, pre-release, throttle, notice), the state file and lock, and the server task
//! against a local fake release server with a dummy binary in a tempdir — never the test's
//! own executable.

use super::*;
use crate::update::fixture;

fn facts(server: Option<&str>) -> ClientFacts {
    ClientFacts {
        server_version: server.map(str::to_string),
        client_version: "0.6.5".into(),
        auto: true,
        winget: false,
        writable: true,
        lang: Lang::En,
    }
}

/// RFC 3339 time `secs` seconds ago.
fn ago(secs: u64) -> Option<String> {
    Some(kioku_core::util::fmt_ts(now() - Duration::from_secs(secs)))
}

#[test]
fn server_newer_equal_older_and_prereleases() {
    let at = unix_now();
    let st = AutoUpdateState::default();
    assert_eq!(
        client_action(&facts(Some("0.7.0")), &st, at),
        ClientAction::Spawn("v0.7.0".into())
    );
    assert_eq!(
        client_action(&facts(Some("v0.7.0")), &st, at),
        ClientAction::Spawn("v0.7.0".into())
    );
    assert_eq!(
        client_action(&facts(Some("0.6.5")), &st, at),
        ClientAction::Nothing
    );
    // Never downgrade: a client newer than its server does nothing.
    assert_eq!(
        client_action(&facts(Some("0.6.4")), &st, at),
        ClientAction::Nothing
    );
    // Pre-releases are never followed, and an older server says nothing.
    assert_eq!(
        client_action(&facts(Some("0.7.0-rc1")), &st, at),
        ClientAction::Nothing
    );
    assert_eq!(client_action(&facts(None), &st, at), ClientAction::Nothing);
    assert!(is_stable("v0.7.0") && !is_stable("v0.7.0-beta.1"));
}

#[test]
fn one_attempt_per_target_per_six_hours() {
    let at = unix_now();
    let recent = AutoUpdateState {
        target: Some("v0.7.0".into()),
        last_attempt: ago(3600),
        ..AutoUpdateState::default()
    };
    assert_eq!(
        client_action(&facts(Some("0.7.0")), &recent, at),
        ClientAction::Nothing
    );
    // Another target is not throttled by this attempt.
    assert_eq!(
        client_action(&facts(Some("0.7.1")), &recent, at),
        ClientAction::Spawn("v0.7.1".into())
    );
    let old = AutoUpdateState {
        last_attempt: ago(6 * 3600 + 60),
        ..recent
    };
    assert_eq!(
        client_action(&facts(Some("0.7.0")), &old, at),
        ClientAction::Spawn("v0.7.0".into())
    );
}

#[test]
fn notice_instead_of_update_once_a_day() {
    let at = unix_now();
    let st = AutoUpdateState::default();
    let off = ClientFacts {
        auto: false,
        ..facts(Some("0.7.0"))
    };
    assert_eq!(
        client_action(&off, &st, at),
        ClientAction::Notice("kioku v0.7.0 is available (you have v0.6.5): kioku update".into())
    );
    let winget = ClientFacts {
        winget: true,
        lang: Lang::Ja,
        ..facts(Some("0.7.0"))
    };
    assert_eq!(
        client_action(&winget, &st, at),
        ClientAction::Notice(
            "kioku v0.7.0 が利用できます（この端末は v0.6.5）: winget upgrade misorafa.kioku"
                .into()
        )
    );
    let readonly = ClientFacts {
        writable: false,
        ..facts(Some("0.7.0"))
    };
    let ClientAction::Notice(line) = client_action(&readonly, &st, at) else {
        panic!("a read-only binary directory gets the installer line");
    };
    assert!(
        line.contains("--version v0.7.0") || line.contains("-Version v0.7.0"),
        "{line}"
    );
    // Shown at most once per day.
    let shown = AutoUpdateState {
        last_notice: ago(3600),
        ..AutoUpdateState::default()
    };
    assert_eq!(client_action(&off, &shown, at), ClientAction::Nothing);
    let yesterday = AutoUpdateState {
        last_notice: ago(25 * 3600),
        ..AutoUpdateState::default()
    };
    assert!(matches!(
        client_action(&off, &yesterday, at),
        ClientAction::Notice(_)
    ));
}

#[test]
fn notice_goes_inside_the_block() {
    assert_eq!(
        with_notice("<kioku>\n引き継ぎ\n</kioku>\n", "kioku v9 が利用できます"),
        "<kioku>\n引き継ぎ\nkioku v9 が利用できます\n</kioku>\n"
    );
    assert_eq!(with_notice("x\n", "n"), "x\nn\n");
}

#[test]
fn state_file_round_trip_and_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    assert_eq!(AutoUpdateState::load(&dir), AutoUpdateState::default());
    AutoUpdateState::update(&dir, |s| {
        s.target = Some("v0.7.0".into());
        s.last_error = Some("チェックサム不一致".into());
    });
    let st = AutoUpdateState::load(&dir);
    assert_eq!(st.target.as_deref(), Some("v0.7.0"));
    assert_eq!(st.last_error.as_deref(), Some("チェックサム不一致"));
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(STATE_FILE)).unwrap()).unwrap();
    for key in ["target", "last_attempt", "last_error"] {
        assert!(raw.get(key).is_some(), "{raw}");
    }

    let lock = UpdateLock::acquire(&dir).unwrap().expect("free lock");
    assert!(UpdateLock::acquire(&dir).unwrap().is_none(), "held");
    drop(lock);
    assert!(!dir.join(LOCK_FILE).exists());
    let again = UpdateLock::acquire(&dir).unwrap();
    assert!(again.is_some());
    // A stale lock (a crashed updater) is taken over.
    std::mem::forget(again);
    let f = std::fs::File::options()
        .write(true)
        .open(dir.join(LOCK_FILE))
        .unwrap();
    f.set_modified(SystemTime::now() - LOCK_STALE - Duration::from_secs(1))
        .unwrap();
    drop(f);
    assert!(UpdateLock::acquire(&dir).unwrap().is_some());
}

#[test]
fn check_interval_has_bounded_jitter() {
    for _ in 0..50 {
        let d = next_interval();
        assert!(d >= CHECK_INTERVAL - CHECK_JITTER && d <= CHECK_INTERVAL + CHECK_JITTER);
    }
}

fn status() -> SharedUpdateStatus {
    std::sync::Arc::new(parking_lot::Mutex::new(kioku_server::UpdateStatus {
        auto: true,
        managed: true,
        ..Default::default()
    }))
}

fn server_check(base: &str, dir: &Path, auto: bool) -> ServerCheck {
    let exe = dir.join("bin").join(crate::update::BIN_NAME);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "old binary").unwrap();
    ServerCheck {
        base: base.to_string(),
        exe,
        current: "0.6.5".into(),
        auto,
        verify: Verify::unsigned(),
        state_dir: Some(dir.join("state")),
    }
}

/// Same tag → nothing; `auto = false` → no download, `latest_seen` set. Cross-platform:
/// neither downloads anything.
#[test]
fn server_same_tag_and_auto_off_download_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fixture::serve(Vec::new(), Some("v0.6.5"));
    let check = server_check(&base, tmp.path(), true);
    let st = status();
    assert_eq!(server_check_once(&check, &st), ServerOutcome::UpToDate);
    let s = st.lock().clone();
    assert_eq!(s.latest_seen.as_deref(), Some("v0.6.5"));
    assert!(s.last_check.is_some() && s.last_error.is_none());
    assert_eq!(std::fs::read_to_string(&check.exe).unwrap(), "old binary");

    // A newer release, but no assets at all: with auto off nothing is fetched.
    let base = fixture::serve(Vec::new(), Some("v0.7.0"));
    let check = server_check(&base, tmp.path(), false);
    let st = status();
    assert_eq!(
        server_check_once(&check, &st),
        ServerOutcome::Available("v0.7.0".into())
    );
    let s = st.lock().clone();
    assert_eq!(s.latest_seen.as_deref(), Some("v0.7.0"));
    assert!(!s.auto);
    assert!(s.last_error.is_none(), "{s:?}");
    assert_eq!(std::fs::read_to_string(&check.exe).unwrap(), "old binary");

    // Pre-release tags are ignored.
    let base = fixture::serve(Vec::new(), Some("v0.8.0-rc1"));
    let check = server_check(&base, tmp.path(), true);
    assert_eq!(
        server_check_once(&check, &status()),
        ServerOutcome::UpToDate
    );

    // Unreachable releases: an error is recorded, the binary is untouched.
    let check = server_check("http://127.0.0.1:9/releases", tmp.path(), true);
    let st = status();
    assert!(matches!(
        server_check_once(&check, &st),
        ServerOutcome::Failed(_)
    ));
    assert!(st.lock().last_error.is_some());
    assert_eq!(std::fs::read_to_string(&check.exe).unwrap(), "old binary");
}

/// Checksum mismatch → nothing changes, the error is recorded (status and state file).
#[cfg(unix)]
#[test]
fn server_checksum_mismatch_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (asset, bytes) = fixture::release("v0.7.0", "0.7.0");
    let base = fixture::serve(
        vec![
            (asset, bytes.clone()),
            fixture::sums("v0.7.0", &bytes, false),
        ],
        Some("v0.7.0"),
    );
    let check = server_check(&base, tmp.path(), true);
    let st = status();
    let ServerOutcome::Failed(err) = server_check_once(&check, &st) else {
        panic!("a checksum mismatch fails");
    };
    assert!(err.contains("checksum mismatch"), "{err}");
    assert!(
        st.lock()
            .last_error
            .as_deref()
            .unwrap()
            .contains("checksum mismatch")
    );
    assert_eq!(std::fs::read_to_string(&check.exe).unwrap(), "old binary");
    let state = AutoUpdateState::load(&tmp.path().join("state"));
    assert_eq!(state.target.as_deref(), Some("v0.7.0"));
    assert!(state.last_error.unwrap().contains("checksum mismatch"));
}

/// Newer tag → the dummy binary is swapped and the task requests the exit (75).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn server_task_swaps_the_binary_and_requests_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let (asset, bytes) = fixture::release("v0.7.0", "0.7.0");
    let base = fixture::serve(
        vec![
            (asset, bytes.clone()),
            fixture::sums("v0.7.0", &bytes, true),
        ],
        Some("v0.7.0"),
    );
    let check = server_check(&base, tmp.path(), true);
    let exe = check.exe.clone();
    let st = status();
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(server_update_task(
        check,
        st.clone(),
        tx,
        Duration::from_millis(10),
    ));
    tokio::time::timeout(Duration::from_secs(30), rx.wait_for(|v| *v))
        .await
        .expect("the task requests shutdown")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        std::fs::read_to_string(&exe)
            .unwrap()
            .contains("kioku 0.7.0")
    );
    let s = st.lock().clone();
    assert_eq!(s.latest_seen.as_deref(), Some("v0.7.0"));
    assert!(s.last_error.is_none(), "{s:?}");
    let state = AutoUpdateState::load(&tmp.path().join("state"));
    assert!(
        state
            .last_result
            .unwrap()
            .starts_with("auto-updated v0.6.5 -> v0.7.0 (server)")
    );
    assert_eq!(crate::service::UPDATE_EXIT_CODE, 75);
    // SPEC-M2.7 §7: the replaced binary is kept for a rollback.
    assert_eq!(
        std::fs::read_to_string(crate::update::sibling(&exe, crate::update::PREV_SUFFIX)).unwrap(),
        "old binary"
    );
}

/// SPEC-M2.7 §7: the boot-failure state machine.
#[test]
fn boot_failures_count_per_version_and_trigger_a_rollback() {
    let now = "2026-10-01T00:00:00Z";
    let run = |b: Option<&BootFailures>, v: &str| match boot_decision(b, v, now) {
        BootDecision::Run(b) => b,
        BootDecision::RollBack => panic!("unexpected rollback"),
    };
    let first = run(None, "0.9.0");
    assert_eq!((first.version.as_str(), first.count), ("0.9.0", 1));
    let second = run(Some(&first), "0.9.0");
    let third = run(Some(&second), "0.9.0");
    assert_eq!(third.count, 3);
    assert_eq!(
        boot_decision(Some(&third), "0.9.0", now),
        BootDecision::RollBack
    );
    // Another version starts counting afresh.
    assert_eq!(run(Some(&third), "0.8.0").count, 1);
}

/// SPEC-M2.7 §7 end to end with dummy "binaries" (shell scripts, never the test's own
/// exe): the new version fails every start; after three failed starts the fourth puts the
/// previous binary back, which then runs and is counted as healthy.
#[cfg(unix)]
#[test]
fn three_failed_starts_roll_the_server_back() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let exe = tmp.path().join("kioku");
    let prev = crate::update::sibling(&exe, crate::update::PREV_SUFFIX);
    let script = |version: &str, serve_exit: u8| {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'kioku {version}' ;;\n  serve) exit {serve_exit} ;;\nesac\n"
        )
    };
    for (path, body) in [(&exe, script("0.9.0", 1)), (&prev, script("0.8.0", 0))] {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // The service manager's loop: each start runs boot_check, then serves (or dies).
    let mut failed = 0;
    let rolled_back = loop {
        let running = crate::update::binary_version(&exe).unwrap();
        if let Some(v) = boot_check(&exe, &state, &running) {
            break v;
        }
        let ok = std::process::Command::new(&exe)
            .arg("serve")
            .status()
            .unwrap()
            .success();
        assert!(!ok, "the new version fails");
        failed += 1;
        assert!(
            failed <= BOOT_FAILURE_LIMIT,
            "no rollback after {failed} failures"
        );
    };
    assert_eq!(failed, BOOT_FAILURE_LIMIT);
    assert_eq!(rolled_back, "0.8.0");
    assert_eq!(
        crate::update::binary_version(&exe).as_deref(),
        Some("0.8.0")
    );
    let st = AutoUpdateState::load(&state);
    assert!(
        st.last_error
            .as_deref()
            .unwrap()
            .contains("rolled back to v0.8.0 after 3 failed starts"),
        "{st:?}"
    );
    assert_eq!(st.boot_failures, None);
    // The old binary starts, serves a minute, and is healthy.
    assert_eq!(boot_check(&exe, &state, "0.8.0"), None);
    assert!(
        std::process::Command::new(&exe)
            .arg("serve")
            .status()
            .unwrap()
            .success()
    );
    boot_succeeded(&state, "0.8.0");
    assert_eq!(AutoUpdateState::load(&state).boot_failures, None);
    // Without an older .prev there is nothing to roll back to: it keeps counting.
    for _ in 0..4 {
        assert_eq!(boot_check(&exe, &state, "0.8.0"), None);
    }
    assert_eq!(
        AutoUpdateState::load(&state).boot_failures.unwrap().count,
        4
    );
}
