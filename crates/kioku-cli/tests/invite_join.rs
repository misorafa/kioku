//! SPEC-M2.3 §6 (CLI): the real `kioku` binary runs `kioku invite` on a "server machine"
//! HOME and `kioku join` on a "new machine" HOME against the real server on an ephemeral
//! loopback port. The client config gets the token; no output ever shows it.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use kioku_core::{Config, Store};

const TOKEN: &str = "join-token-5f0e9c1d2b3a4f5e6d7c8b9a";

fn start_server() -> (String, u16, tempfile::TempDir) {
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
    let addr = rx.recv().unwrap();
    (format!("http://{addr}"), addr.port(), dir)
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the kioku binary with `home` as HOME and no KIOKU_* variables from the outside.
fn kioku(home: &Path, args: &[&str]) -> Out {
    kioku_env(home, args, &[])
}

/// [`kioku`] with extra environment variables.
fn kioku_env(home: &Path, args: &[&str], extra: &[(&str, String)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("NO_PROXY", "127.0.0.1,localhost");
    for (k, _) in std::env::vars() {
        if k.starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn server_home(port: u16) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let kioku = home.path().join(".kioku");
    std::fs::create_dir_all(&kioku).unwrap();
    std::fs::write(
        kioku.join("config.toml"),
        format!(
            "[server]\nbind = \"127.0.0.1\"\nport = {port}\nauth_token = \"{TOKEN}\"\n\n[client]\nserver_url = \"http://127.0.0.1:{port}\"\nauth_token = \"{TOKEN}\"\n"
        ),
    )
    .unwrap();
    home
}

/// The code in the `macOS / Linux:` line of `kioku invite`.
fn code_of(invite_stdout: &str) -> String {
    let line = invite_stdout
        .lines()
        .find(|l| l.trim_start().starts_with("macOS / Linux:"))
        .unwrap_or_else(|| panic!("{invite_stdout}"));
    let url = line.split_whitespace().nth(5).unwrap();
    url.rsplit('/').next().unwrap().to_string()
}

#[test]
fn invite_then_join_writes_the_client_config_without_printing_the_token() {
    let (base, port, _data) = start_server();
    let server = server_home(port);

    let inv = kioku(server.path(), &["invite", "--uses", "2"]);
    assert_eq!(inv.code, 0, "{}{}", inv.stdout, inv.stderr);
    assert!(
        inv.stdout.contains(
            "Paste ONE of these on the machine to add (valid 10 minutes, up to 2 machines)"
        ),
        "{}",
        inv.stdout
    );
    assert!(inv.stdout.contains("追加するマシンで"), "{}", inv.stdout);
    assert!(inv.stdout.contains("| iex"));
    assert!(!inv.stdout.contains(TOKEN) && !inv.stderr.contains(TOKEN));
    let code = code_of(&inv.stdout);
    assert!(inv.stdout.contains(&format!("/i/{code}.ps1 | iex")));

    // The new machine: Claude Code installed, nothing else.
    let client = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(client.path().join(".claude")).unwrap();
    let join = kioku(client.path(), &["join", &base, &code.to_lowercase()]);
    assert_eq!(join.code, 0, "{}{}", join.stdout, join.stderr);
    assert!(!join.stdout.contains(TOKEN), "{}", join.stdout);
    assert!(!join.stderr.contains(TOKEN), "{}", join.stderr);
    assert!(
        join.stdout
            .contains("kioku の準備ができました。Claude Code を再起動してください。"),
        "{}",
        join.stdout
    );
    assert!(
        join.stdout
            .contains("kioku is ready - restart Claude Code.")
    );
    let cfg = std::fs::read_to_string(client.path().join(".kioku").join("config.toml")).unwrap();
    assert!(cfg.contains(TOKEN), "{cfg}");
    assert!(cfg.contains(&base) && !cfg.contains("[server]"), "{cfg}");
    assert!(client.path().join(".claude").join("settings.json").exists());

    // A second machine uses the second use; a third attempt is refused in one sentence.
    let second = tempfile::tempdir().unwrap();
    let j2 = kioku(second.path(), &["join", &base, &code, "--no-agents"]);
    assert_eq!(j2.code, 0, "{}{}", j2.stdout, j2.stderr);
    assert!(!j2.stdout.contains(TOKEN) && !j2.stderr.contains(TOKEN));
    let third = tempfile::tempdir().unwrap();
    let j3 = kioku(third.path(), &["join", &base, &code]);
    assert_eq!(j3.code, 1);
    assert!(
        j3.stderr
            .contains("ask for a new kioku invite on the server"),
        "{}",
        j3.stderr
    );
    assert!(j3.stderr.contains("招待コード"), "{}", j3.stderr);
    assert!(!third.path().join(".kioku").join("config.toml").exists());

    // `join` replaces an old client config (after rotate-token, say) with the new token.
    std::fs::write(
        client.path().join(".kioku").join("config.toml"),
        format!("[client]\nserver_url = \"{base}\"\nauth_token = \"stale-token\"\n"),
    )
    .unwrap();
    let again = kioku(server.path(), &["invite"]);
    let j4 = kioku(
        client.path(),
        &["join", &base, &code_of(&again.stdout), "--no-agents"],
    );
    assert_eq!(j4.code, 0, "{}{}", j4.stdout, j4.stderr);
    let cfg = std::fs::read_to_string(client.path().join(".kioku").join("config.toml")).unwrap();
    assert!(cfg.contains(TOKEN) && !cfg.contains("stale-token"), "{cfg}");
}

#[test]
fn refusals() {
    let (base, port, _data) = start_server();
    // `invite` on a client-only machine.
    let client = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(client.path().join(".kioku")).unwrap();
    std::fs::write(
        client.path().join(".kioku").join("config.toml"),
        format!("[client]\nserver_url = \"{base}\"\nauth_token = \"{TOKEN}\"\n"),
    )
    .unwrap();
    let r = kioku(client.path(), &["invite"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("run kioku invite on the server machine"),
        "{}",
        r.stderr
    );
    // `invite` with no config at all.
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(kioku(empty.path(), &["invite"]).code, 1);
    // …unless the server's token comes from the environment (Docker).
    let docker = kioku_env(
        empty.path(),
        &["invite"],
        &[
            ("KIOKU_AUTH_TOKEN", TOKEN.to_string()),
            ("KIOKU_PORT", port.to_string()),
        ],
    );
    assert_eq!(docker.code, 0, "{}{}", docker.stdout, docker.stderr);
    assert!(docker.stdout.contains("/i/"), "{}", docker.stdout);

    // `invite` when the server is down.
    let down = server_home({
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    });
    let r = kioku(down.path(), &["invite"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("kioku service start"), "{}", r.stderr);

    // `join` with a made-up code, and against nothing listening.
    let fresh = tempfile::tempdir().unwrap();
    let r = kioku(fresh.path(), &["join", &base, "AAAAAAAA"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("invalid, expired or already used"),
        "{}",
        r.stderr
    );
    let dead = format!("127.0.0.1:{}", {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    });
    let r = kioku(fresh.path(), &["join", &dead, "AAAAAAAA"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains(&format!("Cannot reach http://{dead}")),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("firewall"), "{}", r.stderr);
    assert!(!fresh.path().join(".kioku").join("config.toml").exists());
}
