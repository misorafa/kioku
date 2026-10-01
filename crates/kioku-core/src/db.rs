//! SQLite metadata (spec §5): schema and every query the store runs.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

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
    status TEXT NOT NULL DEFAULT 'open',
    root_path TEXT,
    lane TEXT
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
    accepted_by TEXT,
    updated_at TEXT,
    seq_at INTEGER,
    lane TEXT
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
CREATE TABLE IF NOT EXISTS project_aliases(
    alias TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
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
    migrate(&conn).context("migrating schema")?;
    Ok(conn)
}

/// Columns added after the first M1 schema; `CREATE TABLE IF NOT EXISTS` does not add them
/// to an existing database, so they are added here.
const ADDED_COLUMNS: [(&str, &str, &str); 5] = [
    ("sessions", "root_path", "TEXT"),
    ("handoffs", "updated_at", "TEXT"),
    ("handoffs", "seq_at", "INTEGER"),
    ("sessions", "lane", "TEXT"),
    ("handoffs", "lane", "TEXT"),
];

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    for (table, column, decl) in ADDED_COLUMNS {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !names.iter().any(|n| n == column) {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS observation_receipts (
        session_id TEXT NOT NULL, event_id TEXT NOT NULL, seq INTEGER NOT NULL,
        request_hash TEXT NOT NULL, PRIMARY KEY(session_id, event_id));
        CREATE TABLE IF NOT EXISTS page_redirects (old_path TEXT PRIMARY KEY, new_path TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS reliability_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    Ok(())
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

/// A project alias: `alias` is resolved to `project_id` wherever a project id comes in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAlias {
    /// The alias (an id a client may send).
    pub alias: String,
    /// The canonical project id it stands for.
    pub project_id: String,
}

/// The canonical id an alias points to, if `id` is an alias.
pub fn resolve_alias(conn: &Connection, id: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project_id FROM project_aliases WHERE alias = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Records (or re-points) `alias → project_id`.
pub fn upsert_alias(
    conn: &Connection,
    alias: &str,
    project_id: &str,
    now: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO project_aliases(alias, project_id, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(alias) DO UPDATE SET project_id = excluded.project_id",
        params![alias, project_id, now],
    )?;
    Ok(())
}

/// Re-points every alias of `from` to `into` (after a merge).
pub fn repoint_aliases(conn: &Connection, from: &str, into: &str) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE project_aliases SET project_id = ?2 WHERE project_id = ?1",
        params![from, into],
    )?)
}

/// All aliases, ordered by alias.
pub fn list_aliases(conn: &Connection) -> anyhow::Result<Vec<ProjectAlias>> {
    let mut stmt = conn.prepare("SELECT alias, project_id FROM project_aliases ORDER BY alias")?;
    let rows = stmt.query_map([], |r| {
        Ok(ProjectAlias {
            alias: r.get(0)?,
            project_id: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Rows of a project that a merge moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectRowCounts {
    /// Sessions.
    pub sessions: u64,
    /// Observations.
    pub observations: u64,
    /// Handoffs.
    pub handoffs: u64,
}

/// Session / observation / handoff counts of a project.
pub fn project_row_counts(conn: &Connection, project: &str) -> anyhow::Result<ProjectRowCounts> {
    let n = |table: &str| -> anyhow::Result<u64> {
        let v: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
            params![project],
            |r| r.get(0),
        )?;
        Ok(v as u64)
    };
    Ok(ProjectRowCounts {
        sessions: n("sessions")?,
        observations: n("observations")?,
        handoffs: n("handoffs")?,
    })
}

/// Moves sessions, observations, handoffs and page rows of `from` to `into` and deletes the
/// `from` project row (a merge; run inside a transaction).
pub fn move_project_rows(conn: &Connection, from: &str, into: &str) -> anyhow::Result<()> {
    for table in ["sessions", "observations", "handoffs", "pages"] {
        conn.execute(
            &format!("UPDATE {table} SET project_id = ?2 WHERE project_id = ?1"),
            params![from, into],
        )?;
    }
    conn.execute("DELETE FROM projects WHERE id = ?1", params![from])?;
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

/// All projects, ordered by id.
pub fn list_projects(conn: &Connection) -> anyhow::Result<Vec<ProjectRow>> {
    let mut stmt =
        conn.prepare("SELECT id, name, root_path, remote_url FROM projects ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            name: r.get(1)?,
            root_path: r.get(2)?,
            remote_url: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const SESSION_COLS: &str =
    "id, project_id, agent, cwd, source, started_at, ended_at, status, root_path, lane";

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
        root_path: r.get(8)?,
        lane: r.get(9)?,
    })
}

/// Creates a session, or reopens an existing one (resume) keeping its project and start time;
/// the lane follows the latest start (the branch may have changed).
pub fn upsert_session(conn: &Connection, s: &Session) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO sessions(id, project_id, agent, cwd, source, started_at, ended_at, status, root_path, lane)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'open', ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET status = 'open', ended_at = NULL, source = excluded.source,
           root_path = COALESCE(excluded.root_path, sessions.root_path), lane = excluded.lane",
        params![
            s.id,
            s.project_id,
            s.agent,
            s.cwd,
            s.source,
            s.started_at,
            s.root_path,
            s.lane
        ],
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

/// The open session of a project that received the newest observation (the one the agent
/// is working in); sessions without observations rank last, newest start first.
pub fn newest_open_session(conn: &Connection, project: &str) -> anyhow::Result<Option<Session>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {SESSION_COLS} FROM sessions s WHERE project_id = ?1 AND status = 'open'
                 ORDER BY (SELECT MAX(o.id) FROM observations o WHERE o.session_id = s.id)
                          DESC NULLS LAST,
                          started_at DESC, rowid DESC LIMIT 1"
            ),
            params![project],
            session_from_row,
        )
        .optional()?)
}

/// Marks a session finalized only if its newest observation seq is still `max_seq`
/// (an observation that arrived while finalize ran keeps it open); returns whether it did.
pub fn finalize_if_unchanged(
    conn: &Connection,
    id: &str,
    max_seq: i64,
    ended_at: &str,
) -> anyhow::Result<bool> {
    let n = conn.execute(
        "UPDATE sessions SET status = 'finalized', ended_at = ?2 WHERE id = ?1
           AND (SELECT COALESCE(MAX(seq), 0) FROM observations WHERE session_id = ?1) = ?3",
        params![id, ended_at, max_seq],
    )?;
    Ok(n > 0)
}

/// Highest observation seq of a session (0 when it has none).
pub fn max_seq(conn: &Connection, session_id: &str) -> anyhow::Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM observations WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?)
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
    list_observations_after(conn, session_id, None)
}

/// Where the session's latest agent handoff was written: the observation seq at that
/// moment (`seq_at`, NULL on rows from before it was recorded) and its `created_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffMark {
    /// Highest observation seq of the session when the handoff was written.
    pub seq_at: Option<i64>,
    /// RFC 3339 creation time (fallback when `seq_at` is unknown).
    pub created_at: String,
}

/// SQL condition "observation comes after the mark"; binds `?2` = seq_at, `?3` = created_at
/// (both NULL = no mark = every observation).
const AFTER_MARK: &str = "((?2 IS NULL AND ?3 IS NULL) OR (?2 IS NOT NULL AND seq > ?2)
    OR (?2 IS NULL AND ?3 IS NOT NULL AND ts > ?3))";

/// The mark of the newest agent handoff of a session, if any.
pub fn agent_handoff_mark(conn: &Connection, session: &str) -> anyhow::Result<Option<HandoffMark>> {
    Ok(conn
        .query_row(
            "SELECT seq_at, created_at FROM handoffs WHERE session_id = ?1 AND source = 'agent'
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![session],
            |r| {
                Ok(HandoffMark {
                    seq_at: r.get(0)?,
                    created_at: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// Tool uses of a session after its latest agent handoff (all of them when there is none).
pub fn tool_uses_since_handoff(conn: &Connection, session: &str) -> anyhow::Result<u32> {
    let mark = agent_handoff_mark(conn, session)?;
    let n: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1 AND kind = 'tool_use'
               AND {AFTER_MARK}"
        ),
        params![
            session,
            mark.as_ref().and_then(|m| m.seq_at),
            mark.as_ref().map(|m| m.created_at.clone())
        ],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// Observations of a session after `mark` (all when `None`), in seq order.
pub fn list_observations_after(
    conn: &Connection,
    session_id: &str,
    mark: Option<&HandoffMark>,
) -> anyhow::Result<Vec<Observation>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, session_id, project_id, seq, kind, ts, payload, text FROM observations
         WHERE session_id = ?1 AND {AFTER_MARK} ORDER BY seq"
    ))?;
    let seq_at = mark.and_then(|m| m.seq_at);
    let created = mark.map(|m| m.created_at.clone());
    let rows = stmt.query_map(params![session_id, seq_at, created], |r| {
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
    h.created_at, h.accepted_at, h.accepted_by, s.agent, h.updated_at, h.lane
    FROM handoffs h LEFT JOIN sessions s ON s.id = h.session_id";

/// Newest first; on equal `created_at` an agent-written handoff wins over a rules one.
const HANDOFF_ORDER: &str =
    "ORDER BY h.created_at DESC, (h.source = 'agent') DESC, h.rowid DESC LIMIT 1";

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
        updated_at: r.get(9)?,
        lane: r.get(10)?,
    })
}

/// Inserts a handoff; `seq_at` = the session's highest observation seq at that moment.
pub fn insert_handoff(conn: &Connection, h: &Handoff, seq_at: Option<i64>) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO handoffs(id, project_id, session_id, source, content_md, created_at, accepted_at, accepted_by, updated_at, seq_at, lane)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            h.id,
            h.project_id,
            h.session_id,
            h.source.as_str(),
            h.content_md,
            h.created_at,
            h.accepted_at,
            h.accepted_by,
            h.updated_at,
            seq_at,
            h.lane
        ],
    )?;
    Ok(())
}

/// Replaces content of an existing handoff and sets `updated_at`; `created_at` is kept so a
/// refresh never outranks a newer handoff.
pub fn update_handoff_content(
    conn: &Connection,
    id: &str,
    content: &str,
    now: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE handoffs SET content_md = ?2, updated_at = ?3 WHERE id = ?1",
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

/// Newest handoff of a project on one lane (`None` = the project lane);
/// `unaccepted_only` restricts to pending ones.
pub fn newest_handoff(
    conn: &Connection,
    project: &str,
    lane: Option<&str>,
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
                "{HANDOFF_SELECT} WHERE h.project_id = ?1 AND h.lane IS ?2 {cond} {HANDOFF_ORDER}"
            ),
            params![project, lane],
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
                 {HANDOFF_ORDER}"
            ),
            params![session, source.map(HandoffSource::as_str)],
            handoff_from_row,
        )
        .optional()?)
}

/// Newest handoff accepted by `session` (the one its SessionStart consumed; older ones it
/// superseded carry the same `accepted_by` but rank below).
pub fn newest_handoff_accepted_by(
    conn: &Connection,
    session: &str,
    project: &str,
    lane: Option<&str>,
) -> anyhow::Result<Option<Handoff>> {
    Ok(conn
        .query_row(
            &format!("{HANDOFF_SELECT} WHERE h.accepted_by = ?1 AND h.project_id = ?2 AND h.lane IS ?3 {HANDOFF_ORDER}"),
            params![session, project, lane],
            handoff_from_row,
        )
        .optional()?)
}

/// Marks every pending handoff of a project on one lane (`None` = the project lane) as
/// accepted by `by` (consume + supersede); other lanes are untouched.
pub fn accept_pending_handoffs(
    conn: &Connection,
    project: &str,
    lane: Option<&str>,
    by: &str,
    now: &str,
) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE handoffs SET accepted_at = ?3, accepted_by = ?2
         WHERE project_id = ?1 AND lane IS ?4 AND accepted_at IS NULL",
        params![project, by, now, lane],
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
    let allowed = [
        "projects",
        "pages",
        "sessions",
        "observations",
        "handoffs",
        "project_aliases",
        "observation_receipts",
        "page_redirects",
    ];
    anyhow::ensure!(allowed.contains(&table), "unknown table {table}");
    let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
    Ok(n as u64)
}
