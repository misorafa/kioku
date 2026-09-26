//! Bakes the target triple into the binary (`KIOKU_TARGET`) so `kioku update` fetches the
//! matching release asset (a musl build updates to musl).

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=KIOKU_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
