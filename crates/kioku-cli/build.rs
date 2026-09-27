//! Bakes the target triple into the binary (`KIOKU_TARGET`) so `kioku update` fetches the
//! matching release asset (a musl build updates to musl), and on macOS embeds an
//! `Info.plist` into the `kioku` binary so the Local Network privacy prompt can identify
//! and ask for it (SPEC-M2 §10.3.1).

use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=KIOKU_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        let plist = out.join("Info.plist");
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        std::fs::write(&plist, info_plist(&version)).expect("writing Info.plist");
        println!(
            "cargo:rustc-link-arg-bin=kioku=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}

/// The embedded `Info.plist`: bundle identity plus the Local Network usage description.
fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.kioku.kioku</string>
  <key>CFBundleName</key><string>kioku</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>kioku serves shared memory to your AI coding agents on other machines in your network. / kioku は、同じネットワーク内の別のマシンで動く AI コーディングエージェントに共有メモリを提供します。</string>
</dict>
</plist>
"#
    )
}
