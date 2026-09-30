//! SPEC-M2.5 §6 CLI e2e: the real `kioku hook session-start` binary — a copy in a tempdir,
//! with HOME / KIOKU_DATA_DIR in tempdirs — against a fake server that reports a newer
//! version. It starts exactly one detached background updater (observed through the state
//! file and the fake release server, which has no assets, so nothing is ever replaced), a
//! second SessionStart within 6 h starts none, and a winget-path client prints the notice
//! line (Japanese by default, English with `lang = "en"`) instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "auto-update-token-0123456789";
const SERVER_VERSION: &str = "99.0.0";

struct Fake {
    base: String,
    downloads: Arc<AtomicUsize>,
}

/// A kioku stand-in: `POST /api/v1/sessions/start` answers with `server_version` 99.0.0;
/// `/releases/*` is always 404 and counts the download attempts.
fn fake_server() -> Fake {
    use axum::http::{StatusCode, Uri};
    use axum::response::IntoResponse;
    let downloads = Arc::new(AtomicUsize::new(0));
    let counter = downloads.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = axum::Router::new()
                .route(
                    "/api/v1/sessions/start",
                    axum::routing::post(|| async {
                        axum::Json(json!({
                            "project_id": "auto-update-proj",
                            "pending_handoff": null,
                            "state_excerpt": null,
                            "recent_sessions": [],
                            "server_version": SERVER_VERSION,
                        }))
                    }),
                )
                .fallback(move |uri: Uri| {
                    let counter = counter.clone();
                    async move {
                        if uri.path().starts_with("/releases/download/") {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        StatusCode::NOT_FOUND.into_response()
                    }
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    Fake {
        base: format!("http://{}", rx.recv().unwrap()),
        downloads,
    }
}

/// A copy of the built kioku binary at `dir/<rel>/kioku[.exe]` (never the real one).
fn copy_binary(dir: &Path, rel: &str) -> PathBuf {
    let bin_dir = dir.join(rel);
    std::fs::create_dir_all(&bin_dir).unwrap();
    let name = if cfg!(windows) { "kioku.exe" } else { "kioku" };
    let bin = bin_dir.join(name);
    std::fs::copy(env!("CARGO_BIN_EXE_kioku"), &bin).unwrap();
    bin
}

/// Runs `bin hook session-start --agent claude-code` with its own HOME and data dir.
fn session_start(bin: &Path, home: &Path, fake: &Fake, sid: &str) -> String {
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(".kioku.toml"),
        "project = \"auto-update-proj\"\nname = \"auto\"\n",
    )
    .unwrap();
    let mut cmd = Command::new(bin);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .args(["hook", "session-start", "--agent", "claude-code"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIOKU_DATA_DIR", home.join(".kioku"))
        .env("KIOKU_SERVER_URL", &fake.base)
        .env("KIOKU_AUTH_TOKEN", TOKEN)
        .env("KIOKU_DOWNLOAD_BASE", format!("{}/releases", fake.base))
        .current_dir(&project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = json!({
        "session_id": sid,
        "transcript_path": "",
        "cwd": project.display().to_string(),
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    String::from_utf8(out.stdout).unwrap()
}

fn state(home: &Path) -> Value {
    std::fs::read_to_string(home.join(".kioku/state/auto-update.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Waits until the background updater recorded its (failed: no assets) result.
fn wait_for_error(home: &Path) -> Value {
    let start = Instant::now();
    loop {
        let st = state(home);
        if st["last_error"].is_string() && !home.join(".kioku/state/auto-update.lock").exists() {
            return st;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the background updater never finished: {st}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn session_start_spawns_one_background_updater() {
    let fake = fake_server();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    let bin = copy_binary(tmp.path(), "bin");
    let original = std::fs::read(&bin).unwrap();

    let out = session_start(&bin, &home, &fake, "au-1");
    assert!(out.contains("<kioku>"), "{out}");
    assert!(
        !out.contains("99.0.0"),
        "no notice when updating automatically: {out}"
    );
    let st = wait_for_error(&home);
    assert_eq!(st["target"], "v99.0.0", "{st}");
    assert!(st["last_attempt"].is_string(), "{st}");
    assert!(
        st["last_error"].as_str().unwrap().contains("has no"),
        "{st}"
    );
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    let log = std::fs::read_to_string(home.join(".kioku/logs/update.log")).unwrap();
    assert!(log.contains("auto-update failed"), "{log}");

    // Within 6 h: no second attempt.
    let first_attempt = st["last_attempt"].clone();
    let out = session_start(&bin, &home, &fake, "au-2");
    assert!(out.contains("<kioku>"), "{out}");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(state(&home)["last_attempt"], first_attempt);
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        original,
        "nothing was replaced"
    );
}

#[test]
fn winget_client_gets_a_notice_once_a_day() {
    let fake = fake_server();
    let tmp = tempfile::tempdir().unwrap();
    let bin = copy_binary(
        tmp.path(),
        "WinGet/Packages/misorafa.kioku_Microsoft.Winget.Source_8wekyb3d8bbwe",
    );

    // Japanese (the default language).
    let home = tmp.path().join("home-ja");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    let out = session_start(&bin, &home, &fake, "wg-1");
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        out.contains(&format!(
            "kioku v99.0.0 が利用できます（この端末は v{version}）: winget upgrade misorafa.kioku\n</kioku>"
        )),
        "{out}"
    );
    assert!(state(&home)["last_notice"].is_string());
    // Once per day per machine.
    let out = session_start(&bin, &home, &fake, "wg-2");
    assert!(out.contains("<kioku>") && !out.contains("99.0.0"), "{out}");

    // English.
    let home = tmp.path().join("home-en");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    std::fs::write(home.join(".kioku/config.toml"), "[client]\nlang = \"en\"\n").unwrap();
    let out = session_start(&bin, &home, &fake, "wg-3");
    assert!(
        out.contains(&format!(
            "kioku v99.0.0 is available (you have v{version}): winget upgrade misorafa.kioku"
        )),
        "{out}"
    );

    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fake.downloads.load(Ordering::SeqCst),
        0,
        "winget: no download"
    );
}

#[test]
fn auto_off_gets_the_kioku_update_notice() {
    let fake = fake_server();
    let tmp = tempfile::tempdir().unwrap();
    let bin = copy_binary(tmp.path(), "bin");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    std::fs::write(
        home.join(".kioku/config.toml"),
        "[client]\nlang = \"en\"\n\n[update]\nauto = false\n",
    )
    .unwrap();
    let out = session_start(&bin, &home, &fake, "off-1");
    assert!(
        out.contains("is available") && out.contains(": kioku update\n"),
        "{out}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
    assert!(state(&home)["last_attempt"].is_null());
}
