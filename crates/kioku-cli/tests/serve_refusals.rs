//! SPEC-M2.7 §5 / §6 with the real binary: `kioku serve` refuses a data directory written
//! by a newer kioku (exit 78, the data untouched) and one that another process has open.

use std::process::Command;

fn serve(data: &std::path::Path) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("KIOKU_") {
            cmd.env_remove(k);
        }
    }
    cmd.arg("serve")
        .env("HOME", data)
        .env("USERPROFILE", data)
        .env("KIOKU_DATA_DIR", data)
        .env("KIOKU_AUTH_TOKEN", "serve-refusal-token-0123456789")
        .env("KIOKU_BIND", "127.0.0.1")
        .env("KIOKU_PORT", "9")
        .output()
        .unwrap()
}

#[test]
fn serve_exits_78_on_a_newer_schema_and_refuses_a_locked_data_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("データ");
    let db = kioku_core::DataDir::new(&data).db_file();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "user_version", kioku_core::SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    let before = std::fs::read(&db).unwrap();
    let out = serve(&data);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("written by a newer kioku"), "{stderr}");
    assert!(stderr.contains("kioku update"), "{stderr}");
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "the newer database is untouched"
    );

    // Another process (here: this test) has the directory open.
    let other = tmp.path().join("other");
    let store = kioku_core::Store::open(kioku_core::Config::for_data_dir(&other)).unwrap();
    let out = serve(&other);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("another kioku is using"), "{stderr}");
    drop(store);
}
