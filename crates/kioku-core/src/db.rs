//! SQLite metadata (spec §5): schema and every query the store runs.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::handoff::{Handoff, HandoffSource};
use crate::project::ProjectIdentity;
use crate::session::{Observation, ObservationKind, Session, SessionCounts, SessionStatus};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects(
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_path TEXT,
    remote_url TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions(
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    agent TEXT NOT NULL,
    cwd TEXT,
    source TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    status TEXT NOT NULL DEFAULT 'open'
);
CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project_id, started_at);
CREATE TABLE IF NOT EXISTS observations(
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    project_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    ts TEXT NOT NULL,
    payload TEXT NOT NULL,
    text TEXT NOT NULL,
    UNIQUE(session_id, seq)
);
CREATE TABLE IF NOT EXISTS handoffs(
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    session_id TEXT,
    source TEXT NOT NULL,
    content_md TEXT NOT NULL,
    created_at TEXT NOT NULL,
    accepted_at TEXT,
    accepted_by TEXT
);
CREATE INDEX IF NOT EXISTS handoffs_project ON handoffs(project_id, created_at);
CREATE INDEX IF NOT EXISTS handoffs_session ON handoffs(session_id);
CREATE TABLE IF NOT EXISTS pages(
    path TEXT PRIMARY KEY,
    project_id TEXT,
    scope TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    created_at TEXT,
    updated_at TEXT,
    hash TEXT
);
CREATE INDEX IF NOT EXISTS pages_project ON pages(project_id, kind, created_at);
"#;

/// A project row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    /// Project id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Absolute root path.
    pub root_path: Option<String>,
    /// Normalized remote.
    pub remote_url: Option<String>,
}

/// A page metadata row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRow {
    /// Wiki-relative path.
    pub path: String,
    /// Owning project.
    pub project_id: Option<String>,
    /// `project` | `global`.
    pub scope: String,
    /// `session` | `page` | `state`.
    pub kind: String,
    /// Title.
    pub title: String,
    /// Tags.
    pub tags: Vec<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 update time.
    pub updated_at: String,
    /// SHA-256 of the file contents.
    pub hash: String,
}

/// Opens the database with WAL + foreign keys and applies the schema.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA).context("applying schema")?;
    Ok(conn)
}

/// Inserts or updates a project (name/root/remote follow the latest identity).
pub fn upsert_project(conn: &Connection, p: &ProjectIdentity, now: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO projects(id, name, root_path, remote_url, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, root_path = excluded.root_path,
           remote_url = excluded.remote_url",
        params![p.id, p.name, p.root, p.remote, now],
    )?;
    Ok(())
}

/// Fetches a project.
pub fn get_project(conn: &Connection, id: &str) -> anyhow::Result<Option<ProjectRow>> {
    Ok(conn
        .query_row(
            "SELECT id, name, root_path, remote_url FROM projects WHERE id = ?1",
            params![id],
            |r| {
                Ok(ProjectRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    root_path: r.get(2)?,
                    remote_url: r.get(3)?,
                })
            },
        )
        .optional()?)
}

const SESSION_COLS: &str = "id, project_id, agent, cwd, source, started_at, ended_at, status";

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<Session> {
    let status: String = r.get(7)?;
    Ok(Session {
        id: r.get(0)?,
        project_id: r.get(1)?,
        agent: r.get(2)?,
        cwd: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        source: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        started_at: r.get(5)?,
        ended_at: r.get(6)?,
        status: if status == "finalized" {
            SessionStatus::Finalized
        } else {
            SessionStatus::Open
        },
    })
}

/// Creates a session, or reopens an existing one (resume) keeping its project and start time.
pub fn upsert_session(conn: &Connection, s: &Session) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO sessions(id, project_id, agent, cwd, source, started_at, ended_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'open')
         ON CONFLICT(id) DO UPDATE SET status = 'open', ended_at = NULL, source = excluded.source",
        params![s.id, s.project_id, s.agent, s.cwd, s.source, s.started_at],
    )?;
    Ok(())
}

/// Fetches a session.
pub fn get_session(conn: &Connection, id: &str) -> anyhow::Result<Option<Session>> {
    Ok(conn
        .query_row(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"),
            params![id],
            session_from_row,
        )
        .optional()?)
}

/// The most recently started open session of a project.
pub fn newest_open_session(conn: &Connection, project: &str) -> anyhow::Result<Option<Session>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE project_id = ?1 AND status = 'open'
                 ORDER BY started_at DESC, rowid DESC LIMIT 1"
            ),
            params![project],
            session_from_row,
        )
        .optional()?)
}

/// Sets status (and `ended_at`) of a session.
pub fn set_session_status(
    conn: &Connection,
    id: &str,
    status: SessionStatus,
    ended_at: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE sessions SET status = ?2, ended_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), ended_at],
    )?;
    Ok(())
}

/// Sessions of a project that have at least one prompt or tool use and are finalized
/// (or equal `include`), newest first.
pub fn recent_substantive_sessions(
    conn: &Connection,
    project: &str,
    include: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<Session>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SESSION_COLS} FROM sessions s
         WHERE s.project_id = ?1 AND (s.status = 'finalized' OR s.id = ?2)
           AND EXISTS (SELECT 1 FROM observations o WHERE o.session_id = s.id
                       AND o.kind IN ('prompt', 'tool_use'))
         ORDER BY s.started_at DESC, s.rowid DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![project, include, limit as i64], session_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Appends an observation with the next seq for its session; returns the seq.
pub fn insert_observation(
    conn: &Connection,
    session_id: &str,
    project_id: &str,
    kind: ObservationKind,
    ts: &str,
    payload: &str,
    text: &str,
) -> anyhow::Result<i64> {
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM observations WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO observations(session_id, project_id, seq, kind, ts, payload, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session_id,
            project_id,
            seq,
            kind.as_str(),
            ts,
            payload,
            text
        ],
    )?;
    Ok(seq)
}

/// All observations of a session in seq order.
pub fn list_observations(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<Observation>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, project_id, seq, kind, ts, payload, text FROM observations
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![session_id], |r| {
        let kind: String = r.get(4)?;
        let payload: String = r.get(6)?;
        Ok(Observation {
            id: r.get(0)?,
            session_id: r.get(1)?,
            project_id: r.get(2)?,
            seq: r.get(3)?,
            kind: ObservationKind::parse(&kind).unwrap_or(ObservationKind::Note),
            ts: r.get(5)?,
            payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
            text: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Prompt / tool-use counts of a session.
pub fn session_counts(conn: &Connection, session_id: &str) -> anyhow::Result<SessionCounts> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(kind = 'prompt'), 0), COALESCE(SUM(kind = 'tool_use'), 0)
         FROM observations WHERE session_id = ?1",
        params![session_id],
        |r| {
            Ok(SessionCounts {
                prompts: r.get::<_, i64>(0)? as u32,
                tool_uses: r.get::<_, i64>(1)? as u32,
            })
        },
    )?)
}

const HANDOFF_SELECT: &str = "SELECT h.id, h.project_id, h.session_id, h.source, h.content_md,
    h.created_at, h.accepted_at, h.accepted_by, s.agent
    FROM handoffs h LEFT JOIN sessions s ON s.id = h.session_id";

fn handoff_from_row(r: &Row<'_>) -> rusqlite::Result<Handoff> {
    let source: String = r.get(3)?;
    Ok(Handoff {
        id: r.get(0)?,
        project_id: r.get(1)?,
        session_id: r.get(2)?,
        source: HandoffSource::parse(&source).unwrap_or(HandoffSource::Rules),
        content_md: r.get(4)?,
        created_at: r.get(5)?,
        accepted_at: r.get(6)?,
        accepted_by: r.get(7)?,
        agent: r.get(8)?,
    })
}

/// Inserts a handoff.
pub fn insert_handoff(conn: &Connection, h: &Handoff) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO handoffs(id, project_id, session_id, source, content_md, created_at, accepted_at, accepted_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            h.id,
            h.project_id,
            h.session_id,
            h.source.as_str(),
            h.content_md,
            h.created_at,
            h.accepted_at,
            h.accepted_by
        ],
    )?;
    Ok(())
}

/// Replaces content (and bumps `created_at`) of an existing handoff.
pub fn update_handoff_content(
    conn: &Connection,
    id: &str,
    content: &str,
    now: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE handoffs SET content_md = ?2, created_at = ?3 WHERE id = ?1",
        params![id, content, now],
    )?;
    Ok(())
}

/// Fetches a handoff by id.
pub fn get_handoff(conn: &Connection, id: &str) -> anyhow::Result<Option<Handoff>> {
    Ok(conn
        .query_row(
            &format!("{HANDOFF_SELECT} WHERE h.id = ?1"),
            params![id],
            handoff_from_row,
        )
        .optional()?)
}

/// Newest handoff of a project (`unaccepted_only` restricts to pending ones).
pub fn newest_handoff(
    conn: &Connection,
    project: &str,
    unaccepted_only: bool,
) -> anyhow::Result<Option<Handoff>> {
    let cond = if unaccepted_only {
        "AND h.accepted_at IS NULL"
    } else {
        ""
    };
    Ok(conn
        .query_row(
            &format!(
                "{HANDOFF_SELECT} WHERE h.project_id = ?1 {cond}
                 ORDER BY h.created_at DESC, h.rowid DESC LIMIT 1"
            ),
            params![project],
            handoff_from_row,
        )
        .optional()?)
}

/// Newest handoff of a session, optionally restricted to one source / to pending ones.
pub fn newest_session_handoff(
    conn: &Connection,
    session: &str,
    source: Option<HandoffSource>,
    unaccepted_only: bool,
) -> anyhow::Result<Option<Handoff>> {
    let accepted = if unaccepted_only {
        "AND h.accepted_at IS NULL"
    } else {
        ""
    };
    Ok(conn
        .query_row(
            &format!(
                "{HANDOFF_SELECT} WHERE h.session_id = ?1 AND (?2 IS NULL OR h.source = ?2) {accepted}
                 ORDER BY h.created_at DESC, h.rowid DESC LIMIT 1"
            ),
            params![session, source.map(HandoffSource::as_str)],
            handoff_from_row,
        )
        .optional()?)
}

/// Marks every pending handoff of a project as accepted by `by` (consume + supersede).
pub fn accept_pending_handoffs(
    conn: &Connection,
    project: &str,
    by: &str,
    now: &str,
) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE handoffs SET accepted_at = ?3, accepted_by = ?2
         WHERE project_id = ?1 AND accepted_at IS NULL",
        params![project, by, now],
    )?)
}

fn page_from_row(r: &Row<'_>) -> rusqlite::Result<PageRow> {
    let tags: String = r.get(5)?;
    Ok(PageRow {
        path: r.get(0)?,
        project_id: r.get(1)?,
        scope: r.get(2)?,
        kind: r.get(3)?,
        title: r.get(4)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        created_at: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        updated_at: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        hash: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
    })
}

const PAGE_COLS: &str = "path, project_id, scope, kind, title, tags, created_at, updated_at, hash";

/// Inserts or replaces a page row.
pub fn upsert_page(conn: &Connection, p: &PageRow) -> anyhow::Result<()> {
    let tags = serde_json::to_string(&p.tags)?;
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO pages({PAGE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
        ),
        params![
            p.path,
            p.project_id,
            p.scope,
            p.kind,
            p.title,
            tags,
            p.created_at,
            p.updated_at,
            p.hash
        ],
    )?;
    Ok(())
}

/// Fetches a page row.
pub fn get_page(conn: &Connection, path: &str) -> anyhow::Result<Option<PageRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PAGE_COLS} FROM pages WHERE path = ?1"),
            params![path],
            page_from_row,
        )
        .optional()?)
}

/// Deletes every page row (before a reindex).
pub fn delete_all_pages(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM pages", [])?;
    Ok(())
}

/// Row counts of a table (`projects`, `pages`, `sessions`, `observations`, `handoffs`).
pub fn count(conn: &Connection, table: &str) -> anyhow::Result<u64> {
    let allowed = ["projects", "pages", "sessions", "observations", "handoffs"];
    anyhow::ensure!(allowed.contains(&table), "unknown table {table}");
    let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
    Ok(n as u64)
}
