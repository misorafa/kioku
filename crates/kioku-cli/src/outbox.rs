//! Offline queue for observations whose delivery failed, and its replay (SPEC-M2.6 §3).
//!
//! The happy path never touches it: a hook POSTs once, exactly as before. Only a delivery
//! that failed with a transport error, a timeout or a 5xx — to a server known to
//! de-duplicate by `event_id` — is written here, and `kioku sync` (started detached after a
//! later hook) replays it. Queue files hold no credentials and are private (0600 on Unix).

use crate::client::{ApiClient, http_status};
use anyhow::Context;
use kioku_core::store::OFFLINE_REPLAY_SOURCE;
use kioku_core::util::{create_private_dir, generate_token, sha256_hex, write_private_file};
use kioku_core::{Config, NewObservation, SessionStartRequest};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const MAX_BYTES: u64 = 50 * 1024 * 1024;
const MAX_EVENTS: usize = 10_000;
/// Entries replayed per batch (one shared network deadline).
const BATCH: usize = 64;
/// Deadline of one replay batch.
const BATCH_DEADLINE: Duration = Duration::from_secs(30);
/// A replayed session that existed before is finalized only when its newest queued
/// observation is at least this old (its agent has most likely stopped).
const IDLE_FINALIZE: Duration = Duration::from_secs(30 * 60);
/// No new background sync within this time after the last spawn or failed replay.
const SYNC_BACKOFF: Duration = Duration::from_secs(60);
/// Marker: the server de-duplicates observations by `event_id`.
const DEDUP_MARKER: &str = "server-dedups";
/// Last replay error (sanitized); its mtime drives [`SYNC_BACKOFF`].
const LAST_ERROR: &str = "last-error.txt";
/// Touched when a background sync is spawned.
const SPAWNED: &str = "sync-spawned";

#[derive(Serialize, Deserialize)]
struct Entry {
    observation: NewObservation,
    session: SessionStartRequest,
}

/// Queue directory for this exact server URL; credentials are never part of its contents.
pub fn directory(cfg: &Config) -> PathBuf {
    cfg.data_dir
        .join("outbox")
        .join(sha256_hex(cfg.client.server_url.trim_end_matches('/')))
}

/// Where entries the server refused for good (4xx other than auth) are kept for inspection.
fn failed_dir(cfg: &Config) -> PathBuf {
    directory(cfg).join("failed")
}

/// Records whether the configured server de-duplicates observations (learnt from the
/// `sessions/start` response). Never fails: without the marker nothing is ever queued.
pub fn remember_dedup(cfg: &Config, dedup: bool) {
    let marker = directory(cfg).join(DEDUP_MARKER);
    if !dedup {
        let _ = std::fs::remove_file(marker);
        return;
    }
    if marker.exists() || !cfg.data_dir.is_absolute() {
        return;
    }
    let _ = create_private_dir(&directory(cfg)).and_then(|_| write_private_file(&marker, "1"));
}

/// True when the configured server is known to de-duplicate observations by `event_id`.
pub fn server_dedups(cfg: &Config) -> bool {
    directory(cfg).join(DEDUP_MARKER).is_file()
}

/// True for a failure worth queueing: no HTTP answer (connect error, timeout) or a 5xx.
pub fn is_transient(err: &anyhow::Error) -> bool {
    match http_status(err) {
        None => true,
        Some(code) => code >= 500,
    }
}

fn pending(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|e| e == "json") {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(result)
}

/// Number of queued observations.
pub fn count(cfg: &Config) -> anyhow::Result<usize> {
    Ok(pending(&directory(cfg))?.len())
}

/// Number of entries the server refused for good (kept in `failed/`).
pub fn failed_count(cfg: &Config) -> usize {
    pending(&failed_dir(cfg)).map(|p| p.len()).unwrap_or(0)
}

/// Latest replay error, sanitized for diagnostics.
pub fn last_error(cfg: &Config) -> Option<String> {
    std::fs::read_to_string(directory(cfg).join(LAST_ERROR)).ok()
}

/// Saves an observation whose direct delivery failed. `obs` is stored exactly as it was
/// sent (already sanitized), so a replay of a delivery whose response was lost carries the
/// same bytes and the server's receipt matches.
pub fn enqueue(
    cfg: &Config,
    obs: &NewObservation,
    session: SessionStartRequest,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        cfg.data_dir.is_absolute(),
        "outbox requires an absolute data directory"
    );
    anyhow::ensure!(
        obs.event_id.is_some(),
        "only observations with an event_id are queued"
    );
    let dir = directory(cfg);
    create_private_dir(&dir)?;
    let _enqueue = lock(&dir.join("enqueue.lock"), Duration::from_millis(250))?
        .context("outbox busy; enqueue deadline exceeded")?;
    let paths = pending(&dir)?;
    let size: u64 = paths
        .iter()
        .filter_map(|p| p.metadata().ok().map(|m| m.len()))
        .sum();
    let entry = Entry {
        observation: obs.clone(),
        session,
    };
    let text = serde_json::to_string(&entry)?;
    anyhow::ensure!(
        paths.len() < MAX_EVENTS && size + text.len() as u64 <= MAX_BYTES,
        "outbox full (50 MiB / 10000 events); run kioku sync; existing records retained"
    );
    let id = format!("{}-{}", kioku_core::util::new_id(), generate_token());
    let tmp = dir.join(format!(".{id}.tmp"));
    write_private_file(&tmp, &text)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)?
        .sync_all()?;
    let dest = dir.join(format!("{id}.json"));
    std::fs::rename(tmp, &dest)?;
    Ok(dest)
}

// OS locks are released on process exit, including crashes. Never unlink a lock file:
// another process may already be waiting on the same inode (Rust 1.89+, MSRV 1.91).
struct LockedFile(std::fs::File);
impl Drop for LockedFile {
    fn drop(&mut self) {
        // Explicit unlock also releases a descriptor briefly inherited by a concurrent
        // child spawn before its close-on-exec runs (notably git during finalization).
        let _ = self.0.unlock();
    }
}

fn lock(path: &Path, wait: Duration) -> anyhow::Result<Option<LockedFile>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    let until = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(LockedFile(file))),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// What a replay did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Observations the server now has.
    pub delivered: usize,
    /// Entries the server refused for good, moved to `failed/`.
    pub refused: usize,
}

/// Drains the queue in batches until it is empty, a delivery fails, or another worker holds
/// the queue.
pub fn flush(cfg: &Config) -> anyhow::Result<FlushReport> {
    let mut total = FlushReport::default();
    loop {
        let batch = flush_batch(cfg)?;
        total.delivered += batch.delivered;
        total.refused += batch.refused;
        if batch == FlushReport::default() || count(cfg)? == 0 {
            return Ok(total);
        }
    }
}

/// True for a refusal that retrying cannot fix (bad request, conflicting `event_id`, too
/// large). Authentication errors are not: a fixed token makes them succeed.
fn is_permanent(err: &anyhow::Error) -> bool {
    matches!(http_status(err), Some(400 | 404 | 409 | 413 | 422))
}

fn flush_batch(cfg: &Config) -> anyhow::Result<FlushReport> {
    let dir = directory(cfg);
    if pending(&dir)?.is_empty() {
        let _ = std::fs::remove_file(dir.join(LAST_ERROR));
        return Ok(FlushReport::default());
    }
    let Some(_lock) = lock(&dir.join("sync.lock"), Duration::ZERO)? else {
        return Ok(FlushReport::default());
    };
    let result = (|| -> anyhow::Result<FlushReport> {
        let client = ApiClient::new(&cfg.client, BATCH_DEADLINE)?;
        let health = client.get(&["health"], &[])?;
        anyhow::ensure!(
            health["observation_dedup"] == true,
            "server lacks durable delivery receipts; update the server before replay"
        );
        let mut report = FlushReport::default();
        let mut started = BTreeSet::new();
        let mut newest: BTreeMap<String, SystemTime> = BTreeMap::new();
        for path in pending(&dir)?.into_iter().take(BATCH) {
            let queued_at = path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or_else(|_| SystemTime::now());
            let entry = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok(serde_json::from_slice::<Entry>(&b)?));
            let mut entry = match entry {
                Ok(e) if e.observation.event_id.is_some() => e,
                _ => {
                    quarantine(cfg, &path);
                    report.refused += 1;
                    continue;
                }
            };
            match deliver(&client, &mut entry, &mut started) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&path);
                    report.delivered += 1;
                    let sid = entry.observation.session_id;
                    let at = newest.entry(sid).or_insert(queued_at);
                    *at = (*at).max(queued_at);
                }
                Err(e) if is_permanent(&e) => {
                    quarantine(cfg, &path);
                    report.refused += 1;
                }
                Err(e) => return Err(e),
            }
        }
        // Finalize only sessions nobody is working in: ones this replay had to create, or
        // whose newest queued observation is old. A live session is finalized by its own
        // Stop / SessionEnd hook.
        for (sid, at) in newest {
            let idle = at.elapsed().is_ok_and(|e| e >= IDLE_FINALIZE);
            if started.contains(&sid) || idle {
                client.post(
                    &["sessions", &sid, "finalize"],
                    &json!({"reason": OFFLINE_REPLAY_SOURCE}),
                )?;
            }
        }
        Ok(report)
    })();
    match &result {
        Ok(_) => {
            let _ = std::fs::remove_file(dir.join(LAST_ERROR));
        }
        Err(e) => {
            let _ = write_private_file(
                &dir.join(LAST_ERROR),
                &kioku_core::sanitize::redact(&format!("{e:#}")),
            );
        }
    }
    result
}

/// Delivers one entry, re-creating its session first when the server does not know it
/// (that start never consumes a handoff: `source = offline-replay`).
fn deliver(
    client: &ApiClient,
    entry: &mut Entry,
    started: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    let sid = entry.observation.session_id.clone();
    if !started.contains(&sid)
        && let Err(err) = client.get(&["sessions", &sid], &[])
    {
        if http_status(&err) != Some(404) {
            return Err(err);
        }
        entry.session.source = OFFLINE_REPLAY_SOURCE.into();
        client.post(
            &["sessions", "start"],
            &serde_json::to_value(&entry.session)?,
        )?;
        started.insert(sid);
    }
    client.post(
        &["observations"],
        &serde_json::to_value(&entry.observation)?,
    )?;
    Ok(())
}

/// Moves an entry the server will never accept out of the queue (kept for `kioku doctor`).
fn quarantine(cfg: &Config, path: &Path) {
    let dest = failed_dir(cfg);
    let moved = create_private_dir(&dest)
        .and_then(|_| std::fs::rename(path, dest.join(path.file_name().unwrap_or_default())));
    if moved.is_err() {
        let _ = std::fs::remove_file(path);
    }
}

/// True when a background sync should start now: something is queued and neither a spawn
/// nor a failed replay happened within [`SYNC_BACKOFF`].
fn sync_due(dir: &Path, now: SystemTime) -> bool {
    let recent = |name: &str| {
        std::fs::metadata(dir.join(name))
            .and_then(|m| m.modified())
            // A time after `now` (clock skew, or written just now) counts as recent.
            .is_ok_and(|t| now.duration_since(t).map_or(true, |d| d < SYNC_BACKOFF))
    };
    pending(dir).is_ok_and(|p| !p.is_empty()) && !recent(SPAWNED) && !recent(LAST_ERROR)
}

/// Starts `kioku sync` detached when [`sync_due`]; called after a hook delivered something.
pub fn spawn_sync_if_due(cfg: &Config) -> anyhow::Result<()> {
    let dir = directory(cfg);
    if !sync_due(&dir, SystemTime::now()) {
        return Ok(());
    }
    write_private_file(&dir.join(SPAWNED), "")?;
    crate::auto_update::spawn_detached(&["sync"]).context("starting background sync")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kioku_core::{ObservationKind, ProjectIdentity, SessionStatus, Store};
    use std::sync::{Arc, mpsc};

    struct Server {
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn serve(listener: std::net::TcpListener, store: Arc<Store>) -> Server {
        listener.set_nonblocking(true).unwrap();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let (ready, started) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let app = kioku_server::build_app(store, "queue-test-token".into());
                    ready.send(()).unwrap();
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = rx.await;
                        })
                        .await
                        .unwrap();
                });
        });
        started.recv().unwrap();
        Server {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    fn client_cfg(dir: &Path, listener: &std::net::TcpListener) -> Config {
        let mut cfg = Config::for_data_dir(dir);
        cfg.client.server_url = format!("http://{}", listener.local_addr().unwrap());
        cfg.client.auth_token = Some("queue-test-token".into());
        cfg
    }
    fn entry(session: &str, event: &str) -> (NewObservation, SessionStartRequest) {
        (
            NewObservation {
                event_id: Some(event.into()),
                session_id: session.into(),
                kind: ObservationKind::Prompt,
                ts: Some("2026-09-30T00:00:00Z".into()),
                payload: json!({"prompt":"日本語の引き継ぎ"}),
            },
            SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: "/repo".into(),
                source: "startup".into(),
                project: ProjectIdentity {
                    id: "offline-project".into(),
                    name: "記憶".into(),
                    root: "/repo".into(),
                    remote: None,
                },
                lane: Some("feature/検索".into()),
            },
        )
    }

    #[test]
    fn a_queued_entry_is_private_and_replays_once_into_a_recreated_session() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let client_dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = client_cfg(client_dir.path(), &listener);
        let (obs, req) = entry("offline-s1", "delivery-1");
        let path = enqueue(&cfg, &obs, req.clone()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("queue-test-token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _server = serve(listener, store.clone());
        let report = flush(&cfg).unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(count(&cfg).unwrap(), 0);
        assert_eq!(store.observations("offline-s1").unwrap().len(), 1);
        let session = store.session("offline-s1").unwrap();
        assert_eq!(
            session.status,
            SessionStatus::Finalized,
            "replay created it"
        );
        assert_eq!(session.lane.as_deref(), Some("feature/検索"));
        assert!(
            !store
                .search("引き継ぎ", &kioku_core::SearchScope::All, 3)
                .unwrap()
                .is_empty()
        );
        assert_eq!(flush(&cfg).unwrap(), FlushReport::default());
    }

    /// A delivery whose response was lost: the server has it, the replay sends the same
    /// bytes and the receipt answers with the original seq (no duplicate, no 409).
    #[test]
    fn a_lost_response_replays_without_a_duplicate() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let client_dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = client_cfg(client_dir.path(), &listener);
        let (obs, req) = entry("s-lost", "delivery-lost");
        store.start_session(&req).unwrap();
        store.add_observation(&obs).unwrap();
        enqueue(&cfg, &obs, req).unwrap();
        let _server = serve(listener, store.clone());
        assert_eq!(flush(&cfg).unwrap().delivered, 1);
        assert_eq!(store.observations("s-lost").unwrap().len(), 1);
    }

    /// Regression (review of ca8afe5): replay finalized sessions that were still in use.
    #[test]
    fn replay_never_finalizes_a_live_session() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let client_dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = client_cfg(client_dir.path(), &listener);
        let (obs, req) = entry("live", "delivery-live");
        store.start_session(&req).unwrap();
        enqueue(&cfg, &obs, req).unwrap();
        let _server = serve(listener, store.clone());
        assert_eq!(flush(&cfg).unwrap().delivered, 1);
        assert_eq!(store.session("live").unwrap().status, SessionStatus::Open);
    }

    /// Regression (review of ca8afe5): one permanently refused entry blocked the queue.
    #[test]
    fn refused_and_corrupt_entries_are_set_aside_and_the_rest_replays() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let client_dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = client_cfg(client_dir.path(), &listener);
        let (good, req) = entry("s-mixed", "delivery-good");
        store.start_session(&req).unwrap();
        // Same event_id, different content than what the server already has → 409.
        let (mut clash, _) = entry("s-mixed", "delivery-clash");
        store.add_observation(&clash).unwrap();
        clash.payload = json!({"prompt":"別の内容"});
        enqueue(&cfg, &clash, req.clone()).unwrap();
        std::fs::write(directory(&cfg).join("000-broken.json"), "incomplete").unwrap();
        enqueue(&cfg, &good, req).unwrap();
        let _server = serve(listener, store.clone());
        let report = flush(&cfg).unwrap();
        assert_eq!(
            report,
            FlushReport {
                delivered: 1,
                refused: 2
            }
        );
        assert_eq!(count(&cfg).unwrap(), 0);
        assert_eq!(failed_count(&cfg), 2);
        assert_eq!(store.observations("s-mixed").unwrap().len(), 2);
    }

    #[test]
    fn authentication_failure_keeps_the_queue_and_server_urls_are_isolated() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client_dir = tempfile::tempdir().unwrap();
        let mut cfg = client_cfg(client_dir.path(), &listener);
        cfg.client.auth_token = Some("wrong-token".into());
        let _server = serve(listener, store.clone());
        let (obs, req) = entry("s-auth", "delivery-auth");
        enqueue(&cfg, &obs, req).unwrap();
        assert!(flush(&cfg).is_err());
        assert_eq!(count(&cfg).unwrap(), 1);
        assert!(last_error(&cfg).is_some());
        let mut other = cfg.clone();
        other.client.server_url = "http://other.invalid:7391".into();
        assert_eq!(count(&other).unwrap(), 0);
        cfg.client.auth_token = Some("queue-test-token".into());
        assert_eq!(flush(&cfg).unwrap().delivered, 1);
        assert!(last_error(&cfg).is_none());
    }

    /// Regression (review of ca8afe5): every observation was queued (with git calls and an
    /// fsync) before sending, and a failed enqueue dropped an observation the server could
    /// have taken. Now the happy path is one POST and only failed deliveries are queued.
    #[test]
    fn hooks_queue_only_failed_deliveries_to_a_server_that_dedups() {
        let server_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(Config::for_data_dir(server_dir.path())).unwrap());
        let client_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".kioku.toml"),
            "project = \"offline-project\"\n",
        )
        .unwrap();
        // Bound but not answering yet: deliveries time out.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = client_cfg(client_dir.path(), &listener);
        cfg.client.timeout_ms = 300;
        let prompt = |text: &str| {
            let payload = json!({"session_id":"hook-s1", "cwd":repo.path().display().to_string(), "prompt":text});
            let out = crate::hook::run_hook(
                crate::event::HookEventKind::UserPromptSubmit,
                crate::event::Agent::ClaudeCode,
                &payload.to_string(),
                &cfg,
            );
            assert_eq!(out.exit_code, 0);
        };
        prompt("サーバーの対応が不明");
        assert_eq!(
            count(&cfg).unwrap(),
            0,
            "an old server's deliveries are never queued"
        );
        remember_dedup(&cfg, true);
        prompt("日本語の引き継ぎ password=hunter2");
        assert_eq!(count(&cfg).unwrap(), 1);
        let queued = std::fs::read_to_string(&pending(&directory(&cfg)).unwrap()[0]).unwrap();
        assert!(!queued.contains("hunter2"));

        let _server = serve(listener, store.clone());
        prompt("オンラインでの記録");
        assert_eq!(
            count(&cfg).unwrap(),
            1,
            "a delivered observation is never queued"
        );
        assert_eq!(flush(&cfg).unwrap().delivered, 1);
        let texts: Vec<String> = store
            .observations("hook-s1")
            .unwrap()
            .iter()
            .map(|o| o.payload.to_string())
            .collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts.iter().all(|t| !t.contains("hunter2")));
    }

    #[test]
    fn capacity_limits_and_locks_preserve_existing_records() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let dir = directory(&cfg);
        create_private_dir(&dir).unwrap();
        let existing = dir.join("old.json");
        let f = std::fs::File::create(&existing).unwrap();
        f.set_len(MAX_BYTES).unwrap();
        drop(f);
        let (obs, req) = entry("s", "e");
        assert!(
            enqueue(&cfg, &obs, req)
                .unwrap_err()
                .to_string()
                .contains("outbox full")
        );
        assert_eq!(existing.metadata().unwrap().len(), MAX_BYTES);
        let path = dir.join("lock-test");
        let held = lock(&path, Duration::ZERO).unwrap().unwrap();
        assert!(lock(&path, Duration::ZERO).unwrap().is_none());
        drop(held);
        assert!(lock(&path, Duration::ZERO).unwrap().is_some());
    }

    #[test]
    fn background_sync_backs_off_after_a_spawn_or_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        assert!(!sync_due(dir, now), "nothing queued");
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        assert!(sync_due(dir, now));
        std::fs::write(dir.join(SPAWNED), "").unwrap();
        assert!(!sync_due(dir, now));
        assert!(sync_due(dir, now + SYNC_BACKOFF + Duration::from_secs(1)));
        std::fs::remove_file(dir.join(SPAWNED)).unwrap();
        std::fs::write(dir.join(LAST_ERROR), "x").unwrap();
        assert!(!sync_due(dir, now));
    }

    #[test]
    fn the_dedup_marker_follows_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        assert!(!server_dedups(&cfg));
        remember_dedup(&cfg, true);
        assert!(server_dedups(&cfg));
        remember_dedup(&cfg, false);
        assert!(!server_dedups(&cfg));
    }
}
