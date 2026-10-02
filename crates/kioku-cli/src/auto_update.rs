//! Automatic updates (SPEC-M2.5): the server follows GitHub releases, clients follow the
//! server. Owns the state file `<kioku dir>/state/auto-update.json` and its lock, the
//! client's SessionStart decision (update in a detached `kioku update --background`, or a
//! notice line in the `<kioku>` block), the background updater itself, and the daily check
//! task of a `kioku serve` running under the service manager. Download, verification and
//! the binary swap are [`crate::update`]'s.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use kioku_core::strings::Lang;
use kioku_core::util::{now, now_ts, one_line, parse_ts};
use kioku_core::{Config, VERSION};
use kioku_server::SharedUpdateStatus;
use serde::{Deserialize, Serialize};

use crate::event::HookEnv;
use crate::hook::{append_line, client_state_root};
use crate::update::{
    Verify, http_client, install_release, installer_line, is_newer, is_winget_install, latest_tag,
    release_base,
};

/// State file inside `<kioku dir>/state/`.
pub const STATE_FILE: &str = "auto-update.json";
/// Lock file inside `<kioku dir>/state/` (create-exclusive; one updater at a time).
pub const LOCK_FILE: &str = "auto-update.lock";
/// A lock older than this is abandoned (a crashed updater) and taken over.
pub const LOCK_STALE: Duration = Duration::from_secs(15 * 60);
/// At most one attempt per target version in this window (§3.3 step 2).
pub const ATTEMPT_THROTTLE: Duration = Duration::from_secs(6 * 3600);
/// The "update available" notice is shown at most this often per machine (§3.3 step 5).
pub const NOTICE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// `kioku doctor` warns when server and client versions differ for longer (§3.4).
pub const MISMATCH_WARN: Duration = Duration::from_secs(24 * 3600);
/// The server's first check after start (§3.1 step 1).
pub const FIRST_CHECK: Duration = Duration::from_secs(3 * 60);
/// The server's retention run interval (SPEC-M2.8 §3: daily; ± an eighth of jitter).
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// The server's first retention run: 10 minutes after the first update check (SPEC-M2.8
/// §3), then every [`PRUNE_INTERVAL`].
pub const FIRST_PRUNE: Duration = Duration::from_secs(FIRST_CHECK.as_secs() + 10 * 60);
/// Client-side log of automatic updates, in `<kioku dir>/logs/`.
pub const UPDATE_LOG: &str = "update.log";
/// `update.log` is rotated once it would exceed this.
pub const UPDATE_LOG_MAX_BYTES: u64 = 256 * 1024;

// ---------------------------------------------------------------------------------------
// State file and lock
// ---------------------------------------------------------------------------------------

/// `auto-update.json`: throttle, notice and result bookkeeping shared by the hook, the
/// background updater, the server task and `kioku doctor`. Times are RFC 3339.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUpdateState {
    /// Tag of the last attempted update (`v0.7.0`).
    pub target: Option<String>,
    /// When that attempt was started.
    pub last_attempt: Option<String>,
    /// Error of the last attempt (`None` after a successful one).
    pub last_error: Option<String>,
    /// When the "update available" notice was last shown.
    pub last_notice: Option<String>,
    /// Last successful automatic update: `auto-updated vA -> vB (client|server) at <time>`.
    pub last_result: Option<String>,
    /// Since when the client has seen a server of another version (cleared when equal).
    pub mismatch_since: Option<String>,
    /// Server: starts of a version that did not survive [`BOOT_OK_AFTER`] (SPEC-M2.7 §7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_failures: Option<BootFailures>,
    /// Server: a tag the automatic rollback moved away from; the update task does not
    /// install it again (a newer tag clears it; SPEC-M2.8 §9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_tag: Option<String>,
}

/// Consecutive starts of one server version that did not run for [`BOOT_OK_AFTER`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootFailures {
    /// Server version (`X.Y.Z`).
    pub version: String,
    /// Starts counted so far (the current one included while it has not survived).
    pub count: u32,
    /// Time of the last counted start (RFC 3339).
    pub last: Option<String>,
}

/// After this many starts that did not survive, a managed server rolls back to `<exe>.prev`.
pub const BOOT_FAILURE_LIMIT: u32 = 3;
/// A start that keeps running this long counts as a success (the counter is reset).
pub const BOOT_OK_AFTER: Duration = Duration::from_secs(60);

/// What a managed `kioku serve` does when it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootDecision {
    /// Run, with this counter recorded (reset after [`BOOT_OK_AFTER`]).
    Run(BootFailures),
    /// [`BOOT_FAILURE_LIMIT`] starts of this version failed: roll back if possible.
    RollBack,
}

/// The boot-failure state machine (SPEC-M2.7 §7): `recorded` is the counter on disk,
/// `running` this binary's version.
pub fn boot_decision(recorded: Option<&BootFailures>, running: &str, now: &str) -> BootDecision {
    let count = match recorded {
        Some(b) if b.version == running && b.count >= BOOT_FAILURE_LIMIT => {
            return BootDecision::RollBack;
        }
        Some(b) if b.version == running => b.count + 1,
        _ => 1,
    };
    BootDecision::Run(BootFailures {
        version: running.to_string(),
        count,
        last: Some(now.to_string()),
    })
}

/// Start of a managed server: counts this start, or — after [`BOOT_FAILURE_LIMIT`] failed
/// starts of `running` — puts `<exe>.prev` back when it reports an older version. Returns
/// that version after a rollback (the caller exits 75 so the manager starts it).
pub fn boot_check(exe: &Path, state_dir: &Path, running: &str) -> Option<String> {
    let mut state = AutoUpdateState::load(state_dir);
    match boot_decision(state.boot_failures.as_ref(), running, &now_ts()) {
        BootDecision::Run(b) => {
            state.boot_failures = Some(b);
            let _ = state.save(state_dir);
            None
        }
        BootDecision::RollBack => {
            let prev = crate::update::sibling(exe, crate::update::PREV_SUFFIX);
            let older = prev.is_file()
                && crate::update::binary_version(&prev).is_some_and(|v| is_newer(running, &v));
            let rolled = if older {
                crate::update::rollback_binary(exe)
            } else {
                Err(anyhow::anyhow!(
                    "no older {} to roll back to",
                    prev.display()
                ))
            };
            match rolled {
                Ok(version) => {
                    let msg = format!(
                        "kioku: rolled back to v{version} after {BOOT_FAILURE_LIMIT} failed starts of v{running}"
                    );
                    state.boot_failures = None;
                    state.last_error = Some(msg);
                    // Do not install the failing release again (SPEC-M2.8 §9).
                    state.skip_tag = Some(format!("v{}", running.trim_start_matches('v')));
                    let _ = state.save(state_dir);
                    Some(version)
                }
                Err(err) => {
                    // Keep running and counting; the manager's own backoff applies.
                    tracing::warn!(
                        error = format!("{err:#}"),
                        "repeated failed starts, but no rollback is possible"
                    );
                    if let Some(b) = state.boot_failures.as_mut() {
                        b.count += 1;
                        b.last = Some(now_ts());
                    }
                    let _ = state.save(state_dir);
                    None
                }
            }
        }
    }
}

/// A managed server survived [`BOOT_OK_AFTER`]: forget the failed starts of `running`.
pub fn boot_succeeded(state_dir: &Path, running: &str) {
    AutoUpdateState::update(state_dir, |s| {
        if s.boot_failures
            .as_ref()
            .is_some_and(|b| b.version == running)
        {
            s.boot_failures = None;
        }
    });
}

impl AutoUpdateState {
    /// Reads `<dir>/auto-update.json`; missing or unreadable → defaults.
    pub fn load(dir: &Path) -> AutoUpdateState {
        std::fs::read_to_string(dir.join(STATE_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Writes `<dir>/auto-update.json` atomically (temp file + rename).
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        kioku_core::util::create_private_dir(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(STATE_FILE))
            .with_context(|| format!("writing {}", dir.join(STATE_FILE).display()))
    }

    /// Applies `change` to the file's current content and saves it (best effort).
    pub fn update(dir: &Path, change: impl FnOnce(&mut AutoUpdateState)) {
        let mut state = AutoUpdateState::load(dir);
        change(&mut state);
        let _ = state.save(dir);
    }
}

/// Current time in unix seconds (the clock of every throttle).
pub fn unix_now() -> i64 {
    now().timestamp()
}

/// True when RFC 3339 time `ts` lies less than `window` before `at` (unix seconds).
pub fn within(ts: Option<&str>, window: Duration, at: i64) -> bool {
    ts.and_then(parse_ts)
        .is_some_and(|t| at - t.timestamp() < window.as_secs() as i64)
}

/// Held while an updater runs; removing the file on drop releases it.
#[derive(Debug)]
pub struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    /// Takes `<dir>/auto-update.lock` (create-exclusive). `Ok(None)` when another updater
    /// holds it; a lock older than [`LOCK_STALE`] is taken over.
    pub fn acquire(dir: &Path) -> anyhow::Result<Option<UpdateLock>> {
        kioku_core::util::create_private_dir(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(LOCK_FILE);
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{} {}", std::process::id(), now_ts());
                    return Ok(Some(UpdateLock { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age >= LOCK_STALE);
                    if !stale {
                        return Ok(None);
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        Ok(None)
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------
// Client (SessionStart hook)
// ---------------------------------------------------------------------------------------

/// True for a stable release tag or version (no `-`: no pre-release).
pub fn is_stable(tag: &str) -> bool {
    !tag.contains('-')
}

/// What the SessionStart hook does about the server's version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientAction {
    /// Nothing (same version, older server, pre-release, throttled, notice already shown).
    Nothing,
    /// Start `kioku update --version <tag> --background`.
    Spawn(String),
    /// Add this line to the `<kioku>` block.
    Notice(String),
}

/// What the client decision depends on besides the state file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientFacts {
    /// `server_version` from `POST /sessions/start` (`None` from an older server).
    pub server_version: Option<String>,
    /// This binary's version.
    pub client_version: String,
    /// Effective `[update] auto`.
    pub auto: bool,
    /// The binary is managed by winget.
    pub winget: bool,
    /// kioku can write to its binary's directory.
    pub writable: bool,
    /// `[client] lang` (the notice line is bilingual).
    pub lang: Lang,
}

/// The notice line: `kioku v<server> is available (you have v<client>): <how>`.
pub fn notice_line(lang: Lang, server: &str, client: &str, how: &str) -> String {
    match lang {
        Lang::Ja => format!("kioku v{server} が利用できます（この端末は v{client}）: {how}"),
        Lang::En => format!("kioku v{server} is available (you have v{client}): {how}"),
    }
}

/// How to update by hand: winget, the installer (binary directory not writable), or
/// `kioku update` (automatic updates turned off).
pub fn how_to_update(facts: &ClientFacts, tag: &str) -> String {
    if facts.winget {
        "winget upgrade misorafa.kioku".to_string()
    } else if !facts.writable {
        installer_line(tag)
    } else {
        "kioku update".to_string()
    }
}

/// The client rule of SPEC-M2.5 §3.3: follow a strictly newer, stable server — in the
/// background when possible (once per target per [`ATTEMPT_THROTTLE`]), else with a notice
/// (once per [`NOTICE_INTERVAL`]). Never downgrades.
pub fn client_action(facts: &ClientFacts, state: &AutoUpdateState, at: i64) -> ClientAction {
    let Some(server) = facts.server_version.as_deref() else {
        return ClientAction::Nothing;
    };
    let server = server.trim_start_matches('v');
    if !is_stable(server) || !is_newer(server, &facts.client_version) {
        return ClientAction::Nothing;
    }
    let tag = format!("v{server}");
    if facts.auto && !facts.winget && facts.writable {
        let throttled = state.target.as_deref() == Some(tag.as_str())
            && within(state.last_attempt.as_deref(), ATTEMPT_THROTTLE, at);
        return if throttled {
            ClientAction::Nothing
        } else {
            ClientAction::Spawn(tag)
        };
    }
    if within(state.last_notice.as_deref(), NOTICE_INTERVAL, at) {
        return ClientAction::Nothing;
    }
    ClientAction::Notice(notice_line(
        facts.lang,
        server,
        &facts.client_version,
        &how_to_update(facts, &tag),
    ))
}

/// `<kioku dir>/state` of this machine (client side).
pub fn state_dir(cfg: &Config, env: &HookEnv) -> Option<PathBuf> {
    client_state_root(cfg, env).map(|d| d.join("state"))
}

/// The running kioku binary (plain path).
fn current_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(kioku_core::util::canonical_plain(&exe).unwrap_or(exe))
}

/// SessionStart side effects of the server's version: records the version mismatch, then
/// decides per [`client_action`] and records the attempt / notice. Returns the notice line
/// and the tag to update to. Never fails (the hook must not).
pub fn after_session_start(
    cfg: &Config,
    env: &HookEnv,
    server_version: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(server) = server_version else {
        return (None, None);
    };
    let Some(dir) = state_dir(cfg, env) else {
        return (None, None);
    };
    let at = unix_now();
    let mut state = AutoUpdateState::load(&dir);
    let before = state.clone();
    if server.trim_start_matches('v') == VERSION {
        state.mismatch_since = None;
    } else if state.mismatch_since.is_none() {
        state.mismatch_since = Some(now_ts());
    }
    let mut result = (None, None);
    if is_stable(server) && is_newer(server, VERSION) {
        let exe = current_binary();
        let facts = ClientFacts {
            server_version: Some(server.to_string()),
            client_version: VERSION.to_string(),
            auto: cfg.update.auto,
            winget: exe.as_deref().is_some_and(is_winget_install),
            writable: exe
                .as_deref()
                .and_then(Path::parent)
                .is_some_and(crate::update::dir_writable),
            lang: cfg.client.lang,
        };
        match client_action(&facts, &state, at) {
            ClientAction::Nothing => {}
            ClientAction::Spawn(tag) => {
                state.target = Some(tag.clone());
                state.last_attempt = Some(now_ts());
                result.1 = Some(tag);
            }
            ClientAction::Notice(line) => {
                state.last_notice = Some(now_ts());
                result.0 = Some(line);
            }
        }
    }
    if state != before {
        let _ = state.save(&dir);
    }
    result
}

/// `block` with `line` added as the last line inside `<kioku>…</kioku>`.
pub fn with_notice(block: &str, line: &str) -> String {
    match block.strip_suffix("</kioku>\n") {
        Some(body) => format!("{body}{line}\n</kioku>\n"),
        None => format!("{block}{line}\n"),
    }
}

/// Starts `kioku update --version <tag> --background` detached from the hook, the agent
/// and their console: a new process group with null stdio on Unix, `DETACHED_PROCESS |
/// CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on Windows. Not waited for.
pub fn spawn_background(tag: &str) -> anyhow::Result<()> {
    spawn_detached(&["update", "--version", tag, "--background"])
        .context("starting kioku update --background")
}

/// Starts `kioku <args>` detached from the hook, the agent and their console (see
/// [`spawn_background`]). Shared with the offline-queue replay (`kioku sync`).
pub fn spawn_detached(args: &[&str]) -> anyhow::Result<()> {
    let exe = current_binary().context("locating the kioku binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn()?;
    Ok(())
}

/// Appends `line` (timestamped) to `<kioku dir>/logs/update.log`.
fn log_update(root: Option<&Path>, line: &str) {
    if let Some(root) = root {
        let path = root.join("logs").join(UPDATE_LOG);
        let _ = append_line(
            &path,
            &format!("{} {}\n", now_ts(), one_line(line)),
            UPDATE_LOG_MAX_BYTES,
            false,
        );
    }
}

/// `kioku update --version <tag> --background`: nothing on stdout; the result goes to
/// `logs/update.log` and `auto-update.json`. Takes the lock (another updater running →
/// exit 0), never downgrades, and requires kioku's signature on macOS. Returns the exit code.
pub fn run_background(version: Option<String>) -> i32 {
    let env = HookEnv::from_process();
    let cfg = Config::load().unwrap_or_else(|_| {
        Config::for_data_dir(
            &env.home
                .clone()
                .unwrap_or_else(kioku_core::util::home_dir)
                .join(".kioku"),
        )
    });
    let root = client_state_root(&cfg, &env);
    let Some(dir) = root.as_ref().map(|r| r.join("state")) else {
        return 1;
    };
    let _lock = match UpdateLock::acquire(&dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return 0,
        Err(err) => {
            log_update(root.as_deref(), &format!("kioku: auto-update: {err:#}"));
            return 1;
        }
    };
    match background_update(&cfg, root.as_deref(), version) {
        Ok(None) => 0,
        Ok(Some((tag, lines))) => {
            let line = format!("kioku: auto-updated v{VERSION} -> {tag} (client)");
            log_update(root.as_deref(), &line);
            for l in &lines {
                log_update(root.as_deref(), l);
            }
            AutoUpdateState::update(&dir, |s| {
                s.target = Some(tag.clone());
                s.last_error = None;
                s.last_result = Some(format!(
                    "auto-updated v{VERSION} -> {tag} (client) at {}",
                    now_ts()
                ));
            });
            0
        }
        Err(err) => {
            let msg = one_line(&format!("{err:#}"));
            log_update(
                root.as_deref(),
                &format!("kioku: auto-update failed: {msg}"),
            );
            AutoUpdateState::update(&dir, |s| s.last_error = Some(msg.clone()));
            1
        }
    }
}

/// The work of [`run_background`]: `Ok(None)` when there is nothing to do, else the
/// installed tag and the service restart report.
fn background_update(
    cfg: &Config,
    root: Option<&Path>,
    version: Option<String>,
) -> anyhow::Result<Option<(String, Vec<String>)>> {
    let (base, warning) = release_base(cfg)?;
    if let Some(w) = warning {
        log_update(root, &w);
    }
    let http = http_client(&base)?;
    let tag = match version {
        Some(v) if v.starts_with('v') => v,
        Some(v) => format!("v{v}"),
        None => latest_tag(&http, &base)?,
    };
    if !is_stable(&tag) || !is_newer(&tag, VERSION) {
        return Ok(None);
    }
    let exe = current_binary().context("locating the kioku binary")?;
    if is_winget_install(&exe) {
        anyhow::bail!("installed with winget; update with: winget upgrade misorafa.kioku");
    }
    install_release(&http, &base, &tag, &exe, Verify::automatic())?;
    Ok(Some((tag, crate::update::restart_service(&exe))))
}

// ---------------------------------------------------------------------------------------
// Server (the check task inside `kioku serve`)
// ---------------------------------------------------------------------------------------

/// What the server's check needs.
#[derive(Clone, Debug)]
pub struct ServerCheck {
    /// Releases base URL ([`release_base`]).
    pub base: String,
    /// The binary to replace.
    pub exe: PathBuf,
    /// Running version (`X.Y.Z`).
    pub current: String,
    /// Effective `[update] auto`.
    pub auto: bool,
    /// Checks of the new binary ([`Verify::automatic`] in production).
    pub verify: Verify,
    /// `<data dir>/state` for the result bookkeeping (`kioku doctor`).
    pub state_dir: Option<PathBuf>,
}

/// Result of one server check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerOutcome {
    /// Nothing newer (or only a pre-release).
    UpToDate,
    /// A newer release exists but `auto` is off.
    Available(String),
    /// The newest release is the one an automatic rollback moved away from
    /// (`skip_tag`); it is not installed again.
    Skipped(String),
    /// The binary was replaced; the server must exit 75.
    Updated {
        /// Old version (`X.Y.Z`).
        from: String,
        /// New tag.
        to: String,
    },
    /// Check or update failed (retried at the next interval).
    Failed(String),
}

/// One check (blocking): resolve the latest stable release and, when newer and `auto` is
/// on, install it over `check.exe`. Records everything in `status`.
pub fn server_check_once(check: &ServerCheck, status: &SharedUpdateStatus) -> ServerOutcome {
    let fail = |msg: String| {
        status.lock().last_error = Some(msg.clone());
        ServerOutcome::Failed(msg)
    };
    let http = match http_client(&check.base) {
        Ok(h) => h,
        Err(err) => return fail(format!("{err:#}")),
    };
    let tag = latest_tag(&http, &check.base);
    {
        let mut s = status.lock();
        s.last_check = Some(now_ts());
        s.auto = check.auto;
    }
    let tag = match tag {
        Ok(t) => t,
        Err(err) => return fail(format!("{err:#}")),
    };
    if !is_stable(&tag) {
        status.lock().last_error = None;
        return ServerOutcome::UpToDate;
    }
    status.lock().latest_seen = Some(tag.clone());
    if !is_newer(&tag, &check.current) {
        status.lock().last_error = None;
        return ServerOutcome::UpToDate;
    }
    if !check.auto {
        status.lock().last_error = None;
        return ServerOutcome::Available(tag);
    }
    if let Some(dir) = &check.state_dir {
        // SPEC-M2.8 §9: never reinstall the release a rollback moved away from; a newer
        // release clears the mark.
        let state = AutoUpdateState::load(dir);
        match state.skip_tag.as_deref() {
            Some(skip) if skip == tag => {
                status.lock().last_error = None;
                return ServerOutcome::Skipped(tag);
            }
            Some(skip) if is_newer(&tag, skip) => {
                AutoUpdateState::update(dir, |s| s.skip_tag = None);
            }
            _ => {}
        }
        AutoUpdateState::update(dir, |s| {
            s.target = Some(tag.clone());
            s.last_attempt = Some(now_ts());
        });
    }
    match install_release(&http, &check.base, &tag, &check.exe, check.verify) {
        Ok(_) => {
            status.lock().last_error = None;
            if let Some(dir) = &check.state_dir {
                AutoUpdateState::update(dir, |s| {
                    s.last_error = None;
                    s.last_result = Some(format!(
                        "auto-updated v{} -> {tag} (server) at {}",
                        check.current,
                        now_ts()
                    ));
                });
            }
            ServerOutcome::Updated {
                from: check.current.clone(),
                to: tag,
            }
        }
        Err(err) => {
            let msg = one_line(&format!("{err:#}"));
            if let Some(dir) = &check.state_dir {
                AutoUpdateState::update(dir, |s| s.last_error = Some(msg.clone()));
            }
            fail(msg)
        }
    }
}

/// `base` ± up to an eighth of it (from the clock and pid; no RNG crate), so several
/// servers do not all ask GitHub at the same second.
pub fn next_interval(base: Duration) -> Duration {
    let jitter = (base.as_secs() / 8).max(1);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ (u64::from(std::process::id())).wrapping_mul(2_654_435_761);
    let span = 2 * jitter + 1;
    let offset = (mixed % span) as i64 - jitter as i64;
    Duration::from_secs((base.as_secs() as i64 + offset) as u64)
}

/// The server's update loop: first check after `first`, then every `interval` (±
/// [`next_interval`] jitter; `[update] interval_hours`). After a successful update it
/// sets `shutdown` to `true` and returns (the caller exits 75); with `auto` off it logs
/// each newly seen release once.
pub async fn server_update_task(
    check: ServerCheck,
    status: SharedUpdateStatus,
    shutdown: tokio::sync::watch::Sender<bool>,
    first: Duration,
    interval: Duration,
) {
    tokio::time::sleep(first).await;
    let mut announced: Option<String> = None;
    loop {
        let (c, st) = (check.clone(), status.clone());
        let outcome = tokio::task::spawn_blocking(move || server_check_once(&c, &st))
            .await
            .unwrap_or_else(|e| ServerOutcome::Failed(format!("update check panicked: {e}")));
        match outcome {
            ServerOutcome::Updated { from, to } => {
                tracing::info!("kioku: auto-updated v{from} -> {to} (server)");
                let _ = shutdown.send(true);
                return;
            }
            ServerOutcome::Skipped(tag) => {
                if announced.as_deref() != Some(tag.as_str()) {
                    tracing::warn!(
                        "kioku {tag} is not installed again: the server rolled back from it after failed starts; a newer release will be"
                    );
                    announced = Some(tag);
                }
            }
            ServerOutcome::Available(tag) => {
                if announced.as_deref() != Some(tag.as_str()) {
                    tracing::info!(
                        "kioku {tag} is available (running v{}); automatic updates are off: run `kioku update`",
                        check.current
                    );
                    announced = Some(tag);
                }
            }
            ServerOutcome::Failed(err) => {
                tracing::warn!(error = %err, "automatic update check failed; retrying at the next interval");
            }
            ServerOutcome::UpToDate => tracing::debug!("kioku is up to date"),
        }
        tokio::time::sleep(next_interval(interval)).await;
    }
}

/// The server's daily retention run (SPEC-M2.8 §3): `Store::prune` on the blocking pool
/// after `first`, then every [`PRUNE_INTERVAL`]. Started by `kioku serve` unless
/// `[retention] auto = false`; failures are logged and retried at the next interval.
pub async fn prune_task(store: std::sync::Arc<kioku_core::Store>, first: Duration) {
    tokio::time::sleep(first).await;
    loop {
        let s = store.clone();
        match tokio::task::spawn_blocking(move || s.prune(false)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(error = format!("{e:#}"), "daily prune failed"),
            Err(e) => tracing::warn!(error = %e, "daily prune panicked"),
        }
        tokio::time::sleep(next_interval(PRUNE_INTERVAL)).await;
    }
}

#[cfg(test)]
mod tests;
