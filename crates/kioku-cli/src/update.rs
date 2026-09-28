//! `kioku update` (M2 §13.3): download the release asset for the target this binary was
//! built for, verify its SHA-256 exactly like install.sh, replace the running binary
//! atomically and restart the kioku service when one is installed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::setup::{KIOKU_REPO, SetupEnv, VERSION, install_ps1_url, install_sh_url};

/// Target triple this binary was built for (`build.rs`); a musl build updates to musl.
pub const TARGET: &str = env!("KIOKU_TARGET");

/// File name of the kioku binary, in a release archive and on disk (`kioku.exe` on Windows).
pub const BIN_NAME: &str = if cfg!(windows) { "kioku.exe" } else { "kioku" };

/// `<exe><suffix>` next to `exe`, e.g. `kioku.exe.old`.
pub fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Deletes the `kioku.exe.old*` copies a Windows update left next to the running binary
/// (SPEC-M2.2 §5 step 3). Best effort, and a no-op elsewhere.
pub fn remove_stale_old_binary() {
    if !cfg!(windows) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        remove_old_copies(&exe);
    }
}

/// Deletes every `<exe>.old*` next to `exe` that can be deleted. A copy still executing (a
/// `kioku mcp` an agent started before the update, or an app that lingers in the background
/// like Orca) cannot be, and is left for a later run.
pub fn remove_old_copies(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    let prefix = format!("{}.old", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A free name to move `exe` aside to: `<exe>.old`, or when that one is still there (in
/// use, so it could not be deleted) `<exe>.old-<unix ms>[-n]`.
fn aside_name(exe: &Path) -> PathBuf {
    let old = sibling(exe, ".old");
    if !old.exists() {
        return old;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    (0u32..)
        .map(|n| {
            let suffix = if n == 0 {
                format!(".old-{ms}")
            } else {
                format!(".old-{ms}-{n}")
            };
            sibling(exe, &suffix)
        })
        .find(|p| !p.exists())
        .unwrap_or_else(|| sibling(exe, ".old-x"))
}

/// The `tar` to run: on Windows the system's own bsdtar (`%SystemRoot%\System32\tar.exe`,
/// shipped since Windows 10 1803) — a GNU tar from Git for Windows earlier on PATH reads
/// `C:\…` as a remote host; elsewhere `tar` from PATH.
fn tar_program() -> String {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let tar = PathBuf::from(root).join("System32").join("tar.exe");
        if tar.is_file() {
            return tar.display().to_string();
        }
    }
    "tar".to_string()
}

/// Puts `new` in the place of `exe`. Without `rename_dance` (unix) one atomic rename; with
/// it (Windows, where a running exe cannot be overwritten but can be renamed): delete the
/// `<exe>.old*` copies nothing runs any more, rename `exe` to a free `.old…` name (see
/// [`aside_name`]: an old copy still in use no longer blocks the update — found on a real
/// Windows 11 where Orca kept `kioku mcp` alive), then `new` → `exe`, moving the old one
/// back if the second rename fails (SPEC-M2.2 §5).
pub fn swap_binary(new: &Path, exe: &Path, rename_dance: bool) -> anyhow::Result<()> {
    if !rename_dance {
        return std::fs::rename(new, exe).with_context(|| format!("replacing {}", exe.display()));
    }
    remove_old_copies(exe);
    let old = aside_name(exe);
    let moved = exe.exists();
    if moved {
        std::fs::rename(exe, &old)
            .with_context(|| format!("moving {} out of the way", exe.display()))?;
    }
    if let Err(e) = std::fs::rename(new, exe) {
        if moved {
            let _ = std::fs::rename(&old, exe);
        }
        return Err(e).with_context(|| format!("replacing {}", exe.display()));
    }
    Ok(())
}

/// The tag in a `…/releases/tag/<tag>` URL (where GitHub's `releases/latest` lands).
pub fn tag_from_release_url(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next()?.trim_end_matches('/');
    let (_, tag) = url.rsplit_once("/releases/tag/")?;
    (!tag.is_empty() && !tag.contains('/')).then(|| tag.to_string())
}

/// The checksum for `asset` in `SHA256SUMS` / `<asset>.sha256` text (`<hex>  [*]<file>`).
pub fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (hex, file) = (it.next()?, it.next()?);
        (file.trim_start_matches('*') == asset).then(|| hex.to_ascii_lowercase())
    })
}

/// True when release `tag` (`vX.Y.Z`) is newer than `current` (`X.Y.Z`).
pub fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        let v = v.trim_start_matches('v');
        let core = v.split(['-', '+']).next().unwrap_or(v);
        core.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    parse(tag) > parse(current)
}

/// Whether `kioku update` installs release `tag` over `current`: a tag given with
/// `--version` (`explicit`) is installed unless it is the running version (downgrades
/// allowed); the latest release only when it is strictly newer (M2 §13.3).
pub fn should_install(tag: &str, current: &str, explicit: bool) -> bool {
    if explicit {
        tag.trim_start_matches('v') != current
    } else {
        is_newer(tag, current)
    }
}

fn get(http: &reqwest::blocking::Client, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let resp = http.get(url).send().with_context(|| format!("GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    Ok(Some(resp.bytes()?.to_vec()))
}

/// `kioku update [--version <tag>] [--check]`; returns the exit code (10 = `--check` found
/// a newer release).
pub fn run_update(version: Option<String>, check: bool) -> anyhow::Result<i32> {
    let repo = std::env::var("KIOKU_REPO").unwrap_or_else(|_| KIOKU_REPO.to_string());
    let base = std::env::var("KIOKU_DOWNLOAD_BASE")
        .unwrap_or_else(|_| format!("https://github.com/{repo}/releases"));
    let base = base.trim_end_matches('/');
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("kioku/{VERSION}"))
        .build()?;
    let explicit = version.is_some();
    let tag = match version {
        Some(v) if v.starts_with('v') => v,
        Some(v) => format!("v{v}"),
        None => {
            let resp = http.head(format!("{base}/latest")).send()?;
            tag_from_release_url(resp.url().as_str())
                .with_context(|| format!("no published release found at {base}"))?
        }
    };
    if check {
        println!("current: v{VERSION}\nlatest:  {tag} ({TARGET})");
        return Ok(if is_newer(&tag, VERSION) { 10 } else { 0 });
    }
    if !should_install(&tag, VERSION, explicit) {
        if explicit {
            println!("kioku v{VERSION} is already installed");
        } else {
            println!("kioku v{VERSION} is already up to date (latest release: {tag})");
        }
        return Ok(0);
    }
    let exe = std::env::current_exe().context("locating the kioku binary")?;
    let exe = kioku_core::util::canonical_plain(&exe).unwrap_or(exe);
    let dir = exe.parent().context("binary has no parent directory")?;
    let new = if cfg!(windows) {
        sibling(&exe, ".new")
    } else {
        dir.join(format!(".kioku.new.{}", std::process::id()))
    };
    if std::fs::write(&new, b"").is_err() {
        if cfg!(windows) {
            bail!(
                "{} is not writable; re-run the installer instead:\n  & ([scriptblock]::Create((irm {}))) -Version {tag} -NoSetup",
                dir.display(),
                install_ps1_url()
            );
        }
        bail!(
            "{} is not writable; re-run the installer instead (never with sudo):\n  curl -fsSL {} | sh -s -- --version {tag} --no-setup",
            dir.display(),
            install_sh_url()
        );
    }
    let work = dir.join(format!(".kioku-update.{}", std::process::id()));
    let result = replace(&http, base, &tag, &exe, &new, &work);
    let _ = std::fs::remove_file(&new);
    let _ = std::fs::remove_dir_all(&work);
    let installed = result?;
    println!("kioku: updated v{VERSION} -> {tag} ({installed})");
    restart_service(&exe);
    Ok(0)
}

/// Downloads, verifies, extracts and renames the new binary over `exe`.
fn replace(
    http: &reqwest::blocking::Client,
    base: &str,
    tag: &str,
    exe: &Path,
    new: &Path,
    work: &Path,
) -> anyhow::Result<String> {
    let asset = format!("kioku-{tag}-{TARGET}.tar.gz");
    let url = format!("{base}/download/{tag}/{asset}");
    let bytes = get(http, &url)?.with_context(|| format!("release {tag} has no {asset}"))?;
    let expected = [
        format!("{base}/download/{tag}/SHA256SUMS"),
        format!("{url}.sha256"),
    ]
    .iter()
    .find_map(|u| {
        let text = String::from_utf8(get(http, u).ok()??).ok()?;
        checksum_for(&text, &asset)
    })
    .with_context(|| {
        format!("no checksum for {asset}; refusing to install an unverified binary")
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "checksum mismatch for {asset} (expected {expected}, got {actual}); nothing was changed"
        );
    }
    std::fs::create_dir_all(work)?;
    let tarball = work.join(&asset);
    std::fs::write(&tarball, &bytes)?;
    let status = kioku_core::util::quiet_command(&tar_program())
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(work)
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("tar could not extract {asset}");
    }
    let bin = work.join(format!("kioku-{tag}-{TARGET}")).join(BIN_NAME);
    std::fs::copy(&bin, new).with_context(|| format!("{asset} has no kioku binary"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))?;
    }
    // On Windows run the extracted `kioku.exe`: `kioku.exe.new` has no executable extension.
    let probe = if cfg!(windows) { &bin } else { new };
    let out = kioku_core::util::quiet_command(&probe.display().to_string())
        .arg("--version")
        .output()?;
    if !out.status.success() {
        bail!(
            "the new binary does not run here: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Same directory: atomic; a running `kioku serve` keeps the old inode until restarted.
    // On Windows the running exe is renamed aside first (SPEC-M2.2 §5).
    swap_binary(new, exe, cfg!(windows))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Restarts the kioku service if one is installed (hooks need nothing: same path).
fn restart_service(exe: &Path) {
    let Ok(env) = SetupEnv::from_process(exe.display().to_string()) else {
        return;
    };
    let Ok(cfg) = kioku_core::Config::load_from_dir(&env.config_dir(), &env.vars) else {
        return;
    };
    let manager = env.service_manager(&cfg.data_dir);
    if !manager.is_installed() {
        return;
    }
    match manager.restart() {
        Ok(_) => println!("kioku: restarted the service ({})", manager.describe()),
        Err(err) => eprintln!(
            "kioku: warning: restart the service yourself (`kioku service start`): {err:#}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_url_checksums_and_versions() {
        let u = "https://github.com/misorafa/kioku/releases/tag/v0.2.0";
        assert_eq!(tag_from_release_url(u).as_deref(), Some("v0.2.0"));
        assert_eq!(
            tag_from_release_url(&format!("{u}/?x=1")).as_deref(),
            Some("v0.2.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/misorafa/kioku/releases"),
            None
        );
        let sums = "AB12  kioku-v0.2.0-x86_64-unknown-linux-musl.tar.gz\n\
                    cd34 *kioku-v0.2.0-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "kioku-v0.2.0-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("ab12")
        );
        assert_eq!(
            checksum_for(sums, "kioku-v0.2.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("cd34")
        );
        assert_eq!(
            checksum_for(sums, "kioku-v0.2.0-x86_64-apple-darwin.tar.gz"),
            None
        );
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0-rc1", "0.1.0"));
        assert!(TARGET.contains('-'));
    }

    #[test]
    fn update_without_version_never_downgrades() {
        // Latest release: strictly newer only.
        assert!(should_install("v0.3.0", "0.2.0", false));
        assert!(!should_install("v0.2.0", "0.2.0", false));
        assert!(
            !should_install("v0.1.9", "0.2.0", false),
            "an older `latest` (e.g. a dev build ahead of the last release) is not installed"
        );
        assert!(!should_install("v0.2.0-rc1", "0.2.0", false));
        // --version: any other tag, including a downgrade.
        assert!(should_install("v0.1.9", "0.2.0", true));
        assert!(should_install("v0.3.0", "0.2.0", true));
        assert!(!should_install("v0.2.0", "0.2.0", true));
    }

    /// Serves `files` (path → body) on an ephemeral port; unknown paths are 404.
    #[cfg(unix)] // only the unix-only replace test uses it
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        use axum::http::{StatusCode, Uri};
        let files = std::sync::Arc::new(files);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = axum::Router::new().fallback(move |uri: Uri| {
                    let files = files.clone();
                    async move {
                        match files.iter().find(|(p, _)| p == uri.path()) {
                            Some((_, body)) => (StatusCode::OK, body.clone()),
                            None => (StatusCode::NOT_FOUND, Vec::new()),
                        }
                    }
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://{}/releases", rx.recv().unwrap())
    }

    /// Regression (real Windows 11, 2026-09-29): a `kioku.exe.old` that cannot be deleted
    /// (still executing) must not block the update. A directory stands in for the locked file.
    #[test]
    fn a_locked_old_copy_does_not_block_the_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("kioku.exe");
        let locked = sibling(&exe, ".old");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("in-use"), "x").unwrap();
        std::fs::write(&exe, "v1").unwrap();
        let new = sibling(&exe, ".new");
        std::fs::write(&new, "v2").unwrap();
        swap_binary(&new, &exe, true).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        assert!(locked.is_dir(), "the locked copy is left alone");
        let aside: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("kioku.exe.old-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(&aside[0])).unwrap(),
            "v1"
        );
        // A later run cleans up what it can: the free copy goes, the locked one stays.
        remove_old_copies(&exe);
        assert!(!tmp.path().join(&aside[0]).exists());
        assert!(locked.is_dir());
    }

    #[test]
    fn swap_binary_renames_the_running_exe_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("kioku.exe");
        let new = sibling(&exe, ".new");
        assert_eq!(new, tmp.path().join("kioku.exe.new"));
        let old = sibling(&exe, ".old");
        // A stale .old from an earlier update is replaced.
        std::fs::write(&old, "stale").unwrap();
        std::fs::write(&exe, "v1").unwrap();
        std::fs::write(&new, "v2").unwrap();
        swap_binary(&new, &exe, true).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "v1");
        assert!(!new.exists());
        // A failed second rename puts the current binary back.
        assert!(swap_binary(&tmp.path().join("missing.new"), &exe, true).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        // Plain rename (unix).
        std::fs::write(&new, "v3").unwrap();
        swap_binary(&new, &exe, false).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v3");
        assert!(BIN_NAME.starts_with("kioku"));
    }

    // The fixture "binary" is a shell script, which only runs on unix.
    #[cfg(unix)]
    #[test]
    fn replace_verifies_the_checksum_before_touching_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let name = format!("kioku-v9.9.9-{TARGET}");
        std::fs::create_dir(tmp.path().join(&name)).unwrap();
        std::fs::write(
            tmp.path().join(&name).join("kioku"),
            "#!/bin/sh\necho 'kioku 9.9.9'\n",
        )
        .unwrap();
        let tarball = tmp.path().join(format!("{name}.tar.gz"));
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(tmp.path())
            .arg(&name)
            .status()
            .unwrap();
        assert!(ok.success());
        let bytes = std::fs::read(&tarball).unwrap();
        let asset = format!("/releases/download/v9.9.9/{name}.tar.gz");
        let good = format!("{:x}  {name}.tar.gz\n", Sha256::digest(&bytes));
        let bad = format!("{}  {name}.tar.gz\n", "0".repeat(64));
        let sums = "/releases/download/v9.9.9/SHA256SUMS".to_string();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let bin_dir = tmp.path().join("bin dir");
        std::fs::create_dir(&bin_dir).unwrap();
        let exe = bin_dir.join("kioku");
        let run = |base: &str| {
            std::fs::write(&exe, "old").unwrap();
            let r = replace(
                &http,
                base,
                "v9.9.9",
                &exe,
                &bin_dir.join(".kioku.new.1"),
                &bin_dir.join(".kioku-update.1"),
            );
            let _ = std::fs::remove_dir_all(bin_dir.join(".kioku-update.1"));
            (r, std::fs::read_to_string(&exe).unwrap())
        };

        let (r, content) = run(&serve(vec![
            (asset.clone(), bytes.clone()),
            (sums.clone(), bad.into()),
        ]));
        assert!(format!("{:#}", r.unwrap_err()).contains("checksum mismatch"));
        assert_eq!(content, "old");

        let (r, content) = run(&serve(vec![(asset.clone(), bytes.clone())]));
        assert!(format!("{:#}", r.unwrap_err()).contains("no checksum"));
        assert_eq!(content, "old");

        // Pre-SHA256SUMS releases: `<asset>.sha256`.
        let (r, content) = run(&serve(vec![
            (asset.clone(), bytes),
            (format!("{asset}.sha256"), good.into()),
        ]));
        assert_eq!(r.unwrap(), "kioku 9.9.9");
        assert!(content.contains("kioku 9.9.9"));
        assert!(!bin_dir.join(".kioku.new.1").exists());
    }
}
