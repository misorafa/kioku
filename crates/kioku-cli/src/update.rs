//! `kioku update` (M2 §13.3, SPEC-M2.5 §4): download the release asset for the target this
//! binary was built for, verify its SHA-256 exactly like install.sh (plus, on macOS, the
//! Developer ID signature), check that the new binary reports the expected version, replace
//! the running binary atomically and restart the kioku service when one is installed. The
//! building blocks here are shared with the automatic updates in [`crate::auto_update`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::setup::{KIOKU_REPO, SetupEnv, VERSION, install_ps1_url, install_sh_url};

/// Target triple this binary was built for (`build.rs`); a musl build updates to musl.
pub const TARGET: &str = env!("KIOKU_TARGET");

/// File name of the kioku binary, in a release archive and on disk (`kioku.exe` on Windows).
pub const BIN_NAME: &str = if cfg!(windows) { "kioku.exe" } else { "kioku" };

/// Apple Developer Team ID that signs the macOS release binaries (SPEC-M2.5 §4.2).
pub const APPLE_TEAM_ID: &str = "7F6HLTW75D";

/// How the macOS code signature of a downloaded binary is treated (SPEC-M2.5 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Refuse a binary not signed by [`APPLE_TEAM_ID`] (automatic updates,
    /// `--require-signature`).
    Require,
    /// Warn and install anyway (manual `kioku update`: a self-built fork still works).
    Warn,
    /// Do not look (Windows / Linux, and tests with unsigned dummy binaries).
    Skip,
}

impl SignaturePolicy {
    /// The policy of an automatic update: [`SignaturePolicy::Require`] on macOS, else skip.
    pub fn automatic() -> SignaturePolicy {
        if cfg!(target_os = "macos") {
            SignaturePolicy::Require
        } else {
            SignaturePolicy::Skip
        }
    }

    /// The policy of a manual `kioku update` (`require` = `--require-signature`).
    pub fn manual(require: bool) -> SignaturePolicy {
        match (cfg!(target_os = "macos"), require) {
            (false, _) => SignaturePolicy::Skip,
            (true, true) => SignaturePolicy::Require,
            (true, false) => SignaturePolicy::Warn,
        }
    }
}

/// What an update checks besides the SHA-256 (SPEC-M2.5 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verify {
    /// macOS code signature.
    pub signature: SignaturePolicy,
    /// Refuse a binary whose `--version` does not report the tag (`false`: warn only).
    pub exact_version: bool,
}

impl Verify {
    /// Automatic updates: kioku's signature on macOS and the exact version, both required.
    pub fn automatic() -> Verify {
        Verify {
            signature: SignaturePolicy::automatic(),
            exact_version: true,
        }
    }

    /// Manual `kioku update`: warnings only (a self-built fork, a re-tagged mirror), unless
    /// `--require-signature`.
    pub fn manual(require_signature: bool) -> Verify {
        Verify {
            signature: SignaturePolicy::manual(require_signature),
            exact_version: false,
        }
    }

    /// Tests with an unsigned dummy binary: exact version, no signature check.
    #[cfg(test)]
    pub(crate) fn unsigned() -> Verify {
        Verify {
            signature: SignaturePolicy::Skip,
            exact_version: true,
        }
    }
}

/// Arguments of `kioku update`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateArgs {
    /// `--version <tag>`.
    pub version: Option<String>,
    /// `--check`.
    pub check: bool,
    /// `--background` (SPEC-M2.5 §3.3: started by the SessionStart hook).
    pub background: bool,
    /// `--require-signature` (macOS: refuse a binary not signed by kioku's team).
    pub require_signature: bool,
    /// `--rollback` (SPEC-M2.7 §7: put `<exe>.prev` back).
    pub rollback: bool,
}

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

/// The `TeamIdentifier=` value in `codesign -dv` output (`None` when absent).
pub fn parse_team_id(codesign_dv: &str) -> Option<String> {
    codesign_dv.lines().find_map(|l| {
        l.trim()
            .strip_prefix("TeamIdentifier=")
            .map(|v| v.trim().to_string())
    })
}

/// True when `kioku --version` output (`kioku 0.7.0`) reports release `tag` (`v0.7.0`).
pub fn version_matches(output: &str, tag: &str) -> bool {
    let want = tag.trim_start_matches('v');
    output
        .split_whitespace()
        .any(|w| w.trim_start_matches('v') == want)
}

/// macOS: `codesign --verify --strict <bin>` passes and `codesign -dv` names
/// [`APPLE_TEAM_ID`]. Elsewhere always fine (no code signing, SPEC-M2.5 §4.3).
pub fn verify_signature(bin: &Path) -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = std::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(bin)
        .output()
        .context("running codesign --verify")?;
    if !out.status.success() {
        bail!(
            "the new binary's code signature does not verify: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let out = std::process::Command::new("codesign")
        .arg("-dv")
        .arg(bin)
        .output()
        .context("running codesign -dv")?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    match parse_team_id(&text) {
        Some(team) if team == APPLE_TEAM_ID => Ok(()),
        Some(team) => bail!("the new binary is signed by team {team}, not kioku's {APPLE_TEAM_ID}"),
        None => bail!("the new binary is not signed by kioku's team {APPLE_TEAM_ID}"),
    }
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

/// True when `exe` lives in winget's portable package store
/// (`…\\WinGet\\Packages\\misorafa.kioku_…\\kioku.exe`).
pub fn is_winget_install(exe: &Path) -> bool {
    let s = exe
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('/', "\\");
    s.contains("\\winget\\packages\\") || s.contains("\\winget\\links\\")
}

/// True when `exe` lives in a Homebrew keg (`/opt/homebrew/Cellar/kioku/…`,
/// `/usr/local/Cellar/kioku/…`, Linuxbrew's `…/.linuxbrew/Cellar/kioku/…`; SPEC-M3.3 §1).
pub fn is_brew_install(exe: &Path) -> bool {
    let s = exe.to_string_lossy().replace('\\', "/");
    s.contains("/Cellar/kioku/")
}

/// A package manager that owns the kioku binary: `kioku update` and the automatic updates
/// leave the binary alone and point at the manager's upgrade command instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    /// winget (`misorafa.kioku`, packaging/winget).
    Winget,
    /// Homebrew (`misorafa/tap/kioku`, packaging/homebrew).
    Homebrew,
}

impl PackageManager {
    /// Human name (`winget`, `Homebrew`).
    pub fn name(self) -> &'static str {
        match self {
            PackageManager::Winget => "winget",
            PackageManager::Homebrew => "Homebrew",
        }
    }

    /// The command that updates kioku.
    pub fn upgrade_command(self) -> &'static str {
        match self {
            PackageManager::Winget => "winget upgrade misorafa.kioku",
            PackageManager::Homebrew => "brew upgrade kioku",
        }
    }

    /// The command that removes kioku.
    pub fn uninstall_command(self) -> &'static str {
        match self {
            PackageManager::Winget => "winget uninstall misorafa.kioku",
            PackageManager::Homebrew => "brew uninstall kioku",
        }
    }
}

/// Which package manager owns `exe` (a symlink such as `/opt/homebrew/bin/kioku` is
/// followed), if any.
pub fn package_manager(exe: &Path) -> Option<PackageManager> {
    let real = kioku_core::util::canonical_plain(exe).unwrap_or_else(|_| exe.to_path_buf());
    if is_winget_install(exe) || is_winget_install(&real) {
        Some(PackageManager::Winget)
    } else if is_brew_install(exe) || is_brew_install(&real) {
        Some(PackageManager::Homebrew)
    } else {
        None
    }
}

/// The path to register in hooks and the service for `exe`: a Homebrew keg path
/// (`<prefix>/Cellar/kioku/<version>/bin/kioku`, gone after `brew upgrade` + cleanup) becomes
/// the stable `<prefix>/bin/kioku` link when it exists; anything else is returned unchanged.
pub fn stable_binary_path(exe: &Path) -> PathBuf {
    let s = exe.to_string_lossy().replace('\\', "/");
    if let Some(i) = s.find("/Cellar/kioku/") {
        let link = Path::new(&s[..i]).join("bin").join(BIN_NAME);
        if link.exists() {
            return link;
        }
    }
    exe.to_path_buf()
}

/// The manager's notice for `kioku update` (nothing is replaced).
pub fn managed_update_notice(pm: PackageManager, tag: &str) -> String {
    format!(
        "kioku was installed with {}; update it with: {} (latest release: {tag})",
        pm.name(),
        pm.upgrade_command()
    )
}

/// The official releases: `https://github.com/misorafa/kioku/releases`.
pub fn official_release_base() -> String {
    format!("https://github.com/{KIOKU_REPO}/releases")
}

/// Base URL of the releases (no trailing `/`) and a warning to show (SPEC-M2.7 §11). Only
/// with `[update] allow_mirror = true` are `KIOKU_DOWNLOAD_BASE` (a mirror) and `KIOKU_REPO`
/// (a fork) honoured; otherwise they are ignored with a warning, so an environment variable
/// alone can never redirect an update. A mirror must use https unless it is on loopback.
pub fn release_base_for(
    update: &kioku_core::UpdateConfig,
    vars: &std::collections::HashMap<String, String>,
) -> anyhow::Result<(String, Option<String>)> {
    let get = |k: &str| vars.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let (mirror, repo) = (get("KIOKU_DOWNLOAD_BASE"), get("KIOKU_REPO"));
    if !update.allow_mirror {
        let warning = (mirror.is_some() || repo.is_some()).then(|| {
            "kioku: warning: KIOKU_DOWNLOAD_BASE / KIOKU_REPO ignored (set [update] allow_mirror = true in config.toml to use a mirror or fork)".to_string()
        });
        return Ok((official_release_base(), warning));
    }
    let base = match (mirror, repo) {
        (Some(m), _) => m.trim_end_matches('/').to_string(),
        (None, Some(r)) => format!("https://github.com/{r}/releases"),
        (None, None) => official_release_base(),
    };
    let url = reqwest::Url::parse(&base).with_context(|| format!("invalid release base {base}"))?;
    match url.scheme() {
        "https" => {}
        "http" if crate::client::is_loopback_url(&url) => {}
        _ => bail!(
            "refusing the release base {base}: a mirror must use https (plain http only on this machine)"
        ),
    }
    Ok((base, None))
}

/// [`release_base_for`] with `cfg`'s `[update]` and the process environment.
pub fn release_base(cfg: &kioku_core::Config) -> anyhow::Result<(String, Option<String>)> {
    release_base_for(&cfg.update, &kioku_core::util::env_vars())
}

/// The HTTP client of updates from `base` (5 min timeout, `kioku/<version>` user agent;
/// no proxy for a release mirror on this machine or the local network).
pub fn http_client(base: &str) -> anyhow::Result<reqwest::blocking::Client> {
    let mut b = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("kioku/{VERSION}"));
    if reqwest::Url::parse(base).is_ok_and(|u| crate::client::is_local_url(&u)) {
        b = b.no_proxy();
    }
    Ok(b.build()?)
}

/// The latest release tag: `HEAD <base>/latest` lands on `…/releases/tag/<tag>`.
pub fn latest_tag(http: &reqwest::blocking::Client, base: &str) -> anyhow::Result<String> {
    let resp = http
        .head(format!("{base}/latest"))
        .send()
        .with_context(|| format!("HEAD {base}/latest"))?;
    tag_from_release_url(resp.url().as_str())
        .with_context(|| format!("no published release found at {base}"))
}

/// The installer command that installs `tag` without writing to a directory kioku cannot
/// write to itself (the not-writable hint; SPEC-M2.5 §3.3 step 5 shows it too).
pub fn installer_line(tag: &str) -> String {
    if cfg!(windows) {
        format!(
            "& ([scriptblock]::Create((irm {}))) -Version {tag} -NoSetup",
            install_ps1_url()
        )
    } else {
        format!(
            "curl -fsSL {} | sh -s -- --version {tag} --no-setup",
            install_sh_url()
        )
    }
}

/// True when kioku can create files in `dir` (a probe file is created and removed).
pub fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".kioku-write-probe.{}", std::process::id()));
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Downloads release `tag` from `base`, verifies it and puts it in the place of `exe`;
/// returns the new binary's `--version` line. Nothing changes on any failure.
pub fn install_release(
    http: &reqwest::blocking::Client,
    base: &str,
    tag: &str,
    exe: &Path,
    verify: Verify,
) -> anyhow::Result<String> {
    let dir = exe.parent().context("binary has no parent directory")?;
    let new = if cfg!(windows) {
        sibling(exe, ".new")
    } else {
        dir.join(format!(".kioku.new.{}", std::process::id()))
    };
    if std::fs::write(&new, b"").is_err() {
        if cfg!(windows) {
            bail!(
                "{} is not writable; re-run the installer instead:\n  {}",
                dir.display(),
                installer_line(tag)
            );
        }
        bail!(
            "{} is not writable; re-run the installer instead (never with sudo):\n  {}",
            dir.display(),
            installer_line(tag)
        );
    }
    let work = dir.join(format!(".kioku-update.{}", std::process::id()));
    let result = replace(http, base, tag, exe, &new, &work, verify);
    let _ = std::fs::remove_file(&new);
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// `kioku update [--version <tag>] [--check] [--background] [--require-signature]`;
/// returns the exit code (10 = `--check` found a newer release).
pub fn run_update(args: UpdateArgs) -> anyhow::Result<i32> {
    if args.rollback {
        return run_rollback();
    }
    if args.background {
        return Ok(crate::auto_update::run_background(args.version));
    }
    let cfg = kioku_core::Config::load()?;
    let (base, warning) = release_base(&cfg)?;
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    let base = base.as_str();
    let http = http_client(base)?;
    let explicit = args.version.is_some();
    let tag = match args.version {
        Some(v) if v.starts_with('v') => v,
        Some(v) => format!("v{v}"),
        None => latest_tag(&http, base)?,
    };
    if args.check {
        println!("current: v{VERSION}\nlatest:  {tag} ({TARGET})");
        return Ok(if is_newer(&tag, VERSION) { 10 } else { 0 });
    }
    // A winget- or Homebrew-managed install (packaging/winget, packaging/homebrew):
    // replacing the binary behind the manager's back would leave it believing the old
    // version is installed (SPEC-M3.3 §1).
    if let Ok(exe) = std::env::current_exe()
        && let Some(pm) = package_manager(&exe)
    {
        println!("{}", managed_update_notice(pm, &tag));
        return Ok(0);
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
    let verify = Verify::manual(args.require_signature);
    let installed = install_release(&http, base, &tag, &exe, verify)?;
    println!("kioku: updated v{VERSION} -> {tag} ({installed})");
    for line in restart_service(&exe) {
        println!("{line}");
    }
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
    verify: Verify,
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
    // Order (SPEC-M2.7 §2): checksum → extract → signature → `--version` → swap. Nothing
    // from the archive runs before its signature is accepted.
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
    match verify.signature {
        SignaturePolicy::Skip => {}
        SignaturePolicy::Require => verify_signature(new)
            .context("refusing to install an unsigned or foreign binary; nothing was changed")?,
        SignaturePolicy::Warn => {
            if let Err(err) = verify_signature(new) {
                eprintln!(
                    "kioku: warning: {err:#} (installing anyway; pass --require-signature to refuse)"
                );
            }
        }
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
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !version_matches(&reported, tag) {
        if verify.exact_version {
            bail!("the new binary reports {reported:?}, not {tag}; nothing was changed");
        }
        eprintln!("kioku: warning: the new binary reports {reported:?}, not {tag}");
    }
    // SPEC-M2.7 §7: keep the binary being replaced as `<exe>.prev` for a rollback.
    keep_previous(exe);
    // Same directory: atomic; a running `kioku serve` keeps the old inode until restarted.
    // On Windows the running exe is renamed aside first (SPEC-M2.2 §5).
    swap_binary(new, exe, cfg!(windows))?;
    Ok(reported)
}

/// Suffix of the copy of the replaced binary kept for a rollback (SPEC-M2.7 §7).
pub const PREV_SUFFIX: &str = ".prev";

/// Copies `exe` to `<exe>.prev` (atomically: temp copy + rename; a running exe can be
/// copied on every platform). Best effort: a failure only costs the rollback.
pub fn keep_previous(exe: &Path) {
    if !exe.is_file() {
        return;
    }
    let prev = sibling(exe, PREV_SUFFIX);
    let tmp = sibling(exe, &format!("{PREV_SUFFIX}.{}.tmp", std::process::id()));
    let ok = std::fs::copy(exe, &tmp).is_ok() && std::fs::rename(&tmp, &prev).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The version `bin --version` reports (`0.8.0`), `None` when it does not run. On Windows a
/// copy with an `.exe` name is probed (`kioku.exe.prev` has no executable extension).
pub fn binary_version(bin: &Path) -> Option<String> {
    let probe = if cfg!(windows) {
        let p = bin.with_file_name(format!(".kioku-probe-{}.exe", std::process::id()));
        std::fs::copy(bin, &p).ok()?;
        p
    } else {
        bin.to_path_buf()
    };
    let out = output_quiet(&probe);
    if cfg!(windows) {
        let _ = std::fs::remove_file(&probe);
    }
    let out = out?;
    String::from_utf8_lossy(&out)
        .split_whitespace()
        .find(|w| {
            w.trim_start_matches('v')
                .starts_with(|c: char| c.is_ascii_digit())
        })
        .map(|w| w.trim_start_matches('v').to_string())
}

/// stdout of `bin --version` when it exits 0 (5 s at most).
fn output_quiet(bin: &Path) -> Option<Vec<u8>> {
    let mut cmd = kioku_core::util::quiet_command(&bin.display().to_string());
    cmd.arg("--version");
    let out = kioku_core::util::output_with_deadline(cmd, Duration::from_secs(5))?;
    out.status.success().then_some(out.stdout)
}

/// Puts `<exe>.prev` back in the place of `exe` (rename; Windows: the rename dance) and
/// returns the version it reports. Refuses when there is no `.prev` or it does not run.
pub fn rollback_binary(exe: &Path) -> anyhow::Result<String> {
    let prev = sibling(exe, PREV_SUFFIX);
    if !prev.is_file() {
        bail!(
            "there is no previous binary ({} is missing): nothing to roll back to",
            prev.display()
        );
    }
    let version = binary_version(&prev)
        .with_context(|| format!("{} does not run; refusing to roll back", prev.display()))?;
    swap_binary(&prev, exe, cfg!(windows))?;
    Ok(version)
}

/// `kioku update --rollback`: swap `<exe>.prev` back in and restart the service.
pub fn run_rollback() -> anyhow::Result<i32> {
    let exe = std::env::current_exe().context("locating the kioku binary")?;
    let exe = kioku_core::util::canonical_plain(&exe).unwrap_or(exe);
    if let Some(pm) = package_manager(&exe) {
        bail!(
            "kioku was installed with {0}; use {0} to install another version",
            pm.name()
        );
    }
    let version = rollback_binary(&exe)?;
    println!("kioku: rolled back v{VERSION} -> v{version}");
    for line in restart_service(&exe) {
        println!("{line}");
    }
    Ok(0)
}

/// Restarts the kioku service if one is installed (hooks need nothing: same path); returns
/// the report lines. A definition from before SPEC-M2.5 (no `KIOKU_SERVICE=1`) is rewritten
/// and reloaded instead, which is what enables the server's automatic updates (§3.1 step 5).
pub fn restart_service(exe: &Path) -> Vec<String> {
    let Ok(env) = SetupEnv::from_process(exe.display().to_string()) else {
        return Vec::new();
    };
    let Ok(cfg) = kioku_core::Config::load_from_dir(&env.config_dir(), &env.vars) else {
        return Vec::new();
    };
    restart_with(&env.service_manager(&cfg.data_dir))
}

/// [`restart_service`] for a given manager.
pub fn restart_with(manager: &crate::service::ServiceManager) -> Vec<String> {
    if !manager.is_installed() {
        return Vec::new();
    }
    let result = if manager.lacks_service_marker() {
        manager.install().map(|_| {
            format!(
                "kioku: rewrote and restarted the service ({}) so it can update itself",
                manager.describe()
            )
        })
    } else {
        manager
            .restart()
            .map(|_| format!("kioku: restarted the service ({})", manager.describe()))
    };
    match result {
        Ok(line) => vec![line],
        Err(err) => vec![format!(
            "kioku: warning: restart the service yourself (`kioku service start`): {err:#}"
        )],
    }
}

/// Test fixtures shared with [`crate::auto_update`]: a fake release server and release
/// archives holding a dummy "binary" (a shell script), never the test's own executable.
#[cfg(test)]
pub(crate) mod fixture {
    #[cfg(unix)] // only the archive builders use them
    use super::{Sha256, TARGET};
    #[cfg(unix)]
    use sha2::Digest;

    /// Serves `files` (path → body) on an ephemeral port; unknown paths are 404. With
    /// `latest`, `/releases/latest` redirects to `/releases/tag/<latest>` like GitHub.
    /// Returns the releases base URL (`http://127.0.0.1:<port>/releases`).
    pub(crate) fn serve(files: Vec<(String, Vec<u8>)>, latest: Option<&str>) -> String {
        use axum::http::{StatusCode, Uri, header};
        use axum::response::IntoResponse;
        let files = std::sync::Arc::new(files);
        let latest = latest.map(str::to_string);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = axum::Router::new().fallback(move |uri: Uri| {
                    let files = files.clone();
                    let latest = latest.clone();
                    async move {
                        let path = uri.path();
                        if path == "/releases/latest"
                            && let Some(tag) = latest
                        {
                            let to = format!("/releases/tag/{tag}");
                            return (StatusCode::FOUND, [(header::LOCATION, to)]).into_response();
                        }
                        if path.starts_with("/releases/tag/") {
                            return StatusCode::OK.into_response();
                        }
                        match files.iter().find(|(p, _)| p == path) {
                            Some((_, body)) => (StatusCode::OK, body.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
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

    /// A release `tag` whose dummy binary prints `kioku <reports>`: the archive's server path
    /// and bytes (unix: the dummy is a shell script).
    #[cfg(unix)]
    pub(crate) fn release(tag: &str, reports: &str) -> (String, Vec<u8>) {
        release_with_script(tag, &format!("#!/bin/sh\necho 'kioku {reports}'\n"))
    }

    /// A release `tag` whose dummy binary is `script` (unix).
    #[cfg(unix)]
    pub(crate) fn release_with_script(tag: &str, script: &str) -> (String, Vec<u8>) {
        let tmp = tempfile::tempdir().unwrap();
        let name = format!("kioku-{tag}-{TARGET}");
        std::fs::create_dir(tmp.path().join(&name)).unwrap();
        std::fs::write(tmp.path().join(&name).join("kioku"), script).unwrap();
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
        (
            format!("/releases/download/{tag}/{name}.tar.gz"),
            std::fs::read(&tarball).unwrap(),
        )
    }

    /// `SHA256SUMS` served for `tag`, with the right checksum of `bytes` or a wrong one.
    #[cfg(unix)]
    pub(crate) fn sums(tag: &str, bytes: &[u8], correct: bool) -> (String, Vec<u8>) {
        let hex = if correct {
            format!("{:x}", Sha256::digest(bytes))
        } else {
            "0".repeat(64)
        };
        (
            format!("/releases/download/{tag}/SHA256SUMS"),
            format!("{hex}  kioku-{tag}-{TARGET}.tar.gz\n").into_bytes(),
        )
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

    /// SPEC-M3.3 §1: a Homebrew keg is recognised; `kioku update` prints `brew upgrade`.
    #[test]
    fn brew_installs_are_recognised_and_get_the_brew_notice() {
        for p in [
            "/opt/homebrew/Cellar/kioku/0.9.3/bin/kioku",
            "/usr/local/Cellar/kioku/0.9.3/bin/kioku",
            "/home/linuxbrew/.linuxbrew/Cellar/kioku/0.9.3/bin/kioku",
        ] {
            assert!(is_brew_install(Path::new(p)), "{p}");
            assert_eq!(
                package_manager(Path::new(p)),
                Some(PackageManager::Homebrew)
            );
        }
        for p in [
            "/Users/me/.local/bin/kioku",
            "/opt/homebrew/Cellar/kiokuX/1/bin/kioku",
            "/opt/homebrew/bin/other",
        ] {
            assert!(!is_brew_install(Path::new(p)), "{p}");
            assert_eq!(package_manager(Path::new(p)), None, "{p}");
        }
        assert_eq!(
            managed_update_notice(PackageManager::Homebrew, "v0.9.3"),
            "kioku was installed with Homebrew; update it with: brew upgrade kioku (latest release: v0.9.3)"
        );
        assert_eq!(
            managed_update_notice(PackageManager::Winget, "v0.9.3"),
            "kioku was installed with winget; update it with: winget upgrade misorafa.kioku (latest release: v0.9.3)"
        );
    }

    /// A symlink into a keg (`<prefix>/bin/kioku`) counts as Homebrew; hooks and the service
    /// get the stable link instead of the versioned keg path.
    #[cfg(unix)]
    #[test]
    fn brew_links_are_followed_and_kept_stable() {
        let prefix = tempfile::tempdir().unwrap();
        let keg = prefix.path().join("Cellar/kioku/0.9.3/bin");
        std::fs::create_dir_all(&keg).unwrap();
        std::fs::write(keg.join("kioku"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(prefix.path().join("bin")).unwrap();
        let link = prefix.path().join("bin/kioku");
        std::os::unix::fs::symlink(keg.join("kioku"), &link).unwrap();
        assert_eq!(package_manager(&link), Some(PackageManager::Homebrew));
        assert_eq!(stable_binary_path(&keg.join("kioku")), link);
        // No link (yet): the keg path stays; other paths are untouched.
        std::fs::remove_file(&link).unwrap();
        assert_eq!(stable_binary_path(&keg.join("kioku")), keg.join("kioku"));
        let plain = Path::new("/Users/me/.local/bin/kioku");
        assert_eq!(stable_binary_path(plain), plain);
    }

    #[test]
    fn winget_installs_are_recognised() {
        assert!(is_winget_install(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\misorafa.kioku_Microsoft.Winget.Source_8wekyb3d8bbwe\kioku.exe"
        )));
        assert!(is_winget_install(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WinGet\Links\kioku.exe"
        )));
        assert!(!is_winget_install(Path::new(
            r"C:\Users\me\AppData\Local\Programs\kioku\kioku.exe"
        )));
        assert!(!is_winget_install(Path::new("/Users/me/.local/bin/kioku")));
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
        use fixture::{release, serve, sums};
        let tmp = tempfile::tempdir().unwrap();
        let (asset, bytes) = release("v9.9.9", "9.9.9");
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let bin_dir = tmp.path().join("bin dir");
        std::fs::create_dir(&bin_dir).unwrap();
        let exe = bin_dir.join("kioku");
        let run = |base: &str, tag: &str| {
            std::fs::write(&exe, "old").unwrap();
            let r = replace(
                &http,
                base,
                tag,
                &exe,
                &bin_dir.join(".kioku.new.1"),
                &bin_dir.join(".kioku-update.1"),
                Verify::unsigned(),
            );
            let _ = std::fs::remove_dir_all(bin_dir.join(".kioku-update.1"));
            (r, std::fs::read_to_string(&exe).unwrap())
        };

        let (r, content) = run(
            &serve(
                vec![
                    (asset.clone(), bytes.clone()),
                    sums("v9.9.9", &bytes, false),
                ],
                None,
            ),
            "v9.9.9",
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("checksum mismatch"));
        assert_eq!(content, "old");

        let (r, content) = run(&serve(vec![(asset.clone(), bytes.clone())], None), "v9.9.9");
        assert!(format!("{:#}", r.unwrap_err()).contains("no checksum"));
        assert_eq!(content, "old");

        // Pre-SHA256SUMS releases: `<asset>.sha256`.
        let good = format!(
            "{:x}  kioku-v9.9.9-{TARGET}.tar.gz\n",
            Sha256::digest(&bytes)
        );
        let (r, content) = run(
            &serve(
                vec![
                    (asset.clone(), bytes),
                    (format!("{asset}.sha256"), good.into()),
                ],
                None,
            ),
            "v9.9.9",
        );
        assert_eq!(r.unwrap(), "kioku 9.9.9");
        assert!(content.contains("kioku 9.9.9"));
        assert!(!bin_dir.join(".kioku.new.1").exists());
    }

    /// SPEC-M2.5 §4.4: a binary that reports another version than the tag is refused.
    #[cfg(unix)]
    #[test]
    fn replace_refuses_a_binary_reporting_another_version() {
        use fixture::{release, serve, sums};
        let tmp = tempfile::tempdir().unwrap();
        let (asset, bytes) = release("v9.9.9", "9.9.8");
        let base = serve(
            vec![(asset, bytes.clone()), sums("v9.9.9", &bytes, true)],
            None,
        );
        let exe = tmp.path().join("kioku");
        std::fs::write(&exe, "old").unwrap();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let err = install_release(&http, &base, "v9.9.9", &exe, Verify::unsigned()).unwrap_err();
        assert!(format!("{err:#}").contains("not v9.9.9"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        let left: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "no temporary files are left behind");
        // A manual update only warns (e.g. the Windows installer test re-tags a real binary).
        if !cfg!(target_os = "macos") {
            install_release(&http, &base, "v9.9.9", &exe, Verify::manual(false)).unwrap();
            assert!(std::fs::read_to_string(&exe).unwrap().contains("9.9.8"));
        }
    }

    /// SPEC-M2.5 §4.2 on macOS: the unsigned dummy is refused when a signature is required,
    /// and installed with a warning otherwise.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_requires_kiokus_signature_for_automatic_updates() {
        use fixture::{release, serve, sums};
        let tmp = tempfile::tempdir().unwrap();
        let (asset, bytes) = release("v9.9.9", "9.9.9");
        let base = serve(
            vec![(asset, bytes.clone()), sums("v9.9.9", &bytes, true)],
            None,
        );
        let exe = tmp.path().join("kioku");
        std::fs::write(&exe, "old").unwrap();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        assert_eq!(SignaturePolicy::automatic(), SignaturePolicy::Require);
        let err = install_release(&http, &base, "v9.9.9", &exe, Verify::automatic()).unwrap_err();
        assert!(
            format!("{err:#}").contains("unsigned or foreign"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        install_release(&http, &base, "v9.9.9", &exe, Verify::manual(false)).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("9.9.9"));
    }

    /// SPEC-M2.7 §2: an unsigned binary under `Require` is refused before it ever runs
    /// (the dummy writes a marker file when executed; the marker must not exist).
    #[cfg(target_os = "macos")]
    #[test]
    fn the_signature_is_checked_before_the_new_binary_runs() {
        use fixture::{release_with_script, serve, sums};
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let script = format!(
            "#!/bin/sh\ntouch '{}'\necho 'kioku 9.9.9'\n",
            marker.display()
        );
        let (asset, bytes) = release_with_script("v9.9.9", &script);
        let base = serve(
            vec![(asset, bytes.clone()), sums("v9.9.9", &bytes, true)],
            None,
        );
        let exe = tmp.path().join("kioku");
        std::fs::write(&exe, "old").unwrap();
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let verify = Verify {
            signature: SignaturePolicy::Require,
            exact_version: true,
        };
        let err = install_release(&http, &base, "v9.9.9", &exe, verify).unwrap_err();
        assert!(
            format!("{err:#}").contains("unsigned or foreign"),
            "{err:#}"
        );
        assert!(!marker.exists(), "the unverified binary was executed");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        // Without the signature requirement the probe runs it (the marker appears).
        install_release(&http, &base, "v9.9.9", &exe, Verify::unsigned()).unwrap();
        assert!(marker.exists());
    }

    /// SPEC-M2.7 §7: an update keeps the replaced binary as `.prev`; `--rollback` puts it
    /// back and refuses when there is none.
    #[cfg(unix)]
    #[test]
    fn updates_keep_prev_and_rollback_restores_it() {
        use fixture::{release, serve, sums};
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("kioku");
        assert!(
            format!("{:#}", rollback_binary(&exe).unwrap_err()).contains("nothing to roll back"),
            "no .prev → refused"
        );
        std::fs::write(&exe, "#!/bin/sh\necho 'kioku 9.9.8'\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (asset, bytes) = release("v9.9.9", "9.9.9");
        let base = serve(
            vec![(asset, bytes.clone()), sums("v9.9.9", &bytes, true)],
            None,
        );
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        install_release(&http, &base, "v9.9.9", &exe, Verify::unsigned()).unwrap();
        let prev = sibling(&exe, PREV_SUFFIX);
        assert_eq!(binary_version(&exe).as_deref(), Some("9.9.9"));
        assert_eq!(binary_version(&prev).as_deref(), Some("9.9.8"));
        assert_eq!(rollback_binary(&exe).unwrap(), "9.9.8");
        assert_eq!(binary_version(&exe).as_deref(), Some("9.9.8"));
        assert!(!prev.exists(), "the .prev was moved into place");
        // A .prev that does not run is refused, and nothing changes.
        std::fs::write(&prev, "not a program").unwrap();
        assert!(rollback_binary(&exe).is_err());
        assert_eq!(binary_version(&exe).as_deref(), Some("9.9.8"));
    }

    /// The real `codesign` on the test binary itself (never replaced, only inspected): it is
    /// not signed by kioku's team, so it fails verification.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_test_binary_is_not_signed_by_the_team() {
        let me = std::env::current_exe().unwrap();
        assert!(verify_signature(&me).is_err());
    }

    #[test]
    fn team_id_and_version_parsing() {
        // Captured `codesign -dv` output of a Developer ID-signed kioku.
        let signed = "Executable=/Users/me/.local/bin/kioku\n\
                      Identifier=kioku\n\
                      Format=Mach-O thin (arm64)\n\
                      CodeDirectory v=20500 size=51842 flags=0x10000(runtime) hashes=1609+7 location=embedded\n\
                      Signature size=9046\n\
                      Timestamp=30 Sep 2026 at 10:12:44\n\
                      Info.plist=not bound\n\
                      TeamIdentifier=7F6HLTW75D\n\
                      Runtime Version=15.0.0\n\
                      Sealed Resources=none\n\
                      Internal requirements count=1 size=168\n";
        assert_eq!(parse_team_id(signed).as_deref(), Some(APPLE_TEAM_ID));
        let adhoc = "Executable=/tmp/kioku\nSignature=adhoc\nTeamIdentifier=not set\n";
        assert_eq!(parse_team_id(adhoc).as_deref(), Some("not set"));
        assert_eq!(parse_team_id("code object is not signed at all"), None);

        assert!(version_matches("kioku 0.7.0", "v0.7.0"));
        assert!(version_matches("kioku v0.7.0\n", "0.7.0"));
        assert!(!version_matches("kioku 0.7.1", "v0.7.0"));
        assert!(!version_matches("kioku 0.7.0-rc1", "v0.7.0"));
        assert!(!version_matches("", "v0.7.0"));
    }

    #[test]
    fn signature_policies() {
        if cfg!(target_os = "macos") {
            assert_eq!(SignaturePolicy::automatic(), SignaturePolicy::Require);
            assert_eq!(SignaturePolicy::manual(false), SignaturePolicy::Warn);
            assert_eq!(SignaturePolicy::manual(true), SignaturePolicy::Require);
        } else {
            assert_eq!(SignaturePolicy::automatic(), SignaturePolicy::Skip);
            assert_eq!(SignaturePolicy::manual(true), SignaturePolicy::Skip);
            assert!(verify_signature(Path::new("/nonexistent")).is_ok());
        }
    }

    /// SPEC-M2.5 §3.1 step 5: a service definition without `KIOKU_SERVICE=1` is rewritten
    /// (and reloaded) by the restart after an update; a current one is just restarted.
    #[test]
    fn restart_rewrites_a_definition_without_the_marker() {
        use crate::service::{CmdOutput, Platform, Runner, ServiceManager, ServiceSpec};
        let home = tempfile::tempdir().unwrap();
        let runner = Runner::recording(|_| CmdOutput::ok(""));
        let m = ServiceManager::with_platform(
            Platform::Systemd,
            runner.clone(),
            home.path(),
            &[("USER".to_string(), "me".to_string())]
                .into_iter()
                .collect(),
            ServiceSpec {
                bin: "/home/me/.local/bin/kioku".into(),
                data_dir: home.path().join(".kioku"),
            },
        );
        assert!(restart_with(&m).is_empty(), "no service installed");
        let path = m.definition_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let current = m.render().unwrap();
        std::fs::write(&path, current.replace("Environment=KIOKU_SERVICE=1\n", "")).unwrap();
        let lines = restart_with(&m);
        assert!(lines[0].contains("rewrote"), "{lines:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
        let calls: Vec<String> = runner.calls().iter().map(|c| c.join(" ")).collect();
        assert!(
            calls.contains(&"systemctl --user daemon-reload".to_string()),
            "{calls:?}"
        );
        runner.clear_calls();
        let lines = restart_with(&m);
        assert!(lines[0].contains("restarted the service"), "{lines:?}");
        let calls: Vec<String> = runner.calls().iter().map(|c| c.join(" ")).collect();
        assert_eq!(calls, ["systemctl --user restart kioku.service"]);
    }

    /// SPEC-M2.7 §11: mirrors and forks only with `[update] allow_mirror = true`, and only
    /// over https (plain http on loopback for tests).
    #[test]
    fn release_base_needs_allow_mirror_and_https() {
        let vars = |pairs: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let off = kioku_core::UpdateConfig::default();
        let on = kioku_core::UpdateConfig {
            allow_mirror: true,
            ..Default::default()
        };
        let official = "https://github.com/misorafa/kioku/releases".to_string();
        assert_eq!(
            release_base_for(&off, &vars(&[])).unwrap(),
            (official.clone(), None)
        );
        let (base, warning) = release_base_for(
            &off,
            &vars(&[("KIOKU_DOWNLOAD_BASE", "https://evil.example/releases")]),
        )
        .unwrap();
        assert_eq!(base, official);
        assert!(warning.unwrap().contains("allow_mirror"));
        let (base, warning) =
            release_base_for(&off, &vars(&[("KIOKU_REPO", "someone/fork")])).unwrap();
        assert_eq!(base, official);
        assert!(warning.is_some());
        assert_eq!(
            release_base_for(
                &on,
                &vars(&[("KIOKU_DOWNLOAD_BASE", "https://mirror.example/kioku/")])
            )
            .unwrap(),
            ("https://mirror.example/kioku".to_string(), None)
        );
        assert_eq!(
            release_base_for(&on, &vars(&[("KIOKU_REPO", "someone/fork")]))
                .unwrap()
                .0,
            "https://github.com/someone/fork/releases"
        );
        assert_eq!(
            release_base_for(
                &on,
                &vars(&[("KIOKU_DOWNLOAD_BASE", "http://127.0.0.1:8080/releases")])
            )
            .unwrap()
            .0,
            "http://127.0.0.1:8080/releases"
        );
        for bad in [
            "http://192.168.1.5/releases",
            "ftp://x/releases",
            "file:///tmp/r",
        ] {
            assert!(
                release_base_for(&on, &vars(&[("KIOKU_DOWNLOAD_BASE", bad)])).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn latest_tag_follows_the_redirect() {
        let base = fixture::serve(Vec::new(), Some("v1.2.3"));
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        assert_eq!(latest_tag(&http, &base).unwrap(), "v1.2.3");
        let none = fixture::serve(Vec::new(), None);
        assert!(latest_tag(&http, &none).is_err());
        assert!(installer_line("v1.2.3").contains("v1.2.3"));
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_writable(tmp.path()));
        assert!(!dir_writable(&tmp.path().join("missing")));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}
