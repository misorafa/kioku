//! SPEC-M2.8 §3 through the real binary: `kioku status` (sizes), `kioku prune`, `kioku
//! forget` against the real server on an ephemeral loopback port over a temp data dir
//! (never the user's server or `~/.kioku`).

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use kioku_core::{Config, Store};
use serde_json::{Value, json};

const TOKEN: &str = "retention-token-0123456789";

struct Server {
    base: String,
    _dir: tempfile::TempDir,
}

fn start_server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(Config::for_data_dir(dir.path())).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = kioku_server::build_app(store, TOKEN.to_string());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    Server {
        base: format!("http://{}", rx.recv().unwrap()),
        _dir: dir,
    }
}

fn post(base: &str, path: &str, body: Value) -> Value {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/api/v1/{path}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .unwrap()
        .json()
        .unwrap()
}

/// Runs the real `kioku` binary as a client of `base` with a throwaway home.
fn kioku(home: &Path, base: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
    // Keep the platform's environment (Windows needs SystemRoot for sockets); drop only
    // kioku's own variables so nothing reaches a real server or ~/.kioku.
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIOKU_DATA_DIR", home.join(".kioku"))
        .env("KIOKU_SERVER_URL", base)
        .env("KIOKU_AUTH_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn session(base: &str, id: &str, project: &str, prompt: &str) {
    post(
        base,
        "sessions/start",
        json!({"session_id": id, "agent": "claude-code", "cwd": "/w", "source": "startup",
               "project": {"id": project, "name": project, "root": "/w", "remote": null}}),
    );
    post(
        base,
        "observations",
        json!({"session_id": id, "kind": "prompt", "payload": {"prompt": prompt}}),
    );
    post(base, &format!("sessions/{id}/finalize"), json!({}));
}

#[test]
fn status_prune_and_forget_through_the_binary() {
    let server = start_server();
    let home = tempfile::tempdir().unwrap();
    let base = server.base.as_str();
    session(base, "s-keep", "proj-a", "残しておく設計の議論");
    session(base, "s-drop", "proj-a", "誤って貼った秘密の鍵の話");
    session(base, "s-other", "proj-b", "別プロジェクトの作業");

    let out = kioku(home.path(), base, &["status"]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("storage      : db "), "{t}");
    assert!(t.contains("last prune   : never"), "{t}");

    let out = kioku(home.path(), base, &["prune", "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("would prune (dry run"),
        "{}",
        text(&out)
    );
    let out = kioku(home.path(), base, &["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).starts_with("pruned"), "{}", text(&out));
    assert!(!text(&kioku(home.path(), base, &["status"])).contains("last prune   : never"));

    let out = kioku(home.path(), base, &["search", "秘密の鍵"]);
    assert!(text(&out).contains("/sessions/"), "{}", text(&out));
    let out = kioku(
        home.path(),
        base,
        &["forget", "--session", "s-drop", "--purge-history"],
    );
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("forgot in project proj-a: 1 session(s)"), "{t}");
    assert!(t.contains("git filter-repo"), "{t}");
    let out = kioku(home.path(), base, &["search", "秘密の鍵"]);
    assert!(text(&out).contains("no hits"), "{}", text(&out));
    assert!(text(&kioku(home.path(), base, &["search", "設計の議論"])).contains("/sessions/"));

    // A project is forgotten only with a confirmation: stdin is not a terminal here.
    let out = kioku(home.path(), base, &["forget", "--project", "proj-b"]);
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("would forget in project proj-b"), "{t}");
    assert!(t.contains("without --yes"), "{t}");
    assert!(text(&kioku(home.path(), base, &["search", "別プロジェクト"])).contains("proj-b/"));
    let out = kioku(
        home.path(),
        base,
        &["forget", "--project", "proj-b", "--yes"],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&kioku(home.path(), base, &["search", "別プロジェクト"])).contains("no hits"));
}
