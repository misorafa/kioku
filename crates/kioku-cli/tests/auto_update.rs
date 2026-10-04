//! SPEC-M2.5 §6 CLI e2e: the real `kioku hook session-start` binary — a copy in a tempdir,
//! with HOME / KIOKU_DATA_DIR in tempdirs — against a fake server that reports a newer
//! version. It starts exactly one detached background updater (observed through the state
//! file and the fake release server, which has no assets, so nothing is ever replaced), a
//! second SessionStart within 6 h starts none, and a winget-path client prints the notice
//! line (Japanese by default, English with `lang = "en"`) instead.
//!
//! SPEC-M3.4 §1: the same for the `kioku mcp` stdio bridge (desktop apps run no hooks) —
//! the check runs once per bridge process after its first successful tool call, a notice
//! goes into the next query result, and each successful call leaves an `mcp` liveness entry.

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
    /// `GET /api/v1/status` requests seen.
    status_calls: Arc<AtomicUsize>,
}

/// [`fake_server_at`] reporting 99.0.0.
fn fake_server() -> Fake {
    fake_server_at(SERVER_VERSION)
}

/// A kioku stand-in: `POST /api/v1/sessions/start` answers with `server_version` =
/// `version`, `GET /api/v1/status` with `version` (counted), `GET /api/v1/search` with no
/// hits; `/releases/*` is always 404 and counts the download attempts; everything else is
/// 404.
fn fake_server_at(version: &'static str) -> Fake {
    use axum::http::{StatusCode, Uri};
    use axum::response::IntoResponse;
    let downloads = Arc::new(AtomicUsize::new(0));
    let counter = downloads.clone();
    let status_calls = Arc::new(AtomicUsize::new(0));
    let status_counter = status_calls.clone();
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
                    axum::routing::post(move || async move {
                        axum::Json(json!({
                            "project_id": "auto-update-proj",
                            "pending_handoff": null,
                            "state_excerpt": null,
                            "recent_sessions": [],
                            "server_version": version,
                        }))
                    }),
                )
                .route(
                    "/api/v1/status",
                    axum::routing::get(move || {
                        status_counter.fetch_add(1, Ordering::SeqCst);
                        async move {
                            axum::Json(json!({
                                "data_dir": "/srv/kioku", "projects": 1, "pages": 0,
                                "sessions": 0, "observations": 0, "handoffs": 0,
                                "index_docs": 0, "git_enabled": false, "version": version,
                                "project_ids": ["auto-update-proj"],
                            }))
                        }
                    }),
                )
                .route(
                    "/api/v1/search",
                    axum::routing::get(|| async { axum::Json(json!({"hits": []})) }),
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
        status_calls,
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

/// `bin <args>` with its own HOME and data dir, pointed at `fake` (no inherited `KIOKU_*`).
fn kioku(bin: &Path, home: &Path, fake: &Fake, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIOKU_DATA_DIR", home.join(".kioku"))
        .env("KIOKU_SERVER_URL", &fake.base)
        .env("KIOKU_AUTH_TOKEN", TOKEN)
        .env("KIOKU_DOWNLOAD_BASE", format!("{}/releases", fake.base));
    cmd
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
    let mut child = kioku(
        bin,
        home,
        fake,
        &["hook", "session-start", "--agent", "claude-code"],
    )
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
    // SPEC-M2.7 §11: KIOKU_DOWNLOAD_BASE counts only with allow_mirror.
    std::fs::write(
        home.join(".kioku/config.toml"),
        "[update]\nallow_mirror = true\n",
    )
    .unwrap();
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

// ---------------------------------------------------------------------------------------
// SPEC-M3.4 §1: the `kioku mcp` bridge follows the server too
// ---------------------------------------------------------------------------------------

/// A `kioku mcp` process (a copied binary) driven over its stdio.
struct Bridge {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    next_id: u64,
}

impl Bridge {
    /// Starts `bin mcp` and completes the MCP handshake.
    fn start(bin: &Path, home: &Path, fake: &Fake) -> Bridge {
        use std::io::BufRead;
        let mut child = kioku(bin, home, fake, &["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut bridge = Bridge {
            stdin: child.stdin.take().unwrap(),
            lines: std::io::BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            next_id: 1,
        };
        let init = bridge.request(
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "kioku", "{init}");
        bridge.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        bridge
    }

    fn send(&mut self, v: Value) {
        use std::io::Write;
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// One JSON-RPC request; returns its response (other messages are skipped).
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self.lines.next().expect("bridge closed stdout").unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// `tools/call`: (is_error, text).
    fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let result = &r["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (result["isError"] == json!(true), text)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn liveness(home: &Path) -> Value {
    std::fs::read_to_string(home.join(".kioku/state/last-hook.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Waits until the bridge's background task has recorded a successful call.
fn wait_for_liveness(home: &Path) -> Value {
    let start = Instant::now();
    loop {
        let marks = liveness(home);
        if marks["mcp"].is_object() {
            return marks;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "no mcp liveness entry: {marks}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A newer server: a failing first call checks nothing; the first successful call starts
/// exactly one detached updater (and leaves the `mcp` liveness entry); later calls of the
/// same process do not check again.
#[test]
fn bridge_follows_a_newer_server_once() {
    let fake = fake_server();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    std::fs::write(
        home.join(".kioku/config.toml"),
        "[update]\nallow_mirror = true\n",
    )
    .unwrap();
    let bin = copy_binary(tmp.path(), "bin");
    let original = std::fs::read(&bin).unwrap();
    let mut bridge = Bridge::start(&bin, &home, &fake);

    // a failing first call defers the check
    let (is_error, out) = bridge.call("kioku_read", json!({"path": "nope/none.md"}));
    assert!(is_error, "{out}");
    std::thread::sleep(Duration::from_secs(1));
    assert!(!home.join(".kioku/state/auto-update.json").exists());
    assert!(liveness(&home).is_null(), "only successful calls count");
    assert_eq!(fake.status_calls.load(Ordering::SeqCst), 0);

    // the first successful call: the check runs in the background
    let (is_error, out) = bridge.call("kioku_query", json!({"query": "引き継ぎ"}));
    assert!(!is_error, "{out}");
    assert!(out.ends_with("no hits"), "{out}");
    let st = wait_for_error(&home);
    assert_eq!(st["target"], "v99.0.0", "{st}");
    assert!(
        st["last_error"].as_str().unwrap().contains("has no"),
        "{st}"
    );
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(fake.status_calls.load(Ordering::SeqCst), 1);
    let marks = wait_for_liveness(&home);
    assert_eq!(marks["mcp"]["event"], "tool_call", "{marks}");
    assert_eq!(marks["mcp"]["ok"], true, "{marks}");

    // a second call of the same process does not check again
    let first_attempt = st["last_attempt"].clone();
    let (is_error, out) = bridge.call("kioku_query", json!({"query": "検索"}));
    assert!(!is_error && !out.contains("99.0.0"), "{out}");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fake.status_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(state(&home)["last_attempt"], first_attempt);
    drop(bridge);
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        original,
        "nothing was replaced"
    );
}

/// An older or equal server: nothing is started; `kioku_status` supplies the version
/// itself (no second status request).
#[test]
fn bridge_with_an_older_or_equal_server_starts_nothing() {
    for version in ["0.0.1", env!("CARGO_PKG_VERSION")] {
        let fake = fake_server_at(version);
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".kioku")).unwrap();
        std::fs::write(
            home.join(".kioku/config.toml"),
            "[update]\nallow_mirror = true\n",
        )
        .unwrap();
        let bin = copy_binary(tmp.path(), "bin");
        let mut bridge = Bridge::start(&bin, &home, &fake);
        let (is_error, out) = bridge.call("kioku_status", json!({}));
        assert!(!is_error, "{out}");
        assert!(out.contains("auto-update-proj"), "{out}");
        wait_for_liveness(&home);
        std::thread::sleep(Duration::from_secs(1));
        let st = state(&home);
        assert!(st["target"].is_null(), "{version}: {st}");
        assert!(st["last_notice"].is_null(), "{version}: {st}");
        assert_eq!(
            st["mismatch_since"].is_string(),
            version == "0.0.1",
            "{version}: {st}"
        );
        assert_eq!(fake.status_calls.load(Ordering::SeqCst), 1, "{version}");
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 0, "{version}");
    }
}

/// A winget-path bridge cannot update itself: the notice goes into the next query result,
/// once.
#[test]
fn winget_bridge_appends_the_notice_once() {
    let fake = fake_server();
    let tmp = tempfile::tempdir().unwrap();
    let bin = copy_binary(
        tmp.path(),
        "WinGet/Packages/misorafa.kioku_Microsoft.Winget.Source_8wekyb3d8bbwe",
    );
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".kioku")).unwrap();
    let mut bridge = Bridge::start(&bin, &home, &fake);
    let (is_error, out) = bridge.call("kioku_query", json!({"query": "引き継ぎ"}));
    assert!(!is_error && !out.contains("99.0.0"), "{out}");
    let start = Instant::now();
    while !state(&home)["last_notice"].is_string() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "no notice decided"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // the line is handed to the bridge right after the state file is saved
    std::thread::sleep(Duration::from_millis(300));
    let version = env!("CARGO_PKG_VERSION");
    let (_, out) = bridge.call("kioku_query", json!({"query": "設計"}));
    assert!(
        out.ends_with(&format!(
            "no hits\n\nkioku: kioku v99.0.0 が利用できます（この端末は v{version}）: winget upgrade misorafa.kioku"
        )),
        "{out}"
    );
    let (_, out) = bridge.call("kioku_query", json!({"query": "設計"}));
    assert!(!out.contains("99.0.0"), "once: {out}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fake.downloads.load(Ordering::SeqCst),
        0,
        "winget: no download"
    );
}
