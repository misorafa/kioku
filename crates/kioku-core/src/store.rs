//! `Store`: the synchronous facade over wiki files, git, SQLite and the tantivy index.
//!
//! Every method blocks (filesystem, SQLite, tantivy, git); async callers must run them in
//! `tokio::task::spawn_blocking`. Page writes, finalize and reindex are serialized by one
//! write lock; lock order is always write lock → db → index writer.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::{self, PageRow, ProjectRow};
use crate::digest::{SessionDigest, aggregate_files};
use crate::error::{Error, Result};
use crate::git::Git;
use crate::handoff::{Handoff, HandoffInput, HandoffSource, render_agent_handoff};
use crate::index::{Hit, INDEX_SCHEMA_VERSION, IndexDoc, SearchIndex, SearchScope};
use crate::layout::DataDir;
use crate::page::{Frontmatter, Page, PageKind, PageScope, resolve_write_path, validate_rel_path};
use crate::project::{ProjectIdentity, is_valid_id};
use crate::render::{
    StateSession, session_body, session_page_path, session_title, state_body, state_title,
};
use crate::sanitize::{redact, sanitize_payload};
use crate::session::{
    FinalizeResult, HANDOFF_STALE_TOOL_USES, NewObservation, Observation, ObservationKind,
    RecentSession, Session, SessionInfo, SessionStartRequest, SessionStartResponse, SessionStatus,
    is_valid_session_id, observation_text,
};
use crate::util::{self, display_date, display_minute, now_ts, sha256_hex};

/// Lines of STATE.md returned by `start_session`.
pub const STATE_EXCERPT_LINES: usize = 60;
/// Session pages returned by `start_session`.
pub const RECENT_SESSIONS: usize = 5;
/// Sessions considered for STATE.md.
pub const STATE_SESSIONS: usize = 10;

/// Input of `Store::write_page` (`kioku_write_page`, `PUT /api/v1/pages`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritePageRequest {
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
}

/// The kioku store. `Send + Sync`; share it as `Arc<Store>`.
pub struct Store {
    config: Config,
    dirs: DataDir,
    db: Mutex<Connection>,
    index: SearchIndex,
    git: Git,
    write_lock: Mutex<()>,
}

impl Store {
    /// Opens the store for `config.data_dir`, creating the layout, schema and index as needed.
    pub fn open(config: Config) -> Result<Store> {
        let dirs = DataDir::new(&config.data_dir);
        dirs.ensure()?;
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
        };
        if fresh {
            if list_wiki_pages(&store.dirs.wiki())?.is_empty() {
                store.write_index_version()?;
            } else {
                let n = store.reindex()?;
                tracing::info!(pages = n, "index was empty; rebuilt from wiki");
            }
        } else if store.index_outdated() {
            tracing::warn!(
                built_with = store.index_version(),
                current = INDEX_SCHEMA_VERSION,
                "the search index was built by an older kioku; run `kioku reindex` so search \
                 matches this version (e.g. full-width / half-width text)"
            );
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

    /// Fetches a registered project.
    pub fn project(&self, id: &str) -> Result<ProjectRow> {
        db::get_project(&self.db.lock(), id)?
            .ok_or_else(|| Error::not_found(format!("project {id}")))
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
        let project_id = req.project.id.clone();
        let now = now_ts();
        let (pending_handoff, recent) = {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            db::upsert_project(&tx, &req.project, &now)?;
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
                },
            )?;
            let pending = db::newest_handoff(&tx, &project_id, true)?;
            if pending.is_some() {
                db::accept_pending_handoffs(&tx, &project_id, &req.session_id, &now)?;
            }
            let pending = match pending {
                Some(h) => db::get_handoff(&tx, &h.id)?,
                None => None,
            };
            let recent = recent_sessions(&tx, &project_id, None, RECENT_SESSIONS)?;
            tx.commit().context("committing session start")?;
            (pending, recent)
        };
        Ok(SessionStartResponse {
            project_id: project_id.clone(),
            pending_handoff,
            state_excerpt: self.state_excerpt(&project_id),
            recent_sessions: recent.into_iter().map(recent_entry).collect(),
        })
    }

    /// The SessionStart context of an existing session, without side effects (M2 §9.1):
    /// the handoff that session accepted (newest), the STATE.md excerpt and the project's
    /// recent session pages (the session's own page excluded).
    pub fn session_context(&self, id: &str) -> Result<SessionStartResponse> {
        let (session, pending_handoff, recent) = {
            let conn = self.db.lock();
            let session = db::get_session(&conn, id)?
                .ok_or_else(|| Error::not_found(format!("session {id}")))?;
            let pending = db::newest_handoff_accepted_by(&conn, id)?;
            let recent: Vec<(Session, String)> =
                recent_sessions(&conn, &session.project_id, None, RECENT_SESSIONS + 1)?
                    .into_iter()
                    .filter(|(s, _)| s.id != session.id)
                    .take(RECENT_SESSIONS)
                    .collect();
            (session, pending, recent)
        };
        Ok(SessionStartResponse {
            state_excerpt: self.state_excerpt(&session.project_id),
            project_id: session.project_id,
            pending_handoff,
            recent_sessions: recent.into_iter().map(recent_entry).collect(),
        })
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
        Ok(SessionInfo {
            project_id: session.project_id,
            status: session.status,
            counts,
            has_agent_handoff,
            tool_uses_since_handoff: Some(since),
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
        let payload = sanitize_payload(&obs.payload);
        let text = redact(&observation_text(obs.kind, &payload));
        let ts = obs
            .ts
            .as_deref()
            .and_then(util::parse_ts)
            .map(util::fmt_ts)
            .unwrap_or_else(now_ts);
        let payload_json = serde_json::to_string(&payload).context("serializing payload")?;
        let (seq, project_id) = {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            let session = db::get_session(&tx, &obs.session_id)?
                .ok_or_else(|| Error::not_found(format!("session {}", obs.session_id)))?;
            let seq = db::insert_observation(
                &tx,
                &session.id,
                &session.project_id,
                obs.kind,
                &ts,
                &payload_json,
                &text,
            )?;
            let substantive =
                matches!(obs.kind, ObservationKind::Prompt | ObservationKind::ToolUse);
            if substantive && session.status == SessionStatus::Finalized {
                db::set_session_status(&tx, &session.id, SessionStatus::Open, None)?;
            }
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
            let digest = digest_for(&conn, &session)?;
            let delta = match &digest.agent_handoff {
                Some(_)
                    if db::tool_uses_since_handoff(&conn, session_id)?
                        >= HANDOFF_STALE_TOOL_USES =>
                {
                    let mark = db::agent_handoff_mark(&conn, session_id)?;
                    let obs = db::list_observations_after(&conn, session_id, mark.as_ref())?;
                    let obs: Vec<Observation> =
                        obs.into_iter().filter(|o| o.seq <= max_seq).collect();
                    Some(SessionDigest::from_observations(
                        &obs,
                        session_root(&conn, &session)?.as_deref(),
                    ))
                }
                _ => None,
            };
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
        let fm = Frontmatter {
            title: session_title(lang, &session, &digest),
            project: Some(project.id.clone()),
            scope: PageScope::Project,
            kind: PageKind::Session,
            tags: vec![session.agent.clone()],
            session: Some(session.id.clone()),
            agent: Some(session.agent.clone()),
            ..Frontmatter::default()
        };
        self.put_page(
            &page_path,
            fm,
            &session_body(lang, &session, &digest, &handoff_md),
        )?;

        // 4. rules handoff: one pending rules handoff per session (or per agent handoff, for
        // the addendum), refreshed in place on re-finalize with its created_at kept.
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
                        };
                        db::insert_handoff(&conn, &h, Some(max_seq))?;
                        h.id
                    }
                }
            }
        };

        // 5. STATE.md
        self.write_state(&project, Some(session_id))?;

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
    pub fn write_handoff(&self, input: &HandoffInput) -> Result<Handoff> {
        if input.summary.trim().is_empty() {
            return Err(Error::invalid("summary must not be empty"));
        }
        let conn = self.db.lock();
        db::get_project(&conn, &input.project)?
            .ok_or_else(|| Error::not_found(format!("project {}", input.project)))?;
        let session = match &input.session {
            Some(id) => {
                let s = db::get_session(&conn, id)?
                    .ok_or_else(|| Error::not_found(format!("session {id}")))?;
                if s.project_id != input.project {
                    return Err(Error::invalid(format!(
                        "session {id} belongs to project {}",
                        s.project_id
                    )));
                }
                Some(s)
            }
            None => db::newest_open_session(&conn, &input.project)?,
        };
        let now = now_ts();
        let agent = session
            .as_ref()
            .map(|s| s.agent.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let content =
            render_agent_handoff(self.config.lang(), &agent, &display_minute(&now), input);
        let h = Handoff {
            id: util::new_id(),
            project_id: input.project.clone(),
            session_id: session.as_ref().map(|s| s.id.clone()),
            source: HandoffSource::Agent,
            content_md: content,
            created_at: now,
            accepted_at: None,
            accepted_by: None,
            agent: session.as_ref().map(|s| s.agent.clone()),
            updated_at: None,
        };
        let seq_at = match &session {
            Some(s) => Some(db::max_seq(&conn, &s.id)?),
            None => None,
        };
        db::insert_handoff(&conn, &h, seq_at)?;
        Ok(h)
    }

    /// Newest pending handoff of a project. With `accept`, it is consumed by `session`
    /// (or `"api"`) and older pending ones are superseded; without, it is only peeked.
    pub fn pending_handoff(
        &self,
        project: &str,
        accept: bool,
        session: Option<&str>,
    ) -> Result<Option<Handoff>> {
        let mut conn = self.db.lock();
        let tx = conn.transaction().context("starting transaction")?;
        let Some(h) = db::newest_handoff(&tx, project, true)? else {
            return Ok(None);
        };
        if !accept {
            return Ok(Some(h));
        }
        db::accept_pending_handoffs(&tx, project, session.unwrap_or("api"), &now_ts())?;
        let h = db::get_handoff(&tx, &h.id)?;
        tx.commit().context("committing handoff acceptance")?;
        Ok(h)
    }

    /// Newest handoff of a project, accepted or not.
    pub fn latest_handoff(&self, project: &str) -> Result<Option<Handoff>> {
        Ok(db::newest_handoff(&self.db.lock(), project, false)?)
    }

    // ---------------------------------------------------------------- pages

    /// Writes a page (spec §6.1 path rules); returns its wiki-relative path.
    pub fn write_page(&self, req: &WritePageRequest) -> Result<String> {
        let title = req.title.trim();
        if title.is_empty() {
            return Err(Error::invalid("title must not be empty"));
        }
        let scope = req.scope.unwrap_or(if req.project.is_some() {
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
                self.project(id)?;
                Some(id.to_string())
            }
        };
        let path = resolve_write_path(title, scope, project.as_deref(), req.path.as_deref())?;
        let fm = Frontmatter {
            title: title.to_string(),
            project,
            scope,
            kind: PageKind::Page,
            tags: req.tags.clone(),
            ..Frontmatter::default()
        };
        let _write = self.write_lock.lock();
        self.put_page(&path, fm, &req.content)?;
        Ok(path)
    }

    /// Reads a page by wiki-relative path.
    pub fn read_page(&self, path: &str) -> Result<Page> {
        let rel = validate_rel_path(path)?;
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
    pub fn search(&self, query: &str, scope: &SearchScope, limit: usize) -> Result<Vec<Hit>> {
        Ok(self.index.search(query, scope, limit)?)
    }

    /// Clears and rebuilds `pages` + the index from `wiki/`; returns the number of pages.
    pub fn reindex(&self) -> Result<usize> {
        let _write = self.write_lock.lock();
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
        Ok(docs.len())
    }

    /// Counts for `kioku status`.
    pub fn status(&self) -> Result<StatusReport> {
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
        })
    }

    // ---------------------------------------------------------------- internals

    /// Writes a page file (keeping `created` and unknown keys of an existing file), then
    /// updates SQLite, the index and git. Caller must hold the write lock.
    fn put_page(&self, path: &str, mut fm: Frontmatter, body: &str) -> Result<Page> {
        let file = self.dirs.wiki().join(path);
        let now = util::fmt_ts_secs(util::now());
        if let Ok(existing) = std::fs::read_to_string(&file)
            && let Ok(old) = Page::parse(path, &existing)
        {
            if !old.frontmatter.created.is_empty() {
                fm.created = old.frontmatter.created;
            }
            for (k, v) in old.frontmatter.extra {
                fm.extra.entry(k).or_insert(v);
            }
        }
        if fm.created.is_empty() {
            fm.created = now.clone();
        }
        fm.updated = now;
        let page = Page {
            path: path.to_string(),
            frontmatter: fm,
            body: body.to_string(),
        };
        let text = page.render()?;
        write_atomic(&file, &text)?;
        let (row, doc) = page_records(&page, &text);
        db::upsert_page(&self.db.lock(), &row)?;
        self.index.upsert(&doc)?;
        self.git.commit(
            &[path.to_string()],
            &format!("kioku: {} {path}", page.frontmatter.kind.as_str()),
        );
        Ok(page)
    }

    fn write_state(&self, project: &ProjectRow, include: Option<&str>) -> Result<()> {
        let lang = self.config.lang();
        let (latest, recent, digests) = {
            let conn = self.db.lock();
            let latest = db::newest_handoff(&conn, &project.id, false)?;
            let recent = recent_sessions(&conn, &project.id, include, STATE_SESSIONS)?;
            let mut digests = Vec::new();
            for (s, _) in &recent {
                digests.push(digest_for(&conn, s)?);
            }
            (latest, recent, digests)
        };
        let prefix = format!("{}/", project.id);
        let recent: Vec<StateSession> = recent
            .into_iter()
            .map(|(s, title)| {
                let path = session_page_path(&s);
                StateSession {
                    date: display_date(&s.started_at),
                    agent: s.agent.clone(),
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
        self.put_page(
            &format!("{}/STATE.md", project.id),
            fm,
            &state_body(lang, latest.as_ref(), &recent, &hot),
        )?;
        Ok(())
    }

    fn state_excerpt(&self, project: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dirs.wiki().join(project).join("STATE.md")).ok()?;
        let page = Page::parse(&format!("{project}/STATE.md"), &text).ok()?;
        let excerpt: Vec<&str> = page.body.lines().take(STATE_EXCERPT_LINES).collect();
        Some(excerpt.join("\n"))
    }
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
    }
}

/// The root a session's paths are relative to: its own (per machine), else the project's.
fn session_root(conn: &Connection, session: &Session) -> Result<Option<String>> {
    if let Some(root) = session.root_path.clone().filter(|r| !r.is_empty()) {
        return Ok(Some(root));
    }
    Ok(db::get_project(conn, &session.project_id)?.and_then(|p| p.root_path))
}

fn digest_for(conn: &Connection, session: &Session) -> Result<SessionDigest> {
    let root = session_root(conn, session)?;
    let observations = db::list_observations(conn, &session.id)?;
    let mut digest = SessionDigest::from_observations(&observations, root.as_deref());
    digest.agent_handoff =
        db::newest_session_handoff(conn, &session.id, Some(HandoffSource::Agent), false)?;
    Ok(digest)
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
    std::fs::rename(&tmp, file).with_context(|| format!("renaming into {}", file.display()))?;
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
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: "/home/u/kioku".into(),
                source: "startup".into(),
                project: project(),
            })
            .unwrap()
    }

    fn observe(store: &Store, session: &str, kind: ObservationKind, payload: serde_json::Value) {
        store
            .add_observation(&NewObservation {
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
            project: project.into(),
            session: session.map(str::to_string),
            summary: summary.into(),
            next_steps: vec!["テストを書く".into()],
            open_questions: vec![],
            decisions: vec!["lindera を採用".into()],
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
        assert!(page_path.ends_with("-0c2f1a2b.md"));
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
                session_id: session.into(),
                agent: "claude-code".into(),
                cwd: root.into(),
                source: "startup".into(),
                project: p,
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
                session_id: session.into(),
                agent: agent.into(),
                cwd: "/home/u/kioku".into(),
                source: source.into(),
                project: project(),
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
        assert!(ctx.recent_sessions[0].path.ends_with("-earlier.md"));
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
        let store = open_store(tmp.path());
        start(&store, "after-migration");
        work(&store, "after-migration");
        assert!(
            store
                .finalize_session("after-migration")
                .unwrap()
                .substantive
        );
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
            session_id: "unknown".into(),
            kind: ObservationKind::Prompt,
            ts: None,
            payload: json!({}),
        });
        assert!(matches!(err, Err(Error::NotFound(_))));
        let err = store.add_observation(&NewObservation {
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
}
