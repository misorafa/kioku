//! `Store`: the synchronous facade over wiki files, git, SQLite and the tantivy index.
//!
//! Every method blocks (filesystem, SQLite, tantivy, git); async callers must run them in
//! `tokio::task::spawn_blocking`. Page writes, finalize and reindex are serialized by one
//! write lock; lock order is always write lock → db → index writer.

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod finalize_tests;
mod maintenance;
mod reliability;
pub use maintenance::{
    ForgetReport, PruneCount, PruneReport, StorageReport, SweepReport, dir_bytes,
    purge_history_commands,
};
pub use reliability::{
    BACKUP_FORMAT, BackupManifest, ReliabilityReport, WIKI_BUNDLE, restore_backup,
};

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::carry::{
    CARRY_HANDOFFS, Carried, LAST_REPLY_MAX, MAX_PINNED, PINNED_EXCERPT, PINNED_TAG, SourceHandoff,
    carry, handoff_items,
};
use crate::config::Config;
use crate::db::{self, PageRow, ProjectAlias, ProjectRow};
use crate::digest::{SessionDigest, aggregate_files};
use crate::error::{Error, Result};
use crate::git::Git;
use crate::handoff::{Handoff, HandoffInput, HandoffSource, PendingHandoff, render_agent_handoff};
use crate::index::{Hit, INDEX_SCHEMA_VERSION, IndexDoc, SearchIndex, SearchScope};
use crate::layout::DataDir;
use crate::page::{
    Frontmatter, GLOBAL_DIR, Page, PageKind, PageScope, resolve_write_path, validate_rel_path,
};
use crate::project::{
    ProjectIdentity, comparable_root, is_derived_id, is_remote_derived, is_valid_id, normalize_lane,
};
use crate::render::{
    StateSession, agent_label, session_body, session_page_path, session_title, state_body,
    state_title,
};
use crate::sanitize::{redact, sanitize_payload};
use crate::session::{
    ASSISTANT_MAX, CONTEXT_VERSION, FinalizeResult, HANDOFF_STALE_TOOL_USES, NewObservation,
    Observation, ObservationKind, PinnedPage, RecentSession, Session, SessionInfo,
    SessionStartRequest, SessionStartResponse, SessionStatus, is_valid_session_id,
    normalize_machine, observation_text,
};
use crate::util::{self, display_date, display_minute, now_ts, sha256_hex};

/// Lines of STATE.md returned by `start_session`.
pub const STATE_EXCERPT_LINES: usize = 60;
/// Session pages returned by `start_session`.
pub const RECENT_SESSIONS: usize = 5;
/// Sessions considered for STATE.md.
pub const STATE_SESSIONS: usize = 10;

/// `SessionStartRequest.source` of a session re-created while replaying queued observations.
pub const OFFLINE_REPLAY_SOURCE: &str = "offline-replay";

/// Input of `Store::write_page` (`kioku_write_page`, `PUT /api/v1/pages`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritePageRequest {
    /// Revision from a read; empty means create-only, absent means unconditional.
    #[serde(default)]
    pub expected_revision: Option<String>,
    /// Page title.
    pub title: String,
    /// Markdown body.
    pub content: String,
    /// Project id (required for project scope).
    #[serde(default)]
    pub project: Option<String>,
    /// `project` | `global`; defaults to project when `project` is set, else global.
    #[serde(default)]
    pub scope: Option<PageScope>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Explicit path inside the scope.
    #[serde(default)]
    pub path: Option<String>,
}

/// Output of `Store::status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// Data directory.
    pub data_dir: String,
    /// Number of projects.
    pub projects: u64,
    /// Number of pages.
    pub pages: u64,
    /// Number of sessions.
    pub sessions: u64,
    /// Number of observations.
    pub observations: u64,
    /// Number of handoffs.
    pub handoffs: u64,
    /// Number of documents in the search index.
    pub index_docs: u64,
    /// Whether wiki writes are committed to git.
    pub git_enabled: bool,
    /// Server version (M2 §9.2; empty from a server that predates the field).
    #[serde(default)]
    pub version: String,
    /// Index schema version recorded on disk (`index/schema-version`); `None` if missing.
    #[serde(default)]
    pub index_schema_version: Option<u32>,
    /// Index schema version this build expects ([`INDEX_SCHEMA_VERSION`]; 0 from an older
    /// server).
    #[serde(default)]
    pub index_schema_expected: u32,
    /// Project aliases `alias → project_id` (M2.4 §2.2; empty from an older server).
    #[serde(default)]
    pub aliases: Vec<ProjectAlias>,
    /// Disk usage and the last prune (SPEC-M2.8 §3; `None` from an older server).
    #[serde(default)]
    pub storage: Option<StorageReport>,
}

/// Output of `Store::merge_projects` (`kioku project merge`, M2.4 §2.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReport {
    /// The project that was folded in (now an alias).
    pub from: String,
    /// The project that received everything.
    pub into: String,
    /// True when nothing was changed (`--dry-run`).
    pub dry_run: bool,
    /// True when `from` was already an alias of `into` (nothing left to move).
    pub already_merged: bool,
    /// Sessions moved.
    pub sessions: u64,
    /// Observations moved.
    pub observations: u64,
    /// Handoffs moved.
    pub handoffs: u64,
    /// Pages moved, as `(old path, new path)`; `from`'s STATE.md is dropped (the target's is
    /// rewritten).
    pub pages: Vec<(String, String)>,
}

/// File in the data directory locked by the process that has it open (SPEC-M2.7 §6).
pub const LOCK_FILE: &str = "kioku.lock";

/// `reliability_meta` key set when a page reached disk and SQLite but not the index.
const NEEDS_REINDEX: &str = "needs_reindex";

/// The kioku store. `Send + Sync`; share it as `Arc<Store>`.
pub struct Store {
    config: Config,
    dirs: DataDir,
    db: Mutex<Connection>,
    index: SearchIndex,
    git: Git,
    write_lock: Mutex<()>,
    /// Held (OS file lock) for the store's lifetime: one process per data directory.
    _lock: std::fs::File,
}

/// Retries of a held `kioku.lock` before giving up ([`LOCK_RETRY_DELAY`] apart).
const LOCK_RETRIES: u32 = 20;
/// Delay between [`LOCK_RETRIES`].
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Another process holds the data directory's `kioku.lock` (SPEC-M2.7 §6).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("another kioku is using {dir}{pid}; stop it first (kioku service stop)")]
pub struct DataDirLocked {
    /// The data directory.
    pub dir: String,
    /// ` (pid N)` when the lock file could be read, else empty.
    pub pid: String,
}

/// Takes `<data_dir>/kioku.lock` without waiting and writes this pid into it; fails when
/// another process holds it. The file is never deleted (deleting a lock file races).
fn lock_data_dir(root: &Path) -> anyhow::Result<std::fs::File> {
    let path = root.join(LOCK_FILE);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // A short grace period: a process that is just exiting (a service restart), or a child
    // forked by another thread that has not reached exec yet, still holds the lock briefly.
    let mut attempt = 0;
    let locked = loop {
        match file.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) if attempt < LOCK_RETRIES => {
                attempt += 1;
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            other => break other,
        }
    };
    match locked {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let pid = std::fs::read_to_string(&path)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
                .map(|p| format!(" (pid {p})"))
                .unwrap_or_default();
            return Err(DataDirLocked {
                dir: root.display().to_string(),
                pid,
            }
            .into());
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
    }
    // Best effort: the pid only makes the error message above more helpful.
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(file)
}

impl Store {
    /// Opens the store for `config.data_dir`, creating the layout, schema and index as needed.
    /// Fails when another process has the directory open (`kioku.lock`) or when its database
    /// was written by a newer kioku ([`crate::NewerSchema`]).
    pub fn open(config: Config) -> Result<Store> {
        let dirs = DataDir::new(&config.data_dir);
        dirs.ensure()?;
        let lock = lock_data_dir(&dirs.root())?;
        let git = Git::open(&dirs.wiki());
        let conn = db::open(&dirs.db_file())?;
        let (index, fresh) = SearchIndex::open(&dirs.index_dir())?;
        let store = Store {
            config,
            dirs,
            db: Mutex::new(conn),
            index,
            git,
            write_lock: Mutex::new(()),
            _lock: lock,
        };
        if !fresh && store.needs_reindex()? {
            // A page reached disk but not the index last time (SPEC-M2.7 §6): self-heal.
            let n = store.reindex()?;
            tracing::warn!(
                pages = n,
                "rebuilt the search index after an earlier index failure"
            );
        }
        if fresh {
            if list_wiki_pages(&store.dirs.wiki())?.is_empty() {
                store.write_index_version()?;
            } else {
                let n = store.reindex()?;
                tracing::info!(pages = n, "index was empty; rebuilt from wiki");
            }
        } else if store.index_outdated() {
            tracing::info!(
                built_with = store.index_version(),
                current = INDEX_SCHEMA_VERSION,
                "the search index was built by an older kioku; the server rebuilds it after \
                 it starts listening (SPEC-M2.8 §5)"
            );
        }
        // SPEC-M2.8 §5: leftovers of interrupted writes, and pages whose row drifted.
        if let Err(e) = store.startup_sweep() {
            tracing::warn!(error = format!("{e:#}"), "startup sweep did not finish");
        }
        // Never fatal: a page that cannot move stays where it is and stays readable.
        if let Err(e) = store.migrate_session_pages() {
            tracing::warn!(error = %e, "session page migration did not finish; retrying at next start");
        }
        Ok(store)
    }

    /// Schema version the on-disk index was built with (1 when unrecorded: pre-versioning).
    pub fn index_version(&self) -> u32 {
        self.index_version_on_disk().unwrap_or(1)
    }

    /// The version recorded in `index/schema-version`, `None` when missing or unreadable.
    pub fn index_version_on_disk(&self) -> Option<u32> {
        std::fs::read_to_string(self.dirs.index_version_file())
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// True when the index predates [`INDEX_SCHEMA_VERSION`] and needs `kioku reindex`.
    pub fn index_outdated(&self) -> bool {
        self.index_version() < INDEX_SCHEMA_VERSION
    }

    /// True when a page write left the index behind (`reliability_meta.needs_reindex`).
    fn needs_reindex(&self) -> Result<bool> {
        let v: Option<String> = self
            .db
            .lock()
            .query_row(
                "SELECT value FROM reliability_meta WHERE key=?1",
                [NEEDS_REINDEX],
                |r| r.get(0),
            )
            .optional()
            .context("reading needs_reindex")?;
        Ok(v.as_deref() == Some("1"))
    }

    fn write_index_version(&self) -> Result<()> {
        let file = self.dirs.index_version_file();
        std::fs::write(&file, format!("{INDEX_SCHEMA_VERSION}\n"))
            .with_context(|| format!("writing {}", file.display()))?;
        Ok(())
    }

    /// The configuration the store was opened with.
    pub fn config(&self) -> Config {
        self.config.clone()
    }

    /// The data directory layout.
    pub fn data_dir(&self) -> DataDir {
        self.dirs.clone()
    }

    /// Whether wiki writes are committed to git.
    pub fn git_enabled(&self) -> bool {
        self.git.enabled()
    }

    // ---------------------------------------------------------------- projects

    /// Registers (or updates) a project identity.
    pub fn register_project(&self, project: &ProjectIdentity) -> Result<()> {
        if !is_valid_id(&project.id) {
            return Err(Error::invalid(format!(
                "invalid project id: {}",
                project.id
            )));
        }
        db::upsert_project(&self.db.lock(), project, &now_ts())?;
        Ok(())
    }

    /// Fetches a registered project (an alias resolves to its canonical project).
    pub fn project(&self, id: &str) -> Result<ProjectRow> {
        let conn = self.db.lock();
        let id = resolve_id(&conn, id)?;
        db::get_project(&conn, &id)?.ok_or_else(|| Error::not_found(format!("project {id}")))
    }

    /// The canonical id for a project id coming from outside (M2.4 §2.2): an alias resolves
    /// to its project, anything else is returned unchanged.
    pub fn resolve_project_id(&self, id: &str) -> Result<String> {
        resolve_id(&self.db.lock(), id)
    }

    /// All project aliases, ordered by alias.
    pub fn list_aliases(&self) -> Result<Vec<ProjectAlias>> {
        Ok(db::list_aliases(&self.db.lock())?)
    }

    /// All registered projects, ordered by id.
    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        Ok(db::list_projects(&self.db.lock())?)
    }

    // ---------------------------------------------------------------- sessions

    /// SessionStart: registers project + session, consumes the pending handoff (superseding
    /// older ones) and returns the context to inject.
    pub fn start_session(&self, req: &SessionStartRequest) -> Result<SessionStartResponse> {
        if !is_valid_session_id(&req.session_id) {
            return Err(Error::invalid(format!(
                "invalid session id: {}",
                req.session_id
            )));
        }
        if !is_valid_id(&req.project.id) {
            return Err(Error::invalid(format!(
                "invalid project id: {}",
                req.project.id
            )));
        }
        let lane = req.lane.as_deref().and_then(normalize_lane);
        let machine = req.machine.as_deref().and_then(normalize_machine);
        let now = now_ts();
        let (project_id, routed, recent) = {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            let project_id = canonical_for_start(&tx, &req.project, &now)?;
            let project = ProjectIdentity {
                id: project_id.clone(),
                ..req.project.clone()
            };
            db::upsert_project(&tx, &project, &now)?;
            db::upsert_session(
                &tx,
                &Session {
                    id: req.session_id.clone(),
                    project_id: project_id.clone(),
                    agent: non_empty_or(&req.agent, "unknown"),
                    cwd: req.cwd.clone(),
                    source: req.source.clone(),
                    started_at: now.clone(),
                    ended_at: None,
                    status: SessionStatus::Open,
                    root_path: Some(req.project.root.clone()).filter(|r| !r.is_empty()),
                    lane: lane.clone(),
                    machine,
                },
            )?;
            // A session re-created by an offline replay is not a person starting work: it
            // must not consume the handoff meant for the next real session (SPEC-M2.6 §3).
            let routed = if req.source == OFFLINE_REPLAY_SOURCE {
                PendingHandoff {
                    handoff: None,
                    reference_handoff: None,
                }
            } else {
                route_pending(&tx, &project_id, lane.as_deref(), &req.session_id, &now)?
            };
            // The session's own page (a resumed session) is not "recent" context for itself.
            let recent: Vec<(Session, String)> =
                recent_sessions(&tx, &project_id, None, RECENT_SESSIONS + 1)?
                    .into_iter()
                    .filter(|(s, _)| s.id != req.session_id)
                    .take(RECENT_SESSIONS)
                    .collect();
            tx.commit().context("committing session start")?;
            (project_id, routed, recent)
        };
        let mut resp = SessionStartResponse {
            project_id: project_id.clone(),
            pending_handoff: routed.handoff,
            state_excerpt: self.state_excerpt(&project_id),
            recent_sessions: recent.into_iter().map(recent_entry).collect(),
            lane,
            reference_handoff: routed.reference_handoff,
            ..SessionStartResponse::default()
        };
        self.fill_context(&mut resp, &req.session_id)?;
        Ok(resp)
    }

    /// The SPEC-M3.0 §1 sections of a SessionStart response: items carried from the last
    /// [`CARRY_HANDOFFS`] agent handoffs (those of the handoff shown in section 2 left out),
    /// pinned pages and the previous session's last reply on the same lane (left out when
    /// the handoff shown in section 2 came from that session, which already holds it).
    fn fill_context(&self, resp: &mut SessionStartResponse, session_id: &str) -> Result<()> {
        let shown = resp
            .pending_handoff
            .as_ref()
            .or(resp.reference_handoff.as_ref());
        let shown_items = shown
            .map(|h| handoff_items(&h.content_md))
            .unwrap_or_default();
        let (sources, pinned, last_reply) = {
            let conn = self.db.lock();
            let sources: Vec<SourceHandoff> =
                db::recent_agent_handoffs(&conn, &resp.project_id, CARRY_HANDOFFS)?
                    .iter()
                    .map(SourceHandoff::from_handoff)
                    .collect();
            let pinned = db::tagged_pages(&conn, &resp.project_id, PINNED_TAG, MAX_PINNED)?;
            let previous =
                db::previous_session(&conn, &resp.project_id, resp.lane.as_deref(), session_id)?
                    .filter(|p| shown.is_none_or(|h| h.session_id.as_deref() != Some(&p.id)));
            let last_reply = match previous {
                Some(p) => db::last_assistant(&conn, &p.id)?
                    .map(|(_, text)| util::truncate_chars(text.trim(), LAST_REPLY_MAX))
                    .filter(|t| !t.is_empty()),
                None => None,
            };
            (sources, pinned, last_reply)
        };
        let carried = carry(&sources, &shown_items);
        resp.context_version = CONTEXT_VERSION;
        resp.decisions = carried.decisions;
        resp.verified = carried.verified;
        resp.open_questions = carried.open_questions;
        resp.gotchas = carried.gotchas;
        resp.pinned = self.pinned_pages(&pinned);
        resp.last_reply = last_reply;
        Ok(())
    }

    /// Title, path and the start of the body of each pinned page (unreadable files skipped).
    fn pinned_pages(&self, rows: &[PageRow]) -> Vec<PinnedPage> {
        rows.iter()
            .filter_map(|row| {
                let text = std::fs::read_to_string(self.dirs.wiki().join(&row.path)).ok()?;
                let page = Page::parse(&row.path, &text).ok()?;
                Some(PinnedPage {
                    path: row.path.clone(),
                    title: row.title.clone(),
                    excerpt: util::truncate_chars(page.body.trim(), PINNED_EXCERPT),
                })
            })
            .collect()
    }

    /// The SessionStart context of an existing session, without side effects (M2 §9.1):
    /// the handoff that session accepted (newest), the STATE.md excerpt and the project's
    /// recent session pages (the session's own page excluded).
    pub fn session_context(&self, id: &str) -> Result<SessionStartResponse> {
        let (session, pending_handoff, reference_handoff, recent) = {
            let conn = self.db.lock();
            let session = db::get_session(&conn, id)?
                .ok_or_else(|| Error::not_found(format!("session {id}")))?;
            let pending = db::newest_handoff_accepted_by(
                &conn,
                id,
                &session.project_id,
                session.lane.as_deref(),
            )?;
            // A branch lane without a handoff of its own sees the project lane's pending one
            // for reference (§1.4 rule 2), without consuming it.
            let reference = match (&pending, &session.lane) {
                (None, Some(_)) => db::newest_handoff(&conn, &session.project_id, None, true)?,
                _ => None,
            };
            let recent: Vec<(Session, String)> =
                recent_sessions(&conn, &session.project_id, None, RECENT_SESSIONS + 1)?
                    .into_iter()
                    .filter(|(s, _)| s.id != session.id)
                    .take(RECENT_SESSIONS)
                    .collect();
            (session, pending, reference, recent)
        };
        let mut resp = SessionStartResponse {
            state_excerpt: self.state_excerpt(&session.project_id),
            project_id: session.project_id,
            pending_handoff,
            recent_sessions: recent.into_iter().map(recent_entry).collect(),
            lane: session.lane,
            reference_handoff,
            ..SessionStartResponse::default()
        };
        self.fill_context(&mut resp, id)?;
        Ok(resp)
    }

    /// Fetches a session row.
    pub fn session(&self, id: &str) -> Result<Session> {
        db::get_session(&self.db.lock(), id)?
            .ok_or_else(|| Error::not_found(format!("session {id}")))
    }

    /// Stop-hook view of a session: status, counts, whether the agent wrote a handoff.
    pub fn session_info(&self, id: &str) -> Result<SessionInfo> {
        let conn = self.db.lock();
        let session =
            db::get_session(&conn, id)?.ok_or_else(|| Error::not_found(format!("session {id}")))?;
        let counts = db::session_counts(&conn, id)?;
        let has_agent_handoff =
            db::newest_session_handoff(&conn, id, Some(HandoffSource::Agent), false)?.is_some();
        let since = db::tool_uses_since_handoff(&conn, id)?;
        // SPEC-M3.0 §4: time since the latest agent handoff (since the start without one),
        // by this server's clock so client clocks do not matter.
        let mark = db::agent_handoff_mark(&conn, id)?
            .map(|m| m.created_at)
            .unwrap_or_else(|| session.started_at.clone());
        let secs_since_handoff =
            util::parse_ts(&mark).map(|t| (util::now() - t).num_seconds().max(0) as u64);
        Ok(SessionInfo {
            project_id: session.project_id,
            status: session.status,
            counts,
            has_agent_handoff,
            tool_uses_since_handoff: Some(since),
            secs_since_handoff,
        })
    }

    /// Records an observation (payload is sanitized again server-side); returns its seq.
    ///
    /// A prompt or tool use on a finalized session reopens it: Claude Code's Stop hook fires
    /// after every turn, so a session is finalized many times over its life.
    pub fn add_observation(&self, obs: &NewObservation) -> Result<i64> {
        if !is_valid_session_id(&obs.session_id) {
            return Err(Error::invalid(format!(
                "invalid session id: {}",
                obs.session_id
            )));
        }
        let mut payload = sanitize_payload(&obs.payload);
        if obs.kind == ObservationKind::Assistant {
            // SPEC-M3.0 §3: `{text}` only, trimmed, at most ASSISTANT_MAX chars.
            let reply = payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if reply.is_empty() {
                return Err(Error::invalid("an assistant observation needs a non-empty text"));
            }
            payload = serde_json::json!({ "text": util::truncate_chars(&reply, ASSISTANT_MAX) });
        }
        let text = redact(&observation_text(obs.kind, &payload));
        let ts = obs
            .ts
            .as_deref()
            .and_then(util::parse_ts)
            .map(util::fmt_ts)
            .unwrap_or_else(now_ts);
        let payload_json = serde_json::to_string(&payload).context("serializing payload")?;
        if obs
            .event_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 200)
        {
            return Err(Error::invalid("event_id must be 1..200 bytes"));
        }
        let mut receipt_content =
            serde_json::to_value((obs.kind, &obs.ts, &payload)).context("normalizing delivery")?;
        receipt_content.sort_all_objects();
        let request_hash =
            sha256_hex(&serde_json::to_string(&receipt_content).context("hashing delivery")?);
        let mut conn = self.db.lock();
        let (seq, project_id) = {
            let tx = conn.transaction().context("starting transaction")?;
            let session = db::get_session(&tx, &obs.session_id)?
                .ok_or_else(|| Error::not_found(format!("session {}", obs.session_id)))?;
            if let Some(id) = &obs.event_id {
                let receipt: Option<(i64, String)> = tx.query_row(
                    "SELECT seq, request_hash FROM observation_receipts WHERE session_id=?1 AND event_id=?2",
                    params![obs.session_id, id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional().context("reading receipt")?;
                if let Some((seq, old_hash)) = receipt {
                    if old_hash != request_hash {
                        return Err(Error::Conflict(
                            "event_id reused with different content".into(),
                        ));
                    }
                    return Ok(seq);
                }
            }
            // The same reply twice in a row (a Stop repeated without a new turn) is stored
            // once (SPEC-M3.0 §3).
            if obs.kind == ObservationKind::Assistant
                && let Some((seq, last)) = db::last_assistant(&tx, &session.id)?
                && payload.get("text").and_then(serde_json::Value::as_str) == Some(last.as_str())
            {
                return Ok(seq);
            }
            let seq = db::insert_observation(
                &tx,
                &session.id,
                &session.project_id,
                obs.kind,
                &ts,
                &payload_json,
                &text,
            )?;
            let substantive = matches!(
                obs.kind,
                ObservationKind::Prompt | ObservationKind::ToolUse | ObservationKind::Assistant
            );
            if substantive && session.status == SessionStatus::Finalized {
                db::set_session_status(&tx, &session.id, SessionStatus::Open, None)?;
            }
            if let Some(id) = &obs.event_id {
                tx.execute(
                    "INSERT INTO observation_receipts VALUES (?1,?2,?3,?4)",
                    params![obs.session_id, id, seq, request_hash],
                )
                .context("recording receipt")?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO reliability_meta VALUES ('last_received', ?1)",
                [now_ts()],
            )
            .context("recording receipt time")?;
            tx.commit().context("committing observation")?;
            (seq, session.project_id)
        };
        let line = serde_json::json!({
            "seq": seq, "kind": obs.kind, "ts": ts, "payload": payload,
        });
        if let Err(e) = append_line(
            &self.dirs.raw_file(&project_id, &obs.session_id),
            &line.to_string(),
        ) {
            tracing::warn!(error = %e, "appending raw observation failed");
        }
        Ok(seq)
    }

    /// All observations of a session, in order.
    pub fn observations(&self, session_id: &str) -> Result<Vec<Observation>> {
        Ok(db::list_observations(&self.db.lock(), session_id)?)
    }

    /// The rule-based digest of a session (spec §7.2).
    pub fn digest(&self, session_id: &str) -> Result<SessionDigest> {
        let conn = self.db.lock();
        let session = db::get_session(&conn, session_id)?
            .ok_or_else(|| Error::not_found(format!("session {session_id}")))?;
        digest_for(&conn, &session)
    }

    /// Finalize (spec §7.1): session page, rules handoff if the agent wrote none (or an
    /// addendum when work continued after it), STATE.md.
    /// Idempotent: calling it again without new observations returns the same result.
    pub fn finalize_session(&self, session_id: &str) -> Result<FinalizeResult> {
        self.finalize_with(session_id, &|_| {})
    }

    /// [`Store::finalize_session`] with a callback run after the digest was built and before
    /// the session is marked finalized (tests use it to inject a concurrent observation).
    fn finalize_with(
        &self,
        session_id: &str,
        before_mark: &dyn Fn(&Store),
    ) -> Result<FinalizeResult> {
        let _write = self.write_lock.lock();
        let (mut session, project, digest, max_seq, delta) = {
            let conn = self.db.lock();
            let session = db::get_session(&conn, session_id)?
                .ok_or_else(|| Error::not_found(format!("session {session_id}")))?;
            let project = db::get_project(&conn, &session.project_id)?
                .ok_or_else(|| Error::not_found(format!("project {}", session.project_id)))?;
            // Read the high-water mark first: anything newer is not in this digest.
            let max_seq = db::max_seq(&conn, session_id)?;
            // The addendum digest (§7.1 step 4) is needed only when work continued after
            // the agent's handoff.
            let with_delta = db::agent_handoff_mark(&conn, session_id)?.is_some()
                && db::tool_uses_since_handoff(&conn, session_id)? >= HANDOFF_STALE_TOOL_USES;
            // SPEC-M2.8 §1: the cached digest, extended with the new observations only.
            let (digest, delta) = session_digests(&conn, &session, max_seq, with_delta)?;
            (session, project, digest, max_seq, delta)
        };

        if session.status == SessionStatus::Finalized {
            if !digest.is_substantive() {
                return Ok(FinalizeResult::default());
            }
            let handoff = db::newest_session_handoff(&self.db.lock(), session_id, None, false)?;
            return Ok(FinalizeResult {
                substantive: true,
                session_page: Some(session_page_path(&session)),
                handoff_id: handoff.map(|h| h.id),
            });
        }

        let now = now_ts();
        if !digest.is_substantive() {
            before_mark(self);
            db::finalize_if_unchanged(&self.db.lock(), session_id, max_seq, &now)?;
            return Ok(FinalizeResult::default());
        }
        session.ended_at = Some(now.clone());
        let lang = self.config.lang();

        // 3. session page
        let handoff_md = match (&digest.agent_handoff, &delta) {
            (Some(h), Some(d)) => format!(
                "{}\n\n{}",
                h.content_md.trim_end(),
                d.handoff_delta_section(lang)
            ),
            (Some(h), None) => h.content_md.clone(),
            (None, _) => digest.handoff_section(lang),
        };
        let page_path = session_page_path(&session);
        let title = session_title(lang, &session, &digest);
        let fm = Frontmatter {
            title: title.clone(),
            project: Some(project.id.clone()),
            scope: PageScope::Project,
            kind: PageKind::Session,
            tags: vec![session.agent.clone()],
            session: Some(session.id.clone()),
            agent: Some(session.agent.clone()),
            lane: session.lane.clone(),
            machine: session.machine.clone(),
            ..Frontmatter::default()
        };
        // Written together with STATE.md below: one git commit, one index commit (§2).
        let session_write = self.prepare_page(
            &page_path,
            fm,
            &session_body(lang, &session, &digest, &handoff_md),
            true,
        )?;

        // 4. rules handoff: one pending rules handoff per session (or per agent handoff, for
        // the addendum), refreshed in place on re-finalize with its created_at kept. One that
        // was already accepted is refreshed in place too, unless the digest changed in a way
        // that matters since it was issued (SPEC-M3.0 §4); only then is a new row issued.
        let handoff_id = match (&digest.agent_handoff, &delta) {
            (Some(h), None) => h.id.clone(),
            (agent, _) => {
                let conn = self.db.lock();
                let existing = db::newest_session_handoff(
                    &conn,
                    session_id,
                    Some(HandoffSource::Rules),
                    true,
                )?
                // An addendum must be newer than the agent handoff it extends; an older
                // rules handoff predates it and is left alone (it ranks below the agent's).
                .filter(|r| agent.as_ref().is_none_or(|a| r.created_at >= a.created_at));
                let existing = match existing {
                    Some(pending) => Some(pending),
                    None => db::newest_session_handoff(
                        &conn,
                        session_id,
                        Some(HandoffSource::Rules),
                        false,
                    )?
                    .filter(|r| agent.as_ref().is_none_or(|a| r.created_at >= a.created_at))
                    .map(|r| -> Result<Option<Handoff>> {
                        let Some(at) = db::handoff_seq_at(&conn, &r.id)? else {
                            return Ok(None);
                        };
                        let since = db::list_observations_between(&conn, session_id, at, max_seq)?;
                        let changed =
                            SessionDigest::from_observations(&since, None).is_meaningful_change();
                        Ok((!changed).then_some(r))
                    })
                    .transpose()?
                    .flatten(),
                };
                match existing {
                    Some(existing) => {
                        db::update_handoff_content(&conn, &existing.id, &handoff_md, &now)?;
                        existing.id
                    }
                    None => {
                        let h = Handoff {
                            id: util::new_id(),
                            project_id: project.id.clone(),
                            session_id: Some(session.id.clone()),
                            source: HandoffSource::Rules,
                            content_md: handoff_md.clone(),
                            created_at: now.clone(),
                            accepted_at: None,
                            accepted_by: None,
                            agent: Some(session.agent.clone()),
                            updated_at: None,
                            lane: session.lane.clone(),
                        };
                        db::insert_handoff(&conn, &h, Some(max_seq))?;
                        h.id
                    }
                }
            }
        };

        // 5. STATE.md, then one commit for both pages (SPEC-M2.8 §2); an unchanged page
        // (apart from `updated:`) is neither written, committed nor reindexed.
        let state_write = self.state_page(&project, Some((session_id, &title)))?;
        let message = match &session_write {
            Some(_) => format!("kioku: session {page_path}"),
            None => format!("kioku: state {}/STATE.md", project.id),
        };
        let writes: Vec<PageWrite> = session_write.into_iter().chain(state_write).collect();
        self.store_pages(&writes, &message)?;

        // 6. mark finalized — unless an observation arrived meanwhile (then it stays open
        // and the next Stop / SessionEnd finalizes it with that observation included).
        before_mark(self);
        db::finalize_if_unchanged(&self.db.lock(), session_id, max_seq, &now)?;
        Ok(FinalizeResult {
            substantive: true,
            session_page: Some(page_path),
            handoff_id: Some(handoff_id),
        })
    }

    // ---------------------------------------------------------------- handoffs

    /// Records an agent-written handoff (spec §7.5); when `session` is omitted it attaches to
    /// the open session of the project with the newest observation.
    /// Every text field is redacted like an observation before it is stored (SPEC-M2.7 §9).
    pub fn write_handoff(&self, input: &HandoffInput) -> Result<Handoff> {
        if input.summary.trim().is_empty() {
            return Err(Error::invalid("summary must not be empty"));
        }
        let redact_all = |v: &[String]| v.iter().map(|s| redact(s)).collect::<Vec<_>>();
        let input = &HandoffInput {
            project: input.project.clone(),
            session: input.session.clone(),
            summary: redact(&input.summary),
            next_steps: redact_all(&input.next_steps),
            open_questions: redact_all(&input.open_questions),
            decisions: redact_all(&input.decisions),
            gotchas: redact_all(&input.gotchas),
            verified: redact_all(&input.verified),
        };
        let conn = self.db.lock();
        let project_id = resolve_id(&conn, &input.project)?;
        db::get_project(&conn, &project_id)?
            .ok_or_else(|| Error::not_found(format!("project {}", input.project)))?;
        let session = match &input.session {
            Some(id) => {
                let s = db::get_session(&conn, id)?
                    .ok_or_else(|| Error::not_found(format!("session {id}")))?;
                if s.project_id != project_id {
                    return Err(Error::invalid(format!(
                        "session {id} belongs to project {}",
                        s.project_id
                    )));
                }
                Some(s)
            }
            None => db::newest_open_session(&conn, &project_id)?,
        };
        // The lane of the session the agent named; without one, the project lane (§1.3).
        let lane = match (&input.session, &session) {
            (Some(_), Some(s)) => s.lane.clone(),
            _ => None,
        };
        let now = now_ts();
        let agent = session
            .as_ref()
            .map(|s| s.agent.clone())
            .unwrap_or_else(|| "unknown".to_string());
        // `claude-code@mini` in the heading when the session's machine is known (§6).
        let label = agent_label(
            &agent,
            session.as_ref().and_then(|s| s.machine.as_deref()),
        );
        let content =
            render_agent_handoff(self.config.lang(), &label, &display_minute(&now), input);
        let h = Handoff {
            id: util::new_id(),
            project_id,
            session_id: session.as_ref().map(|s| s.id.clone()),
            source: HandoffSource::Agent,
            content_md: content,
            created_at: now,
            accepted_at: None,
            accepted_by: None,
            agent: session.as_ref().map(|s| s.agent.clone()),
            updated_at: None,
            lane,
        };
        let seq_at = match &session {
            Some(s) => Some(db::max_seq(&conn, &s.id)?),
            None => None,
        };
        db::insert_handoff(&conn, &h, seq_at)?;
        Ok(h)
    }

    /// Newest pending handoff of a project on the lane of `session` (the project lane when
    /// `session` is absent or unknown). With `accept`, it is consumed by `session` (or
    /// `"api"`) and older pending ones of that lane are superseded; without, it is only
    /// peeked. See [`Store::pending_handoff_routed`] for the reference handoff.
    pub fn pending_handoff(
        &self,
        project: &str,
        accept: bool,
        session: Option<&str>,
    ) -> Result<Option<Handoff>> {
        Ok(self
            .pending_handoff_routed(project, accept, session, None)?
            .handoff)
    }

    /// The pending handoff routed by lane (M2.4 §1.4). The lane is `lane` when given (`""` =
    /// the project lane), else the lane of `session`, else the project lane. On a branch
    /// lane without a pending handoff of its own, the project lane's is returned as
    /// `reference_handoff` and never accepted.
    pub fn pending_handoff_routed(
        &self,
        project: &str,
        accept: bool,
        session: Option<&str>,
        lane: Option<&str>,
    ) -> Result<PendingHandoff> {
        let mut conn = self.db.lock();
        let tx = conn.transaction().context("starting transaction")?;
        let project = resolve_id(&tx, project)?;
        let lane = match lane {
            Some(l) => normalize_lane(l),
            None => match session {
                Some(id) => db::get_session(&tx, id)?
                    .filter(|s| s.project_id == project)
                    .and_then(|s| s.lane),
                None => None,
            },
        };
        let routed = if accept {
            route_pending(
                &tx,
                &project,
                lane.as_deref(),
                session.unwrap_or("api"),
                &now_ts(),
            )?
        } else {
            peek_pending(&tx, &project, lane.as_deref())?
        };
        tx.commit().context("committing handoff acceptance")?;
        Ok(routed)
    }

    /// Newest handoff of a project on the project lane, accepted or not.
    pub fn latest_handoff(&self, project: &str) -> Result<Option<Handoff>> {
        let conn = self.db.lock();
        let project = resolve_id(&conn, project)?;
        Ok(db::newest_handoff(&conn, &project, None, false)?)
    }

    // ---------------------------------------------------------------- pages

    /// Writes a page (spec §6.1 path rules); returns its wiki-relative path. Title and body
    /// are redacted like observations before anything is written (SPEC-M2.7 §9).
    pub fn write_page(&self, req: &WritePageRequest) -> Result<String> {
        let title = redact(req.title.trim());
        let title = title.as_str();
        let content = redact(&req.content);
        if title.is_empty() {
            return Err(Error::invalid("title must not be empty"));
        }
        // A path read from `_global/…` is a global page even when the agent also names its
        // project (it usually does); nesting it under `<project>/pages/` would write elsewhere.
        let explicit_global = req.path.as_deref().is_some_and(|p| {
            p.trim_start_matches('/')
                .starts_with(&format!("{GLOBAL_DIR}/"))
        });
        let scope = req
            .scope
            .unwrap_or(if req.project.is_some() && !explicit_global {
                PageScope::Project
            } else {
                PageScope::Global
            });
        let project = match scope {
            PageScope::Global => None,
            PageScope::Project => {
                let id = req
                    .project
                    .as_deref()
                    .ok_or_else(|| Error::invalid("scope=project requires a project"))?;
                Some(self.project(id)?.id)
            }
        };
        let path =
            match self.redirected_write_path(req.path.as_deref(), scope, project.as_deref())? {
                Some(moved) => moved,
                None => resolve_write_path(title, scope, project.as_deref(), req.path.as_deref())?,
            };
        let fm = Frontmatter {
            title: title.to_string(),
            project,
            scope,
            kind: PageKind::Page,
            tags: req.tags.clone(),
            ..Frontmatter::default()
        };
        let _write = self.write_lock.lock();
        // An empty revision is the same as none: some MCP clients send "" for every
        // optional string argument, and that must keep the legacy unconditional write.
        if let Some(expected) = req.expected_revision.as_deref().filter(|r| !r.is_empty()) {
            let actual = match std::fs::read(self.dirs.wiki().join(&path)) {
                Ok(bytes) => format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::Conflict(format!(
                        "{path} does not exist, so it cannot match expected_revision; check the \
                         path and scope (a revision is only for updating an existing page)"
                    )));
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context("reading page revision")
                        .into());
                }
            };
            if expected != actual {
                return Err(Error::Conflict(format!(
                    "{path} changed since it was read; read it again before writing"
                )));
            }
        }
        self.put_page(&path, fm, &content)?;
        Ok(path)
    }

    /// Where an explicit write path that was moved (merge, migration) lives now, when that
    /// place is one this scope may write (`<project>/pages/…` or `_global/…`).
    fn redirected_write_path(
        &self,
        explicit: Option<&str>,
        scope: PageScope,
        project: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(raw) = explicit else {
            return Ok(None);
        };
        let rel = validate_rel_path(raw)?;
        if self.dirs.wiki().join(&rel).is_file() {
            return Ok(None);
        }
        let moved = self.follow_redirect(rel.clone())?;
        if moved == rel {
            return Ok(None);
        }
        let root = match (scope, project) {
            (PageScope::Project, Some(p)) => format!("{p}/pages/"),
            _ => format!("{GLOBAL_DIR}/"),
        };
        Ok(moved.starts_with(&root).then_some(moved))
    }

    /// `rel`, or where a moved page now lives (`page_redirects`); a real file at `rel` wins.
    fn follow_redirect(&self, rel: String) -> Result<String> {
        if self.dirs.wiki().join(&rel).is_file() {
            return Ok(rel);
        }
        Ok(self
            .db
            .lock()
            .query_row(
                "SELECT new_path FROM page_redirects WHERE old_path=?1",
                [&rel],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .context("reading page redirect")?
            .unwrap_or(rel))
    }

    /// Reads a page by wiki-relative path.
    pub fn read_page(&self, path: &str) -> Result<Page> {
        let rel = validate_rel_path(path)?;
        let rel = self.follow_redirect(rel)?;
        let file = self.dirs.wiki().join(&rel);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::not_found(format!("page {rel}")));
            }
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("reading {rel}"))
                    .into());
            }
        };
        Page::parse(&rel, &text)
    }

    /// Full-text search (spec §6.3).
    /// A `project` scope naming an alias searches its canonical project.
    pub fn search(&self, query: &str, scope: &SearchScope, limit: usize) -> Result<Vec<Hit>> {
        let scope = match scope {
            SearchScope::Project(id) => SearchScope::Project(self.resolve_project_id(id)?),
            other => other.clone(),
        };
        Ok(self.index.search(query, &scope, limit)?)
    }

    /// Clears and rebuilds `pages` + the index from `wiki/`; returns the number of pages.
    pub fn reindex(&self) -> Result<usize> {
        let _write = self.write_lock.lock();
        self.reindex_locked()
    }

    /// [`Store::reindex`] body; the caller holds the write lock.
    fn reindex_locked(&self) -> Result<usize> {
        let wiki = self.dirs.wiki();
        let mut rows = Vec::new();
        let mut docs = Vec::new();
        for rel in list_wiki_pages(&wiki)? {
            let text = match std::fs::read_to_string(wiki.join(&rel)) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(path = %rel, error = %e, "skipping unreadable page");
                    continue;
                }
            };
            let page = match Page::parse(&rel, &text) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %rel, error = %e, "skipping unparsable page");
                    continue;
                }
            };
            let (row, doc) = page_records(&page, &text);
            rows.push(row);
            docs.push(doc);
        }
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            db::delete_all_pages(&tx)?;
            for row in &rows {
                db::upsert_page(&tx, row)?;
            }
            tx.commit().context("committing reindex")?;
        }
        self.index.upsert_many(&docs, true)?;
        self.write_index_version()?;
        self.db
            .lock()
            .execute("DELETE FROM reliability_meta WHERE key=?1", [NEEDS_REINDEX])
            .context("recording needs_reindex")?;
        Ok(docs.len())
    }

    /// Counts and disk usage for `kioku status`.
    pub fn status(&self) -> Result<StatusReport> {
        let storage = Some(self.storage()?);
        let conn = self.db.lock();
        Ok(StatusReport {
            data_dir: self.dirs.root().display().to_string(),
            projects: db::count(&conn, "projects")?,
            pages: db::count(&conn, "pages")?,
            sessions: db::count(&conn, "sessions")?,
            observations: db::count(&conn, "observations")?,
            handoffs: db::count(&conn, "handoffs")?,
            index_docs: self.index.num_docs(),
            git_enabled: self.git.enabled(),
            version: crate::VERSION.to_string(),
            index_schema_version: self.index_version_on_disk(),
            index_schema_expected: INDEX_SCHEMA_VERSION,
            aliases: db::list_aliases(&conn)?,
            storage,
        })
    }

    /// `kioku project merge` (M2.4 §2.3): moves sessions, observations, handoffs, raw logs
    /// and pages of `from` into `into` (pages go to `into`'s wiki directory, with a git
    /// commit), records `from` as an alias of `into`, reindexes and rewrites `into`'s
    /// STATE.md. `dry_run` only reports. Refuses unknown or identical ids; merging again once
    /// `from` is already an alias of `into` changes nothing (`already_merged`).
    pub fn merge_projects(&self, from: &str, into: &str, dry_run: bool) -> Result<MergeReport> {
        let from = from.trim();
        if !is_valid_id(from) {
            return Err(Error::invalid(format!("invalid project id: {from}")));
        }
        let _write = self.write_lock.lock();
        let (into_row, counts, from_existed) = {
            let conn = self.db.lock();
            let into_id = resolve_id(&conn, into.trim())?;
            let into_row = db::get_project(&conn, &into_id)?
                .ok_or_else(|| Error::not_found(format!("project {into}")))?;
            if from == into_row.id {
                return Err(Error::invalid(format!(
                    "cannot merge project {from} into itself"
                )));
            }
            if db::get_project(&conn, from)?.is_none() {
                if db::resolve_alias(&conn, from)?.as_deref() == Some(into_row.id.as_str()) {
                    return Ok(MergeReport {
                        from: from.to_string(),
                        into: into_row.id,
                        dry_run,
                        already_merged: true,
                        ..MergeReport::default()
                    });
                }
                return Err(Error::not_found(format!("project {from}")));
            }
            let counts = db::project_row_counts(&conn, from)?;
            (into_row, counts, self.dirs.wiki().join(from).is_dir())
        };
        let wiki = self.dirs.wiki();
        let moves = plan_page_moves(&wiki, from, &into_row.id)?;
        let report = MergeReport {
            from: from.to_string(),
            into: into_row.id.clone(),
            dry_run,
            already_merged: false,
            sessions: counts.sessions,
            observations: counts.observations,
            handoffs: counts.handoffs,
            pages: moves.clone(),
        };
        if dry_run {
            return Ok(report);
        }

        for (old, new) in &moves {
            move_page(&wiki, old, new, from, &into_row.id)?;
        }
        let from_dir = wiki.join(from);
        let state = from_dir.join("STATE.md");
        if state.exists() {
            std::fs::remove_file(&state)
                .with_context(|| format!("removing {}", state.display()))?;
        }
        remove_empty_dirs(&from_dir);
        move_raw_logs(
            &self.dirs.raw().join(from),
            &self.dirs.raw().join(&into_row.id),
        )?;
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            db::move_project_rows(&tx, from, &into_row.id)?;
            for (old, new) in &moves {
                tx.execute(
                    "UPDATE page_redirects SET new_path=?2 WHERE new_path=?1",
                    params![old, new],
                )
                .context("moving legacy redirects")?;
                tx.execute(
                    "INSERT OR REPLACE INTO page_redirects VALUES (?1,?2)",
                    params![old, new],
                )
                .context("preserving moved page paths")?;
            }
            db::repoint_aliases(&tx, from, &into_row.id)?;
            db::upsert_alias(&tx, from, &into_row.id, &now_ts())?;
            tx.commit().context("committing project merge")?;
        }
        let mut paths = Vec::new();
        if from_existed {
            paths.push(from.to_string());
        }
        if wiki.join(&into_row.id).is_dir() {
            paths.push(into_row.id.clone());
        }
        self.git.commit(
            &paths,
            &format!("kioku: merge project {from} into {}", into_row.id),
        );
        self.reindex_locked()?;
        self.write_state(&into_row)?;
        Ok(report)
    }

    // ---------------------------------------------------------------- internals

    /// Writes a page file (keeping `created` and unknown keys of an existing file), then
    /// updates SQLite, the index and git (its own commit). Caller must hold the write lock.
    fn put_page(&self, path: &str, fm: Frontmatter, body: &str) -> Result<Page> {
        let write = self
            .prepare_page(path, fm, body, false)?
            .context("a page write without skip_unchanged always renders")?;
        let page = write.page.clone();
        self.store_pages(
            std::slice::from_ref(&write),
            &format!("kioku: {} {path}", page.frontmatter.kind.as_str()),
        )?;
        Ok(page)
    }

    /// Renders a page for [`Store::store_pages`], keeping `created` and unknown keys of an
    /// existing file. With `skip_unchanged`, `None` when the file would only differ in its
    /// `updated:` line (SPEC-M2.8 §2).
    fn prepare_page(
        &self,
        path: &str,
        mut fm: Frontmatter,
        body: &str,
        skip_unchanged: bool,
    ) -> Result<Option<PageWrite>> {
        let file = self.dirs.wiki().join(path);
        let now = util::fmt_ts_secs(util::now());
        let mut existing_text = None;
        let mut old_updated = None;
        if let Ok(existing) = std::fs::read_to_string(&file)
            && let Ok(old) = Page::parse(path, &existing)
        {
            if !old.frontmatter.created.is_empty() {
                fm.created = old.frontmatter.created;
            }
            for (k, v) in old.frontmatter.extra {
                fm.extra.entry(k).or_insert(v);
            }
            old_updated = Some(old.frontmatter.updated);
            existing_text = Some(existing);
        }
        if fm.created.is_empty() {
            fm.created = now.clone();
        }
        let mut page = Page {
            revision: String::new(),
            path: path.to_string(),
            frontmatter: fm,
            body: body.to_string(),
        };
        if skip_unchanged && let (Some(existing), Some(updated)) = (&existing_text, old_updated) {
            page.frontmatter.updated = updated;
            if page.render()? == *existing {
                return Ok(None);
            }
        }
        page.frontmatter.updated = now;
        let text = page.render()?;
        page.revision = sha256_hex(&text);
        Ok(Some(PageWrite {
            path: path.to_string(),
            page,
            text,
        }))
    }

    /// Writes prepared pages: files, SQLite rows, **one** index commit and **one** git
    /// commit with `message` (SPEC-M2.8 §2). Caller must hold the write lock.
    fn store_pages(&self, writes: &[PageWrite], message: &str) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let mut docs = Vec::new();
        for w in writes {
            write_atomic(&self.dirs.wiki().join(&w.path), &w.text)?;
            let (row, doc) = page_records(&w.page, &w.text);
            db::upsert_page(&self.db.lock(), &row)?;
            docs.push(doc);
        }
        if let Err(e) = self.index.upsert_many(&docs, false) {
            // The files and rows are written; the next start rebuilds the index.
            tracing::warn!(
                paths = ?writes.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(),
                error = format!("{e:#}"),
                "indexing failed; the index is rebuilt at the next start"
            );
            self.db
                .lock()
                .execute(
                    "INSERT OR REPLACE INTO reliability_meta VALUES (?1, '1')",
                    [NEEDS_REINDEX],
                )
                .context("recording needs_reindex")?;
        }
        let paths: Vec<String> = writes.iter().map(|w| w.path.clone()).collect();
        self.git.commit(&paths, message);
        Ok(())
    }

    /// Rewrites STATE.md with its own commit (merge, forget); unchanged → nothing happens.
    fn write_state(&self, project: &ProjectRow) -> Result<()> {
        let writes: Vec<PageWrite> = self.state_page(project, None)?.into_iter().collect();
        self.store_pages(&writes, &format!("kioku: state {}/STATE.md", project.id))
    }

    /// STATE.md of `project` rendered for [`Store::store_pages`]; `None` when unchanged.
    /// `include` is the session being finalized with its page title (its page may not be
    /// written yet). Finalized sessions contribute their cached digests (SPEC-M2.8 §1).
    fn state_page(
        &self,
        project: &ProjectRow,
        include: Option<(&str, &str)>,
    ) -> Result<Option<PageWrite>> {
        let lang = self.config.lang();
        let (latest, recent, digests, sources, pinned) = {
            let conn = self.db.lock();
            // The project lane's handoff: STATE.md is shown to every session, and a branch
            // lane's handoff must not reach the default branch (M2.4 §1.4).
            let latest = db::newest_handoff(&conn, &project.id, None, false)?;
            // SPEC-M3.0 §1: the same carried sections as the `<kioku>` block.
            let sources: Vec<SourceHandoff> =
                db::recent_agent_handoffs(&conn, &project.id, CARRY_HANDOFFS)?
                    .iter()
                    .map(SourceHandoff::from_handoff)
                    .collect();
            let pinned = db::tagged_pages(&conn, &project.id, PINNED_TAG, MAX_PINNED)?;
            let mut recent = Vec::new();
            for s in db::recent_substantive_sessions(
                &conn,
                &project.id,
                include.map(|i| i.0),
                STATE_SESSIONS,
            )? {
                match include {
                    Some((id, title)) if id == s.id => recent.push((s, title.to_string())),
                    _ => {
                        if let Some(page) = db::get_page(&conn, &session_page_path(&s))? {
                            recent.push((s, page.title));
                        }
                    }
                }
            }
            let mut digests = Vec::new();
            for (s, _) in &recent {
                digests.push(cached_digest(&conn, s)?);
            }
            (latest, recent, digests, sources, pinned)
        };
        let shown = latest
            .as_ref()
            .map(|h| handoff_items(&h.content_md))
            .unwrap_or_default();
        let carried: Carried = carry(&sources, &shown);
        let pinned = self.pinned_pages(&pinned);
        let prefix = format!("{}/", project.id);
        let recent: Vec<StateSession> = recent
            .into_iter()
            .map(|(s, title)| {
                let path = session_page_path(&s);
                StateSession {
                    date: display_date(&s.started_at),
                    agent: s.agent.clone(),
                    lane: s.lane.clone(),
                    machine: s.machine.clone(),
                    title,
                    rel_path: path.strip_prefix(&prefix).unwrap_or(&path).to_string(),
                }
            })
            .collect();
        let hot = aggregate_files(&digests, STATE_SESSIONS);
        let fm = Frontmatter {
            title: state_title(lang, &project.name),
            project: Some(project.id.clone()),
            scope: PageScope::Project,
            kind: PageKind::State,
            ..Frontmatter::default()
        };
        self.prepare_page(
            &format!("{}/STATE.md", project.id),
            fm,
            &state_body(lang, latest.as_ref(), &carried, &pinned, &recent, &hot),
            true,
        )
    }

    fn state_excerpt(&self, project: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dirs.wiki().join(project).join("STATE.md")).ok()?;
        let page = Page::parse(&format!("{project}/STATE.md"), &text).ok()?;
        let excerpt: Vec<&str> = page.body.lines().take(STATE_EXCERPT_LINES).collect();
        Some(excerpt.join("\n"))
    }
}

/// An alias resolved to its canonical project id; any other id unchanged.
fn resolve_id(conn: &Connection, id: &str) -> Result<String> {
    Ok(db::resolve_alias(conn, id)?.unwrap_or_else(|| id.to_string()))
}

/// The project id a session start is recorded under (M2.4 §2.1): an alias resolves to its
/// project; an unknown remote-derived id whose root is the root of a known path-derived
/// project (no remote) becomes an alias of that project (the checkout just gained a remote).
fn canonical_for_start(conn: &Connection, identity: &ProjectIdentity, now: &str) -> Result<String> {
    let id = resolve_id(conn, &identity.id)?;
    if db::get_project(conn, &id)?.is_some() || !is_remote_derived(identity) {
        return Ok(id);
    }
    let root = comparable_root(&identity.root);
    let existing = db::list_projects(conn)?.into_iter().find(|p| {
        p.remote_url.is_none()
            && is_derived_id(&p.id)
            && p.root_path
                .as_deref()
                .is_some_and(|r| comparable_root(r) == root)
    });
    match existing {
        Some(p) => {
            db::upsert_alias(conn, &identity.id, &p.id, now)?;
            tracing::info!(alias = %identity.id, project = %p.id, "project gained a remote; recorded an alias");
            Ok(p.id)
        }
        None => Ok(id),
    }
}

/// Peeks at the pending handoff for `lane` (§1.4) without accepting anything.
fn peek_pending(conn: &Connection, project: &str, lane: Option<&str>) -> Result<PendingHandoff> {
    let handoff = db::newest_handoff(conn, project, lane, true)?;
    let reference_handoff = match (&handoff, lane) {
        (None, Some(_)) => db::newest_handoff(conn, project, None, true)?,
        _ => None,
    };
    Ok(PendingHandoff {
        handoff,
        reference_handoff,
    })
}

/// §1.4 routing with acceptance: the newest pending handoff on `lane` is accepted by `by`
/// (older pending ones of that lane are superseded); on a branch lane without one, the
/// project lane's pending handoff is returned as a reference and left pending.
fn route_pending(
    conn: &Connection,
    project: &str,
    lane: Option<&str>,
    by: &str,
    now: &str,
) -> Result<PendingHandoff> {
    let mut routed = peek_pending(conn, project, lane)?;
    if let Some(h) = routed.handoff.take() {
        db::accept_pending_handoffs(conn, project, lane, by, now)?;
        routed.handoff = db::get_handoff(conn, &h.id)?;
    }
    Ok(routed)
}

/// Recent substantive sessions with their page titles (sessions without a page are skipped).
fn recent_sessions(
    conn: &Connection,
    project: &str,
    include: Option<&str>,
    limit: usize,
) -> Result<Vec<(Session, String)>> {
    let mut out = Vec::new();
    for s in db::recent_substantive_sessions(conn, project, include, limit)? {
        if let Some(page) = db::get_page(conn, &session_page_path(&s))? {
            out.push((s, page.title));
        }
    }
    Ok(out)
}

fn recent_entry((s, title): (Session, String)) -> RecentSession {
    RecentSession {
        title,
        path: session_page_path(&s),
        date: display_date(&s.started_at),
        agent: s.agent,
        lane: s.lane,
        machine: s.machine,
    }
}

/// The root a session's paths are relative to: its own (per machine), else the project's.
fn session_root(conn: &Connection, session: &Session) -> Result<Option<String>> {
    if let Some(root) = session.root_path.clone().filter(|r| !r.is_empty()) {
        return Ok(Some(root));
    }
    Ok(db::get_project(conn, &session.project_id)?.and_then(|p| p.root_path))
}

/// A session digest built from scratch (no cache involved).
fn digest_for(conn: &Connection, session: &Session) -> Result<SessionDigest> {
    let root = session_root(conn, session)?;
    let observations = db::list_observations(conn, &session.id)?;
    let mut digest = SessionDigest::from_observations(&observations, root.as_deref());
    digest.agent_handoff =
        db::newest_session_handoff(conn, &session.id, Some(HandoffSource::Agent), false)?;
    Ok(digest)
}

/// A page rendered and ready to be written by [`Store::store_pages`].
#[derive(Clone, Debug)]
struct PageWrite {
    path: String,
    page: Page,
    text: String,
}

/// `sessions.digest_json` (SPEC-M2.8 §1): the digest as of `digest_seq` (without the
/// agent handoff, which is read fresh), and the addendum digest of the observations after
/// the agent handoff `handoff_id` (seq > `after`) when one was needed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DigestCache {
    full: SessionDigest,
    #[serde(default)]
    delta: Option<DeltaCache>,
}

/// The cached addendum digest of [`DigestCache`].
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeltaCache {
    handoff_id: String,
    after: i64,
    digest: SessionDigest,
}

/// The session's digest up to observation `upto` (and, with `with_delta`, the digest of the
/// observations after its agent handoff), from the cache extended with only the newer
/// observations; a session without a usable cache is digested from scratch once. The cache
/// is updated. Equal to [`digest_for`] by construction ([`SessionDigest::extend`]).
fn session_digests(
    conn: &Connection,
    session: &Session,
    upto: i64,
    with_delta: bool,
) -> Result<(SessionDigest, Option<SessionDigest>)> {
    let root = session_root(conn, session)?;
    let stored = db::digest_cache(conn, &session.id)?;
    let cache = stored
        .as_ref()
        .and_then(|(json, seq)| {
            serde_json::from_str::<DigestCache>(json)
                .ok()
                .map(|c| (c, *seq))
        })
        // A different root (the project's fallback root moved) or a cache ahead of `upto`
        // cannot be extended: start over.
        .filter(|(c, seq)| c.full.tally.root == root && *seq <= upto);
    let (mut full, delta_cache, from) = match cache {
        Some((c, seq)) => (c.full, c.delta, seq),
        None => (
            SessionDigest::from_observations(&[], root.as_deref()),
            None,
            0,
        ),
    };
    let new = db::list_observations_between(conn, &session.id, from, upto)?;
    full.extend(&new);
    let agent = db::newest_session_handoff(conn, &session.id, Some(HandoffSource::Agent), false)?;
    let mark = match (&agent, with_delta) {
        (Some(_), true) => db::agent_handoff_mark(conn, &session.id)?,
        _ => None,
    };
    let delta = match (&agent, mark) {
        (Some(h), Some(mark)) => Some(match (delta_cache, mark.seq_at) {
            (Some(d), Some(after)) if d.handoff_id == h.id && d.after == after => {
                let mut digest = d.digest;
                let newer: Vec<Observation> =
                    new.iter().filter(|o| o.seq > after).cloned().collect();
                digest.extend(&newer);
                DeltaCache {
                    handoff_id: h.id.clone(),
                    after,
                    digest,
                }
            }
            (_, seq_at) => {
                let obs: Vec<Observation> =
                    db::list_observations_after(conn, &session.id, Some(&mark))?
                        .into_iter()
                        .filter(|o| o.seq <= upto)
                        .collect();
                DeltaCache {
                    handoff_id: h.id.clone(),
                    // Without `seq_at` (rows from before it existed) never reused.
                    after: seq_at.unwrap_or(-1),
                    digest: SessionDigest::from_observations(&obs, root.as_deref()),
                }
            }
        }),
        _ => None,
    };
    let json = serde_json::to_string(&DigestCache {
        full: full.clone(),
        delta: delta.clone(),
    })
    .context("serializing the session digest")?;
    if stored.as_ref().map(|(j, s)| (j.as_str(), *s)) != Some((json.as_str(), upto)) {
        db::set_digest_cache(conn, &session.id, &json, upto)?;
    }
    full.agent_handoff = agent;
    Ok((full, delta.map(|d| d.digest)))
}

/// The digest of a finalized session for STATE.md: the cache as is when it is current,
/// else [`session_digests`] (which brings it up to date).
fn cached_digest(conn: &Connection, session: &Session) -> Result<SessionDigest> {
    let upto = db::max_seq(conn, &session.id)?;
    if let Some((json, seq)) = db::digest_cache(conn, &session.id)?
        && seq == upto
        && let Ok(cache) = serde_json::from_str::<DigestCache>(&json)
        && cache.full.tally.root == session_root(conn, session)?
    {
        let mut full = cache.full;
        full.agent_handoff =
            db::newest_session_handoff(conn, &session.id, Some(HandoffSource::Agent), false)?;
        return Ok(full);
    }
    Ok(session_digests(conn, session, upto, false)?.0)
}

fn page_records(page: &Page, text: &str) -> (PageRow, IndexDoc) {
    let fm = &page.frontmatter;
    let project_id = match fm.scope {
        PageScope::Global => None,
        PageScope::Project => fm
            .project
            .clone()
            .or_else(|| page.path.split('/').next().map(str::to_string)),
    };
    let updated = util::parse_ts(&fm.updated).unwrap_or_else(util::now);
    let row = PageRow {
        path: page.path.clone(),
        project_id: project_id.clone(),
        scope: fm.scope.as_str().to_string(),
        kind: fm.kind.as_str().to_string(),
        title: fm.title.clone(),
        tags: fm.tags.clone(),
        created_at: fm.created.clone(),
        updated_at: fm.updated.clone(),
        hash: sha256_hex(text),
    };
    let doc = IndexDoc {
        path: page.path.clone(),
        project_id,
        scope: fm.scope.as_str().to_string(),
        kind: fm.kind.as_str().to_string(),
        title: fm.title.clone(),
        body: page.body.clone(),
        tags: fm.tags.clone(),
        updated,
    };
    (row, doc)
}

/// Every `*.md` under the wiki (hidden directories skipped), as `/`-separated relative paths.
fn list_wiki_pages(wiki: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![wiki.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("listing {}", dir.display()))
                    .into());
            }
        };
        for entry in entries {
            let entry = entry.context("reading directory entry")?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if name.ends_with(".md")
                && let Ok(rel) = path.strip_prefix(wiki)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Where each page of `from` goes in a merge: `from/<rest>` → `into/<rest>`, or
/// `into/<dir>/<stem>-<from>[-N].md` when that name is taken. `from/STATE.md` is not moved.
fn plan_page_moves(wiki: &Path, from: &str, into: &str) -> Result<Vec<(String, String)>> {
    let prefix = format!("{from}/");
    let mut taken: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for rel in list_wiki_pages(wiki)? {
        let Some(rest) = rel.strip_prefix(&prefix) else {
            continue;
        };
        if rest == "STATE.md" {
            continue;
        }
        let free = |p: &str| !wiki.join(p).exists() && !taken.iter().any(|t| t == p);
        let mut new = format!("{into}/{rest}");
        if !free(&new) {
            let stem = rest.strip_suffix(".md").unwrap_or(rest);
            new = format!("{into}/{stem}-{from}.md");
            let mut n = 2;
            while !free(&new) {
                new = format!("{into}/{stem}-{from}-{n}.md");
                n += 1;
            }
        }
        taken.push(new.clone());
        out.push((rel, new));
    }
    Ok(out)
}

/// Moves one page file, rewriting `project: <from>` in its frontmatter to `into`.
fn move_page(wiki: &Path, old: &str, new: &str, from: &str, into: &str) -> Result<()> {
    let src = wiki.join(old);
    let text =
        std::fs::read_to_string(&src).with_context(|| format!("reading {}", src.display()))?;
    let text = match Page::parse(old, &text) {
        Ok(mut page) => {
            if page
                .frontmatter
                .project
                .as_deref()
                .is_none_or(|p| p == from)
                && page.frontmatter.scope == PageScope::Project
            {
                page.frontmatter.project = Some(into.to_string());
            }
            page.path = new.to_string();
            page.render()?
        }
        Err(_) => text,
    };
    write_atomic(&wiki.join(new), &text)?;
    std::fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
    Ok(())
}

/// Moves `raw/<from>/*.jsonl` to `raw/<into>/` (appending when a file exists on both sides).
fn move_raw_logs(from: &Path, into: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    std::fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;
    for entry in entries.flatten() {
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let dst = into.join(entry.file_name());
        if dst.exists() {
            let text = std::fs::read_to_string(&src)
                .with_context(|| format!("reading {}", src.display()))?;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&dst)
                .with_context(|| format!("opening {}", dst.display()))?;
            f.write_all(text.as_bytes())
                .with_context(|| format!("appending to {}", dst.display()))?;
            std::fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
        } else {
            std::fs::rename(&src, &dst)
                .with_context(|| format!("moving {} to {}", src.display(), dst.display()))?;
        }
    }
    remove_empty_dirs(from);
    Ok(())
}

/// Removes `dir` and its subdirectories as far as they are empty (errors are ignored).
fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Writes `file` through a hidden `.<name>.<id>.tmp` sibling, fsync and rename (then fsyncs
/// the directory on Unix). A crash leaves at most the temporary file, which the startup
/// sweep removes.
fn write_atomic(file: &Path, text: &str) -> Result<()> {
    let parent = file.parent().context("page path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        file.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        util::new_id()
    ));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .and_then(|f| f.sync_all())
        .context("syncing page")?;
    std::fs::rename(&tmp, file).with_context(|| format!("renaming into {}", file.display()))?;
    // The rename itself is durable only once the directory entry is (SPEC-M2.8 §5).
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

fn append_line(file: &Path, line: &str) -> anyhow::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn non_empty_or(s: &str, default: &str) -> String {
    if s.trim().is_empty() {
        default.to_string()
    } else {
        s.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_store(dir: &Path) -> Store {
        Store::open(Config::for_data_dir(dir)).unwrap()
    }

    fn project() -> ProjectIdentity {
        ProjectIdentity {
            id: "kioku-3f9a1c2e".into(),
            name: "kioku".into(),
            root: "/home/u/kioku".into(),
            remote: Some("github.com/u/kioku".into()),
        }
    }

    fn start(store: &Store, session: &str) -> SessionStartResponse {
        store
            .start_session(&SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: "/home/u/kioku".into(),
                source: "startup".into(),
                project: project(),
                lane: None,
            })
            .unwrap()
    }

    fn observe(store: &Store, session: &str, kind: ObservationKind, payload: serde_json::Value) {
        store
            .add_observation(&NewObservation {
                event_id: None,
                session_id: session.into(),
                kind,
                ts: None,
                payload,
            })
            .unwrap();
    }

    fn work(store: &Store, session: &str) {
        observe(
            store,
            session,
            ObservationKind::Prompt,
            json!({"prompt": "検索のテストを追加して"}),
        );
        observe(
            store,
            session,
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit", "tool_input": {"file_path": "/home/u/kioku/src/index.rs"}, "tool_response": {}}),
        );
        observe(
            store,
            session,
            ObservationKind::ToolUse,
            json!({"tool_name": "Bash", "tool_input": {"command": "git commit -m \"feat: 検索\""}, "tool_response": {"stdout": "ok"}}),
        );
    }

    fn agent_handoff(project: &str, session: Option<&str>, summary: &str) -> HandoffInput {
        HandoffInput {
            gotchas: Vec::new(),
            verified: Vec::new(),
            project: project.into(),
            session: session.map(str::to_string),
            summary: summary.into(),
            next_steps: vec!["テストを書く".into()],
            open_questions: vec![],
            decisions: vec!["lindera を採用".into()],
        }
    }

    fn start_on(store: &Store, session: &str, lane: Option<&str>) -> SessionStartResponse {
        store
            .start_session(&SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: "/home/u/kioku".into(),
                source: "startup".into(),
                project: project(),
                lane: lane.map(str::to_string),
            })
            .unwrap()
    }

    fn handoff_row(store: &Store, id: &str) -> Handoff {
        db::get_handoff(&store.db.lock(), id).unwrap().unwrap()
    }

    #[test]
    fn handoffs_are_routed_per_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let pid = project().id;

        // Two worktrees on two branches plus the default branch.
        start_on(&store, "a-1", Some("task-a"));
        start_on(&store, "b-1", Some("task-b"));
        start_on(&store, "main-1", None);
        let ha = store
            .write_handoff(&agent_handoff(&pid, Some("a-1"), "A の作業"))
            .unwrap();
        assert_eq!(ha.lane.as_deref(), Some("task-a"));
        let hb = store
            .write_handoff(&agent_handoff(&pid, Some("b-1"), "B の作業"))
            .unwrap();
        let hm = store
            .write_handoff(&agent_handoff(&pid, Some("main-1"), "メインの作業"))
            .unwrap();
        assert_eq!(hm.lane, None);
        // without a session the handoff goes to the project lane, even though it attaches
        // to the newest open session
        let loose = store
            .write_handoff(&agent_handoff(&pid, None, "セッションなし"))
            .unwrap();
        assert_eq!(loose.lane, None);

        // same lane: accepted; another lane's handoff is untouched
        let a2 = start_on(&store, "a-2", Some("task-a"));
        assert_eq!(a2.lane.as_deref(), Some("task-a"));
        assert_eq!(a2.pending_handoff.unwrap().id, ha.id);
        assert!(a2.reference_handoff.is_none());
        assert!(handoff_row(&store, &hb.id).accepted_at.is_none());
        assert!(handoff_row(&store, &loose.id).accepted_at.is_none());

        // the project lane gets the newest project-lane handoff and supersedes only its lane
        let m2 = start_on(&store, "main-2", None);
        assert_eq!(m2.lane, None);
        assert_eq!(m2.pending_handoff.unwrap().id, loose.id);
        assert_eq!(
            handoff_row(&store, &hm.id).accepted_by.as_deref(),
            Some("main-2")
        );
        assert!(handoff_row(&store, &hb.id).accepted_at.is_none());
        assert!(start_on(&store, "main-3", None).pending_handoff.is_none());

        let b2 = start_on(&store, "b-2", Some("task-b"));
        assert_eq!(b2.pending_handoff.unwrap().id, hb.id);

        // a new branch without its own handoff sees the project lane's as a reference only
        let main_next = store
            .write_handoff(&agent_handoff(&pid, Some("main-3"), "次はリリース"))
            .unwrap();
        let c1 = start_on(&store, "c-1", Some("task-c"));
        assert!(c1.pending_handoff.is_none());
        let reference = c1.reference_handoff.unwrap();
        assert_eq!(reference.id, main_next.id);
        assert!(reference.accepted_at.is_none());
        assert!(handoff_row(&store, &main_next.id).accepted_at.is_none());
        // ... and the context of that session repeats it, still without consuming it
        let ctx = store.session_context("c-1").unwrap();
        assert_eq!(ctx.lane.as_deref(), Some("task-c"));
        assert_eq!(ctx.reference_handoff.unwrap().id, main_next.id);
        // the default branch still receives it
        let m4 = start_on(&store, "main-4", None);
        assert_eq!(m4.pending_handoff.unwrap().id, main_next.id);
        assert!(m4.reference_handoff.is_none());
        let c2 = start_on(&store, "c-2", Some("task-c"));
        assert!(c2.pending_handoff.is_none() && c2.reference_handoff.is_none());

        // pending lookups: project lane by default, a lane by name or by session
        let hb2 = store
            .write_handoff(&agent_handoff(&pid, Some("b-2"), "B の続き"))
            .unwrap();
        assert!(store.pending_handoff(&pid, false, None).unwrap().is_none());
        let by_lane = store
            .pending_handoff_routed(&pid, false, None, Some("task-b"))
            .unwrap();
        assert_eq!(by_lane.handoff.unwrap().id, hb2.id);
        let by_session = store
            .pending_handoff_routed(&pid, true, Some("b-2"), None)
            .unwrap();
        assert_eq!(
            by_session.handoff.unwrap().accepted_by.as_deref(),
            Some("b-2")
        );
        let main_again = store
            .write_handoff(&agent_handoff(&pid, Some("main-4"), "メイン"))
            .unwrap();
        let routed = store
            .pending_handoff_routed(&pid, true, None, Some("task-z"))
            .unwrap();
        assert!(routed.handoff.is_none());
        assert_eq!(routed.reference_handoff.unwrap().id, main_again.id);
        assert!(
            handoff_row(&store, &main_again.id).accepted_at.is_none(),
            "a reference is never accepted, even with accept=true"
        );
        let explicit_main = store
            .pending_handoff_routed(&pid, false, Some("b-2"), Some(""))
            .unwrap();
        assert_eq!(explicit_main.handoff.unwrap().id, main_again.id);
    }

    #[test]
    fn lanes_show_in_session_pages_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start_on(&store, "lane-s", Some("feature/検索"));
        work(&store, "lane-s");
        let r = store.finalize_session("lane-s").unwrap();
        let rules = handoff_row(&store, r.handoff_id.as_deref().unwrap());
        assert_eq!(rules.lane.as_deref(), Some("feature/検索"));
        let page = store.read_page(r.session_page.as_deref().unwrap()).unwrap();
        assert_eq!(page.frontmatter.lane.as_deref(), Some("feature/検索"));
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("wiki")
                .join(r.session_page.as_deref().unwrap()),
        )
        .unwrap();
        assert!(raw.contains("lane: feature/検索"), "{raw}");
        let state = store.read_page("kioku-3f9a1c2e/STATE.md").unwrap();
        assert!(
            state
                .body
                .contains(" claude-code [feature/検索] — 検索のテストを追加して"),
            "{}",
            state.body
        );
        // the branch's handoff is not STATE.md's "latest handoff" (every lane reads STATE.md)
        assert!(!state.body.contains("source: rules"), "{}", state.body);
        // lanes never hide memory
        let hits = store
            .search("検索のテスト", &SearchScope::Project(project().id), 10)
            .unwrap();
        assert!(
            hits.iter()
                .any(|h| Some(&h.path) == r.session_page.as_ref())
        );

        // a project-lane session page has no lane key
        start_on(&store, "main-s", None);
        work(&store, "main-s");
        let r = store.finalize_session("main-s").unwrap();
        let page = store.read_page(r.session_page.as_deref().unwrap()).unwrap();
        assert_eq!(page.frontmatter.lane, None);
        let state = store.read_page("kioku-3f9a1c2e/STATE.md").unwrap();
        assert!(state.body.contains("source: rules"), "{}", state.body);
    }

    fn identity(id: &str, root: &str, remote: Option<&str>) -> ProjectIdentity {
        ProjectIdentity {
            id: id.into(),
            name: crate::project::root_basename(root),
            root: root.into(),
            remote: remote.map(str::to_string),
        }
    }

    fn start_with(store: &Store, session: &str, p: ProjectIdentity) -> SessionStartResponse {
        store
            .start_session(&SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: "codex".into(),
                cwd: p.root.clone(),
                source: "startup".into(),
                project: p,
                lane: None,
            })
            .unwrap()
    }

    fn path_identity(root: &str) -> ProjectIdentity {
        let name = crate::project::root_basename(root);
        identity(
            &crate::project::id_from_path(&name, Path::new(root)),
            root,
            None,
        )
    }

    fn remote_identity(root: &str, remote: &str) -> ProjectIdentity {
        let name = crate::project::root_basename(root);
        identity(
            &crate::project::id_from_remote(&name, remote),
            root,
            Some(remote),
        )
    }

    #[test]
    fn a_checkout_that_gains_a_remote_keeps_its_project() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let root = "/Users/u/AI_agents_shared_memory";
        let old = path_identity(root);
        start_with(&store, "before", old.clone());
        work(&store, "before");
        store.finalize_session("before").unwrap();
        let page = store
            .write_page(&WritePageRequest {
                title: "設計メモ".into(),
                content: "引き継ぎのレーンを設計した".into(),
                project: Some(old.id.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();

        // `origin` is added: the client now computes a remote-derived id
        let new = remote_identity(&format!("{root}/"), "github.com/misorafa/kioku");
        assert_ne!(new.id, old.id);
        let resp = start_with(&store, "after", new.clone());
        assert_eq!(resp.project_id, old.id, "the canonical id is reported back");
        assert!(
            resp.pending_handoff.is_some(),
            "the old handoff is delivered"
        );
        assert_eq!(resp.recent_sessions.len(), 1);
        let row = store.project(&old.id).unwrap();
        assert_eq!(row.remote_url.as_deref(), Some("github.com/misorafa/kioku"));
        assert_eq!(store.session("after").unwrap().project_id, old.id);
        assert_eq!(store.list_projects().unwrap().len(), 1);
        assert_eq!(
            store.status().unwrap().aliases,
            vec![ProjectAlias {
                alias: new.id.clone(),
                project_id: old.id.clone()
            }]
        );

        // another machine's clone (only ever the remote id) lands in the same project
        let clone = remote_identity(r"C:\src\kioku", "github.com/misorafa/kioku");
        assert_eq!(clone.id, new.id);
        assert_eq!(start_with(&store, "win", clone).project_id, old.id);

        // the alias resolves in search, pages, handoffs and project lookups
        assert_eq!(store.project(&new.id).unwrap().id, old.id);
        let hits = store
            .search("レーン", &SearchScope::Project(new.id.clone()), 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.path == page), "{hits:?}");
        let p2 = store
            .write_page(&WritePageRequest {
                title: "別名で書く".into(),
                content: "エイリアス経由".into(),
                project: Some(new.id.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();
        assert!(p2.starts_with(&format!("{}/pages/", old.id)), "{p2}");
        let h = store
            .write_handoff(&agent_handoff(&new.id, Some("win"), "別名の引き継ぎ"))
            .unwrap();
        assert_eq!(h.project_id, old.id);
        assert_eq!(
            store
                .pending_handoff(&new.id, false, None)
                .unwrap()
                .unwrap()
                .id,
            h.id
        );
        assert_eq!(store.latest_handoff(&new.id).unwrap().unwrap().id, h.id);
    }

    #[test]
    fn aliases_are_only_made_for_the_same_root_and_a_git_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let old = path_identity(r"\\?\C:\work\notes");
        start_with(&store, "s1", old.clone());

        // a different root → a new project
        let elsewhere = remote_identity(r"C:\work\other", "github.com/u/notes");
        assert_eq!(
            start_with(&store, "s2", elsewhere.clone()).project_id,
            elsewhere.id
        );

        // `.kioku.toml` id on the same root → no alias, its own project
        let toml = identity("my-notes", r"C:\work\notes", Some("github.com/u/notes"));
        assert_eq!(start_with(&store, "s3", toml).project_id, "my-notes");
        assert!(store.list_aliases().unwrap().is_empty());

        // same root (verbatim prefix and trailing separator ignored) → alias
        let gained = remote_identity(r"C:\work\notes\", "github.com/u/notes2");
        assert_eq!(start_with(&store, "s4", gained.clone()).project_id, old.id);
        assert_eq!(store.resolve_project_id(&gained.id).unwrap(), old.id);

        // a project that already has a remote is never aliased again
        let changed = remote_identity(r"C:\work\notes", "gitlab.com/u/notes3");
        assert_eq!(
            start_with(&store, "s5", changed.clone()).project_id,
            changed.id
        );
        assert_eq!(store.list_aliases().unwrap().len(), 1);
    }

    #[test]
    fn merge_moves_everything_and_is_safe_to_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let from = path_identity("/Users/u/kioku");
        let into = remote_identity("/Users/u/AI_agents_shared_memory", "github.com/u/ai-agents");
        start_with(&store, "old-s", from.clone());
        work(&store, "old-s");
        store.finalize_session("old-s").unwrap();
        store
            .write_page(&WritePageRequest {
                title: "Design Notes".into(),
                content: "古いプロジェクトの設計メモ".into(),
                project: Some(from.id.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();
        start_with(&store, "new-s", into.clone());
        store
            .write_page(&WritePageRequest {
                title: "Design Notes".into(),
                content: "新しい方".into(),
                project: Some(into.id.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();

        // errors
        for (a, b) in [
            ("unknown-00000000", into.id.as_str()),
            (from.id.as_str(), "unknown-00000000"),
            (into.id.as_str(), into.id.as_str()),
            ("../x", into.id.as_str()),
        ] {
            assert!(store.merge_projects(a, b, false).is_err(), "{a} → {b}");
        }

        // dry run lists without changing
        let dry = store.merge_projects(&from.id, &into.id, true).unwrap();
        assert!(dry.dry_run && !dry.already_merged);
        assert_eq!((dry.sessions, dry.handoffs), (1, 1));
        assert_eq!(dry.observations, 3);
        assert_eq!(dry.pages.len(), 2, "{:?}", dry.pages);
        assert!(dry.pages.contains(&(
            format!("{}/pages/design-notes.md", from.id),
            format!("{}/pages/design-notes-{}.md", into.id, from.id)
        )));
        assert!(store.project(&from.id).is_ok());
        assert!(tmp.path().join("wiki").join(&from.id).is_dir());

        let report = store.merge_projects(&from.id, &into.id, false).unwrap();
        assert_eq!(report.pages, dry.pages);
        assert!(!report.dry_run);
        assert!(!tmp.path().join("wiki").join(&from.id).exists());
        assert!(!tmp.path().join("raw").join(&from.id).exists());
        assert!(
            tmp.path()
                .join("raw")
                .join(&into.id)
                .join("old-s.jsonl")
                .is_file()
        );
        assert_eq!(store.session("old-s").unwrap().project_id, into.id);
        assert_eq!(store.observations("old-s").unwrap()[0].project_id, into.id);
        assert_eq!(
            store.latest_handoff(&into.id).unwrap().unwrap().project_id,
            into.id
        );
        assert_eq!(store.resolve_project_id(&from.id).unwrap(), into.id);
        assert_eq!(store.list_projects().unwrap().len(), 1);
        let moved = store
            .read_page(&format!("{}/pages/design-notes-{}.md", into.id, from.id))
            .unwrap();
        assert_eq!(moved.frontmatter.project.as_deref(), Some(into.id.as_str()));
        // reindexed: the moved pages are found under the target project, in Japanese
        let hits = store
            .search("設計メモ", &SearchScope::Project(into.id.clone()), 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.path == moved.path), "{hits:?}");
        assert!(hits.iter().all(|h| !h.path.starts_with(&from.id)));
        // STATE.md of the target now lists the moved session
        let state = store.read_page(&format!("{}/STATE.md", into.id)).unwrap();
        assert!(
            state.body.contains("検索のテストを追加して"),
            "{}",
            state.body
        );
        // the old id keeps working for clients that still send it
        assert_eq!(start_with(&store, "late", from.clone()).project_id, into.id);

        // repeating is harmless
        let again = store.merge_projects(&from.id, &into.id, false).unwrap();
        assert!(again.already_merged);
        assert!(again.pages.is_empty());

        if crate::git::git_available() {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path().join("wiki"))
                .args(["status", "--porcelain"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "",
                "wiki is committed"
            );
            let log = std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path().join("wiki"))
                .args(["log", "--format=%s"])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&log.stdout).contains(&format!(
                "kioku: merge project {} into {}",
                from.id, into.id
            )));
        }
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Store>();
    }

    #[test]
    fn handoff_single_use_and_supersede() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        store.register_project(&project()).unwrap();
        let pid = project().id;
        let older = store
            .write_handoff(&agent_handoff(&pid, None, "古い引き継ぎ"))
            .unwrap();
        let newer = store
            .write_handoff(&agent_handoff(&pid, None, "新しい引き継ぎ"))
            .unwrap();
        assert!(newer.content_md.contains("### 要約\n新しい引き継ぎ"));

        // peek does not consume
        for _ in 0..2 {
            let peek = store.pending_handoff(&pid, false, None).unwrap().unwrap();
            assert_eq!(peek.id, newer.id);
            assert!(peek.accepted_at.is_none());
        }

        // session start consumes the newest and supersedes the older one
        let resp = start(&store, "sess-a");
        let got = resp.pending_handoff.unwrap();
        assert_eq!(got.id, newer.id);
        assert_eq!(got.accepted_by.as_deref(), Some("sess-a"));
        assert!(got.accepted_at.is_some());
        assert!(store.pending_handoff(&pid, false, None).unwrap().is_none());
        assert!(start(&store, "sess-b").pending_handoff.is_none());
        let conn = store.db.lock();
        let old = db::get_handoff(&conn, &older.id).unwrap().unwrap();
        assert_eq!(old.accepted_by.as_deref(), Some("sess-a"));
        drop(conn);

        // explicit accept via the API path
        let third = store
            .write_handoff(&agent_handoff(&pid, Some("sess-b"), "三つ目"))
            .unwrap();
        assert_eq!(third.session_id.as_deref(), Some("sess-b"));
        assert_eq!(third.agent.as_deref(), Some("claude-code"));
        let acc = store
            .pending_handoff(&pid, true, Some("sess-c"))
            .unwrap()
            .unwrap();
        assert_eq!(acc.id, third.id);
        assert_eq!(acc.accepted_by.as_deref(), Some("sess-c"));
        assert!(store.pending_handoff(&pid, true, None).unwrap().is_none());

        // errors
        assert!(matches!(
            store.write_handoff(&agent_handoff("nope", None, "x")),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            store.write_handoff(&agent_handoff(&pid, None, "  ")),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn handoff_without_session_attaches_to_newest_open_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "sess-1");
        start(&store, "sess-2");
        let h = store
            .write_handoff(&agent_handoff(&project().id, None, "要約"))
            .unwrap();
        assert_eq!(h.session_id.as_deref(), Some("sess-2"));
        assert!(store.session_info("sess-2").unwrap().has_agent_handoff);
        assert!(!store.session_info("sess-1").unwrap().has_agent_handoff);
    }

    #[test]
    fn finalize_is_idempotent_and_writes_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "0c2f1a2b-aaaa");
        work(&store, "0c2f1a2b-aaaa");
        let info = store.session_info("0c2f1a2b-aaaa").unwrap();
        assert_eq!(info.counts.prompts, 1);
        assert_eq!(info.counts.tool_uses, 2);
        assert_eq!(info.status, SessionStatus::Open);
        assert!(!info.has_agent_handoff);

        let first = store.finalize_session("0c2f1a2b-aaaa").unwrap();
        assert!(first.substantive);
        let page_path = first.session_page.clone().unwrap();
        assert!(page_path.starts_with("kioku-3f9a1c2e/sessions/"));
        assert!(page_path.ends_with(&format!(
            "-0c2f1a2b-{}.md",
            &sha256_hex("0c2f1a2b-aaaa")[..12]
        )));
        let page = store.read_page(&page_path).unwrap();
        assert_eq!(page.frontmatter.kind, PageKind::Session);
        assert_eq!(page.frontmatter.tags, vec!["claude-code"]);
        assert_eq!(page.frontmatter.session.as_deref(), Some("0c2f1a2b-aaaa"));
        assert!(
            page.frontmatter
                .title
                .ends_with("claude-code — 検索のテストを追加して")
        );
        assert!(page.body.contains("- src/index.rs (1)"));
        assert!(page.body.contains("## 引き継ぎ（自動生成）"));
        let handoff = store.latest_handoff(&project().id).unwrap().unwrap();
        assert_eq!(Some(handoff.id.clone()), first.handoff_id);
        assert_eq!(handoff.source, HandoffSource::Rules);
        let state = store.read_page("kioku-3f9a1c2e/STATE.md").unwrap();
        assert_eq!(state.frontmatter.kind, PageKind::State);
        assert_eq!(state.frontmatter.title, "kioku — 現在の状態");
        assert!(state.body.contains("source: rules"));
        assert!(state.body.contains("— 検索のテストを追加して (sessions/"));
        assert!(state.body.contains("- src/index.rs (1)"));
        assert_eq!(
            store.session_info("0c2f1a2b-aaaa").unwrap().status,
            SessionStatus::Finalized
        );

        let raw_page = std::fs::read_to_string(tmp.path().join("wiki").join(&page_path)).unwrap();
        let handoffs_before = store.status().unwrap().handoffs;
        let second = store.finalize_session("0c2f1a2b-aaaa").unwrap();
        assert_eq!(second, first);
        assert_eq!(store.status().unwrap().handoffs, handoffs_before);
        let raw_again = std::fs::read_to_string(tmp.path().join("wiki").join(&page_path)).unwrap();
        assert_eq!(raw_page, raw_again);

        // the page is searchable in Japanese
        let hits = store
            .search("検索のテスト", &SearchScope::Project(project().id), 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.path == page_path));

        // a new turn reopens the session; re-finalize refreshes the same rules handoff
        observe(
            &store,
            "0c2f1a2b-aaaa",
            ObservationKind::Prompt,
            json!({"prompt": "次はドキュメント"}),
        );
        assert_eq!(
            store.session_info("0c2f1a2b-aaaa").unwrap().status,
            SessionStatus::Open
        );
        let third = store.finalize_session("0c2f1a2b-aaaa").unwrap();
        assert_eq!(third.handoff_id, first.handoff_id);
        assert_eq!(third.session_page, first.session_page);
        let refreshed = store.latest_handoff(&project().id).unwrap().unwrap();
        assert!(
            refreshed
                .content_md
                .contains("最後の指示: 次はドキュメント")
        );
        let page = store.read_page(&page_path).unwrap();
        assert!(page.body.contains("2. 次はドキュメント"));
        assert_eq!(
            page.frontmatter.created,
            store.read_page(&page_path).unwrap().frontmatter.created
        );

        // the next session receives the rules handoff and STATE excerpt
        let resp = start(&store, "next-session");
        assert_eq!(resp.pending_handoff.unwrap().id, refreshed.id);
        assert!(resp.state_excerpt.unwrap().starts_with("## 最新の引き継ぎ"));
        assert_eq!(resp.recent_sessions.len(), 1);
        assert_eq!(resp.recent_sessions[0].path, page_path);
    }

    fn start_at(store: &Store, session: &str, root: &str) {
        let mut p = project();
        p.root = root.into();
        store
            .start_session(&SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: root.into(),
                source: "startup".into(),
                project: p,
                lane: None,
            })
            .unwrap();
    }

    fn tool(store: &Store, session: &str, path: &str) {
        observe(
            store,
            session,
            ObservationKind::ToolUse,
            json!({"tool_name": "Edit", "tool_input": {"file_path": path}, "tool_response": {}}),
        );
    }

    #[test]
    fn observation_during_finalize_keeps_session_open() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "race");
        work(&store, "race");
        let r = store
            .finalize_with("race", &|s| {
                observe(
                    s,
                    "race",
                    ObservationKind::Prompt,
                    json!({"prompt": "途中で届いた指示"}),
                )
            })
            .unwrap();
        assert!(r.substantive);
        assert_eq!(
            store.session_info("race").unwrap().status,
            SessionStatus::Open,
            "a late observation must not be swallowed by finalize"
        );
        // the next finalize includes it and closes the session
        let r2 = store.finalize_session("race").unwrap();
        let page = store
            .read_page(r2.session_page.as_deref().unwrap())
            .unwrap();
        assert!(page.body.contains("途中で届いた指示"));
        assert_eq!(
            store.session_info("race").unwrap().status,
            SessionStatus::Finalized
        );

        // same for a non-substantive session
        start(&store, "race-empty");
        store
            .finalize_with("race-empty", &|s| {
                observe(
                    s,
                    "race-empty",
                    ObservationKind::Prompt,
                    json!({"prompt": "x"}),
                )
            })
            .unwrap();
        assert_eq!(
            store.session_info("race-empty").unwrap().status,
            SessionStatus::Open
        );
    }

    #[test]
    fn handoff_without_session_prefers_session_with_newest_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "working");
        start(&store, "idle-but-newer");
        tool(&store, "working", "/home/u/kioku/a.rs");
        let h = store
            .write_handoff(&agent_handoff(&project().id, None, "作業中のセッション"))
            .unwrap();
        assert_eq!(h.session_id.as_deref(), Some("working"));
        tool(&store, "idle-but-newer", "/home/u/kioku/b.rs");
        let h = store
            .write_handoff(&agent_handoff(&project().id, None, "もう一方"))
            .unwrap();
        assert_eq!(h.session_id.as_deref(), Some("idle-but-newer"));
    }

    #[test]
    fn tool_uses_since_handoff_and_addendum() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let pid = project().id;
        start(&store, "s");
        work(&store, "s");
        tool(&store, "s", "/home/u/kioku/c.rs");
        let info = store.session_info("s").unwrap();
        assert_eq!(info.tool_uses_since_handoff, Some(3));
        let agent = store
            .write_handoff(&agent_handoff(&pid, Some("s"), "検索を実装した"))
            .unwrap();
        let info = store.session_info("s").unwrap();
        assert_eq!(info.tool_uses_since_handoff, Some(0));
        assert_eq!(info.counts.tool_uses, 3);

        // two more tool uses: still covered by the agent handoff
        tool(&store, "s", "/home/u/kioku/d.rs");
        tool(&store, "s", "/home/u/kioku/d.rs");
        let r = store.finalize_session("s").unwrap();
        assert_eq!(r.handoff_id.as_deref(), Some(agent.id.as_str()));

        // a third one makes the agent handoff stale → rules addendum for the delta
        observe(
            &store,
            "s",
            ObservationKind::Prompt,
            json!({"prompt": "ドキュメントも直して"}),
        );
        tool(&store, "s", "/home/u/kioku/README.md");
        assert_eq!(
            store.session_info("s").unwrap().tool_uses_since_handoff,
            Some(3)
        );
        let r = store.finalize_session("s").unwrap();
        let addendum_id = r.handoff_id.clone().unwrap();
        assert_ne!(addendum_id, agent.id);
        let pending = store.pending_handoff(&pid, false, None).unwrap().unwrap();
        assert_eq!(pending.id, addendum_id);
        assert_eq!(pending.source, HandoffSource::Rules);
        let md = &pending.content_md;
        assert!(md.contains("### 要約\n検索を実装した"), "{md}");
        assert!(md.contains("## 引き継ぎ（自動生成・追記）"), "{md}");
        assert!(md.contains("最後の指示: ドキュメントも直して"), "{md}");
        assert!(
            md.contains("d.rs (2)") && md.contains("README.md (1)"),
            "{md}"
        );
        assert!(!md.contains("index.rs"), "delta only: {md}");
        let page = store.read_page(r.session_page.as_deref().unwrap()).unwrap();
        assert!(page.body.contains("## 引き継ぎ（自動生成・追記）"));

        // re-finalize refreshes the same addendum, keeping created_at
        tool(&store, "s", "/home/u/kioku/e.rs");
        let r = store.finalize_session("s").unwrap();
        assert_eq!(r.handoff_id.as_deref(), Some(addendum_id.as_str()));
        let refreshed = store.pending_handoff(&pid, false, None).unwrap().unwrap();
        assert_eq!(refreshed.created_at, pending.created_at);
        assert!(refreshed.updated_at.is_some());
        assert!(refreshed.content_md.contains("e.rs (1)"));

        // a new agent handoff supersedes the addendum again
        let agent2 = store
            .write_handoff(&agent_handoff(&pid, Some("s"), "全部終わった"))
            .unwrap();
        assert_eq!(
            store.session_info("s").unwrap().tool_uses_since_handoff,
            Some(0)
        );
        let r = store.finalize_session("s").unwrap();
        assert_eq!(r.handoff_id.as_deref(), Some(agent2.id.as_str()));
        assert_eq!(
            store
                .pending_handoff(&pid, false, None)
                .unwrap()
                .unwrap()
                .id,
            agent2.id
        );
    }

    #[test]
    fn refreshed_rules_handoff_does_not_outrank_newer_agent_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let pid = project().id;
        start(&store, "b");
        start(&store, "a");
        work(&store, "a");
        let rules = store.finalize_session("a").unwrap().handoff_id.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let agent = store
            .write_handoff(&agent_handoff(&pid, Some("b"), "B の引き継ぎ"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        // session a keeps working and re-finalizes: its rules handoff is refreshed in place
        observe(
            &store,
            "a",
            ObservationKind::Prompt,
            json!({"prompt": "続き"}),
        );
        assert_eq!(
            store.finalize_session("a").unwrap().handoff_id.as_deref(),
            Some(rules.as_str())
        );
        let pending = store.pending_handoff(&pid, false, None).unwrap().unwrap();
        assert_eq!(
            pending.id, agent.id,
            "the newer agent handoff must stay on top"
        );
    }

    #[test]
    fn digest_paths_are_relative_to_the_sessions_own_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start_at(&store, "mac", "/Users/u/src/kioku");
        observe(
            &store,
            "mac",
            ObservationKind::Prompt,
            json!({"prompt": "x"}),
        );
        tool(&store, "mac", "/Users/u/src/kioku/src/lib.rs");
        // another machine starts a session later and overwrites the project's root
        start_at(&store, "linux", "/home/u/kioku");
        tool(&store, "linux", "/home/u/kioku/src/main.rs");
        assert_eq!(
            store.project(&project().id).unwrap().root_path.as_deref(),
            Some("/home/u/kioku")
        );
        let d = store.digest("mac").unwrap();
        assert_eq!(d.files[0].path, "src/lib.rs");
        let d = store.digest("linux").unwrap();
        assert_eq!(d.files[0].path, "src/main.rs");
        assert_eq!(
            store.session("mac").unwrap().root_path.as_deref(),
            Some("/Users/u/src/kioku")
        );
    }

    fn start_as(store: &Store, session: &str, agent: &str, source: &str) -> SessionStartResponse {
        store
            .start_session(&SessionStartRequest {
                machine: None,
                session_id: session.into(),
                agent: agent.into(),
                cwd: "/home/u/kioku".into(),
                source: source.into(),
                project: project(),
                lane: None,
            })
            .unwrap()
    }

    #[test]
    fn session_context_has_no_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let pid = project().id;
        assert!(matches!(
            store.session_context("unknown"),
            Err(Error::NotFound(_))
        ));

        // an earlier session leaves a rules handoff and a session page
        start(&store, "earlier");
        work(&store, "earlier");
        let rules = store
            .finalize_session("earlier")
            .unwrap()
            .handoff_id
            .unwrap();

        // a Cursor session starts (consuming it); nothing was accepted by "earlier" itself
        let started = start_as(&store, "cursor-1", "cursor", "startup");
        assert_eq!(started.pending_handoff.as_ref().unwrap().id, rules);
        let ctx = store.session_context("earlier").unwrap();
        assert!(ctx.pending_handoff.is_none());

        // a new handoff arrives meanwhile: context must neither return nor consume it
        let newer = store
            .write_handoff(&agent_handoff(&pid, Some("earlier"), "別の引き継ぎ"))
            .unwrap();
        let status_before = store.session("cursor-1").unwrap().status;
        for _ in 0..2 {
            let ctx = store.session_context("cursor-1").unwrap();
            assert_eq!(ctx.project_id, pid);
            let h = ctx.pending_handoff.unwrap();
            assert_eq!(h.id, rules);
            assert_eq!(h.accepted_by.as_deref(), Some("cursor-1"));
            assert_eq!(ctx.state_excerpt, started.state_excerpt);
            assert!(
                ctx.state_excerpt
                    .as_deref()
                    .unwrap()
                    .starts_with("## 最新の引き継ぎ")
            );
            assert_eq!(ctx.recent_sessions, started.recent_sessions);
            assert_eq!(ctx.recent_sessions.len(), 1);
        }
        assert_eq!(store.session("cursor-1").unwrap().status, status_before);
        let pending = store.pending_handoff(&pid, false, None).unwrap().unwrap();
        assert_eq!(pending.id, newer.id);
        assert!(pending.accepted_at.is_none(), "context must not consume");

        // once the session has its own page, it is not listed among the recent sessions
        observe(
            &store,
            "cursor-1",
            ObservationKind::Prompt,
            json!({"prompt": "続きをやって"}),
        );
        store.finalize_session("cursor-1").unwrap();
        let ctx = store.session_context("cursor-1").unwrap();
        assert_eq!(ctx.recent_sessions.len(), 1);
        assert!(
            ctx.recent_sessions[0]
                .path
                .ends_with(&format!("-earlier-{}.md", &sha256_hex("earlier")[..12]))
        );
        assert_eq!(ctx.pending_handoff.unwrap().id, rules);
    }

    #[test]
    fn implicit_and_fork_sources_and_agent_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start_as(&store, "codex-fork", "codex", "fork");
        assert_eq!(store.session("codex-fork").unwrap().source, "fork");
        let resp = start_as(&store, "gemini-implicit", "gemini-cli", "implicit");
        assert_eq!(resp.project_id, project().id);
        let s = store.session("gemini-implicit").unwrap();
        assert_eq!(s.source, "implicit");
        assert_eq!(s.agent, "gemini-cli");
        assert_eq!(s.status, SessionStatus::Open);

        // Codex apply_patch, normalized to Edit + file_paths; the patch exceeds the
        // tool_input cap, the path list survives server-side sanitization
        observe(
            &store,
            "codex-fork",
            ObservationKind::Prompt,
            json!({"prompt": "二つのファイルを直して"}),
        );
        observe(
            &store,
            "codex-fork",
            ObservationKind::ToolUse,
            json!({
                "tool_name": "Edit",
                "native_tool": "apply_patch",
                "tool_input": {
                    "file_paths": ["/home/u/kioku/src/a.rs", "/home/u/kioku/src/b.rs"],
                    "patch": format!("*** Begin Patch\n{}", "+x\n".repeat(3000)),
                },
                "tool_response": "Success. Updated the following files",
            }),
        );
        let stored = &store.observations("codex-fork").unwrap()[1].payload;
        assert!(stored["tool_input"].to_string().chars().count() <= 4000);
        let r = store.finalize_session("codex-fork").unwrap();
        let page = store.read_page(r.session_page.as_deref().unwrap()).unwrap();
        assert_eq!(page.frontmatter.agent.as_deref(), Some("codex"));
        assert_eq!(page.frontmatter.tags, vec!["codex"]);
        assert!(
            page.frontmatter
                .title
                .contains(" codex — 二つのファイルを直して")
        );
        assert!(page.body.contains("- src/a.rs (1)"), "{}", page.body);
        assert!(page.body.contains("- src/b.rs (1)"), "{}", page.body);
        let state = store.read_page("kioku-3f9a1c2e/STATE.md").unwrap();
        assert!(state.body.contains(" codex — 二つのファイルを直して"));
    }

    #[test]
    fn status_reports_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let s = store.status().unwrap();
        assert_eq!(s.version, crate::VERSION);
        assert_eq!(s.index_schema_version, Some(INDEX_SCHEMA_VERSION));
        assert_eq!(s.index_schema_expected, INDEX_SCHEMA_VERSION);
        std::fs::remove_file(DataDir::new(tmp.path()).index_version_file()).unwrap();
        assert_eq!(store.status().unwrap().index_schema_version, None);
        // a status body from an M1 server still deserializes
        let old: StatusReport = serde_json::from_value(json!({
            "data_dir": "/d", "projects": 0, "pages": 0, "sessions": 0, "observations": 0,
            "handoffs": 0, "index_docs": 0, "git_enabled": false
        }))
        .unwrap();
        assert_eq!(old.index_schema_version, None);
        assert_eq!(old.version, "");
    }

    #[test]
    fn old_database_is_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("db").join("kioku.sqlite");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&file).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, agent TEXT NOT NULL,
                   cwd TEXT, source TEXT, started_at TEXT NOT NULL, ended_at TEXT,
                   status TEXT NOT NULL DEFAULT 'open');
                 CREATE TABLE handoffs(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, session_id TEXT,
                   source TEXT NOT NULL, content_md TEXT NOT NULL, created_at TEXT NOT NULL,
                   accepted_at TEXT, accepted_by TEXT);",
            )
            .unwrap();
        }
        {
            // an old row: NULL lane = the project lane
            let conn = Connection::open(&file).unwrap();
            conn.execute_batch(
                "INSERT INTO handoffs(id, project_id, session_id, source, content_md, created_at)
                 VALUES ('old-h', 'kioku-3f9a1c2e', NULL, 'agent', '## 引き継ぎ\n古い', '2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        }
        let store = open_store(tmp.path());
        let branch = start_on(&store, "on-a-branch", Some("feature/x"));
        assert!(branch.pending_handoff.is_none());
        assert_eq!(branch.reference_handoff.unwrap().id, "old-h");
        let resp = start(&store, "after-migration");
        // SPEC-M3.0: the new sections (and `sessions.machine`) work on a migrated database
        assert_eq!(resp.context_version, crate::session::CONTEXT_VERSION);
        assert_eq!(resp.pending_handoff.unwrap().id, "old-h");
        work(&store, "after-migration");
        assert!(
            store
                .finalize_session("after-migration")
                .unwrap()
                .substantive
        );
        assert_eq!(
            store.session("on-a-branch").unwrap().lane.as_deref(),
            Some("feature/x")
        );
        assert!(store.list_aliases().unwrap().is_empty());
    }

    #[test]
    fn finalize_uses_agent_handoff_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "s-agent");
        work(&store, "s-agent");
        let h = store
            .write_handoff(&agent_handoff(
                &project().id,
                Some("s-agent"),
                "検索を実装した",
            ))
            .unwrap();
        let r = store.finalize_session("s-agent").unwrap();
        assert_eq!(r.handoff_id.as_deref(), Some(h.id.as_str()));
        let page = store.read_page(r.session_page.as_deref().unwrap()).unwrap();
        assert!(page.body.contains("## 引き継ぎ（claude-code, "));
        assert!(page.body.contains("### 要約\n検索を実装した"));
        assert!(!page.body.contains("自動生成"));
        assert_eq!(store.status().unwrap().handoffs, 1);
        let state = store.read_page("kioku-3f9a1c2e/STATE.md").unwrap();
        assert!(state.body.contains("source: agent"));
    }

    #[test]
    fn finalize_non_substantive_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "empty");
        observe(
            &store,
            "empty",
            ObservationKind::Compact,
            json!({"trigger": "auto"}),
        );
        let r = store.finalize_session("empty").unwrap();
        assert_eq!(r, FinalizeResult::default());
        assert_eq!(store.status().unwrap().pages, 0);
        assert_eq!(store.finalize_session("empty").unwrap(), r);
        assert_eq!(
            store.session_info("empty").unwrap().status,
            SessionStatus::Finalized
        );
        assert!(matches!(
            store.finalize_session("missing"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn observations_are_sanitized_and_logged_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        start(&store, "s1");
        observe(
            &store,
            "s1",
            ObservationKind::Prompt,
            json!({"prompt": "password=hunter2 で接続"}),
        );
        let obs = store.observations("s1").unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].seq, 1);
        assert_eq!(obs[0].payload["prompt"], "password=[REDACTED] で接続");
        assert!(!obs[0].text.contains("hunter2"));
        let raw = std::fs::read_to_string(DataDir::new(tmp.path()).raw_file(&project().id, "s1"))
            .unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(!raw.contains("hunter2"));
        let err = store.add_observation(&NewObservation {
            event_id: None,
            session_id: "unknown".into(),
            kind: ObservationKind::Prompt,
            ts: None,
            payload: json!({}),
        });
        assert!(matches!(err, Err(Error::NotFound(_))));
        let err = store.add_observation(&NewObservation {
            event_id: None,
            session_id: "../x".into(),
            kind: ObservationKind::Prompt,
            ts: None,
            payload: json!({}),
        });
        assert!(matches!(err, Err(Error::InvalidInput(_))));
    }

    #[test]
    fn write_read_search_and_reindex_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        store.register_project(&project()).unwrap();
        let pid = project().id;
        let global = store
            .write_page(&WritePageRequest {
                title: "自宅サーバーの構成".into(),
                content: "k3sクラスタにWireGuardで自宅サーバーを参加させた".into(),
                tags: vec!["infra".into()],
                ..WritePageRequest::default()
            })
            .unwrap();
        assert!(global.starts_with("_global/page-"));
        let proj = store
            .write_page(&WritePageRequest {
                title: "Design Notes".into(),
                content: "引き継ぎ書を毎回作るのが手間なので自動化したい".into(),
                project: Some(pid.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();
        assert_eq!(proj, "kioku-3f9a1c2e/pages/design-notes.md");

        // unknown keys and `created` survive a rewrite
        let file = tmp.path().join("wiki").join(&proj);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, text.replacen("---\n", "---\nreviewed_by: 人間\n", 1)).unwrap();
        let before = store.read_page(&proj).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store
            .write_page(&WritePageRequest {
                title: "Design Notes".into(),
                content: "更新した本文".into(),
                project: Some(pid.clone()),
                ..WritePageRequest::default()
            })
            .unwrap();
        let after = store.read_page(&proj).unwrap();
        assert_eq!(after.body, "更新した本文\n");
        assert_eq!(after.frontmatter.created, before.frontmatter.created);
        assert_ne!(after.frontmatter.updated, before.frontmatter.updated);
        assert_eq!(
            after.frontmatter.extra.get("reviewed_by"),
            Some(&serde_yaml_ng::Value::String("人間".into()))
        );

        let hits = store
            .search("自宅サーバー", &SearchScope::Project(pid.clone()), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].global);
        assert!(
            store
                .search("手間", &SearchScope::All, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search("更新", &SearchScope::All, 10).unwrap()[0].path,
            proj
        );

        assert!(matches!(
            store.read_page("_global/none.md"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            store.read_page("../etc/passwd"),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            store.write_page(&WritePageRequest {
                title: "x".into(),
                project: Some("unknown".into()),
                ..WritePageRequest::default()
            }),
            Err(Error::NotFound(_))
        ));

        // a hand-written file is picked up by reindex
        std::fs::write(
            tmp.path().join("wiki/_global/manual.md"),
            "# 手書きメモ\nFlutterでコードチャートのアプリを作っている\n",
        )
        .unwrap();
        assert!(
            store
                .search("アプリ", &SearchScope::All, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.reindex().unwrap(), 3);
        let hits = store.search("flutter", &SearchScope::Global, 10).unwrap();
        assert_eq!(hits[0].path, "_global/manual.md");
        assert_eq!(hits[0].title, "手書きメモ");
        let status = store.status().unwrap();
        assert_eq!(status.pages, 3);
        assert_eq!(status.index_docs, 3);
        assert_eq!(status.projects, 1);
        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, pid);

        // reopening keeps everything; a deleted index is rebuilt from the wiki
        drop(store);
        std::fs::remove_dir_all(tmp.path().join("index")).unwrap();
        let store = open_store(tmp.path());
        assert_eq!(store.status().unwrap().index_docs, 3);
        assert_eq!(
            store.search("WireGuard", &SearchScope::All, 10).unwrap()[0].path,
            global
        );
    }

    #[test]
    fn old_index_version_is_detected_until_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        assert!(!store.index_outdated(), "a fresh index is current");
        store
            .write_page(&WritePageRequest {
                title: "全角".into(),
                content: "Ｆｌｕｔｔｅｒ ｱﾌﾟﾘ".into(),
                ..WritePageRequest::default()
            })
            .unwrap();
        drop(store);
        // an index from before versioning (no version file) is reported, not rebuilt
        let version = DataDir::new(tmp.path()).index_version_file();
        std::fs::remove_file(&version).unwrap();
        let store = open_store(tmp.path());
        assert_eq!(store.index_version(), 1);
        assert!(store.index_outdated());
        assert_eq!(
            store.status().unwrap().index_docs,
            1,
            "not silently rebuilt"
        );
        store.reindex().unwrap();
        assert!(!store.index_outdated());
        assert_eq!(
            std::fs::read_to_string(&version).unwrap().trim(),
            INDEX_SCHEMA_VERSION.to_string()
        );
        assert_eq!(
            store.search("アプリ", &SearchScope::All, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn wiki_commits_when_git_available() {
        if !crate::git::git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        store
            .write_page(&WritePageRequest {
                title: "Tips".into(),
                content: "本文".into(),
                ..WritePageRequest::default()
            })
            .unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path().join("wiki"))
            .args(["log", "--format=%an %s"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout);
        assert_eq!(log.trim(), "kioku kioku: page _global/tips.md");
    }

    /// SPEC-M2.7 §9: pages and handoffs are redacted before they reach disk, SQLite, the
    /// index or git.
    #[test]
    fn pages_and_handoffs_are_redacted_before_storing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let token = format!("ghp_{}", "a1B2".repeat(9));
        let path = store
            .write_page(&WritePageRequest {
                title: "環境変数のメモ".into(),
                content: format!("CI では GITHUB_TOKEN={token} を使う\n秘密鍵: {token}\n"),
                ..WritePageRequest::default()
            })
            .unwrap();
        let file = std::fs::read_to_string(tmp.path().join("wiki").join(&path)).unwrap();
        assert!(!file.contains(&token), "{file}");
        assert!(file.contains("GITHUB_TOKEN=[REDACTED]"), "{file}");
        let page = store.read_page(&path).unwrap();
        assert!(!page.body.contains(&token));
        // The search index has the redacted text too (Japanese query).
        let hits = store.search("環境変数", &SearchScope::All, 3).unwrap();
        assert_eq!(hits[0].path, path);
        assert!(!hits[0].snippet.contains(&token));

        start(&store, "s-redact");
        let h = store
            .write_handoff(&HandoffInput {
                gotchas: Vec::new(),
                verified: Vec::new(),
                project: project().id,
                session: Some("s-redact".into()),
                summary: format!("token は {token}"),
                next_steps: vec![format!("export NPM_TOKEN=npm_{}", "x1Y2z3".repeat(6))],
                open_questions: vec!["DB_PASS=hunter2 を変える？".into()],
                decisions: vec![format!("Cookie: sid={token}")],
            })
            .unwrap();
        assert!(!h.content_md.contains(&token), "{}", h.content_md);
        assert!(!h.content_md.contains("hunter2"), "{}", h.content_md);
        assert!(!h.content_md.contains("x1Y2z3x1Y2z3"), "{}", h.content_md);
        assert!(h.content_md.contains(REDACTED_MARK));

        if crate::git::git_available() {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path().join("wiki"))
                .args(["log", "-p", "--all"])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stdout);
            assert!(log.contains("GITHUB_TOKEN=[REDACTED]"), "{log}");
            assert!(!log.contains(&token), "{log}");
        }
    }

    const REDACTED_MARK: &str = crate::sanitize::REDACTED;
}
