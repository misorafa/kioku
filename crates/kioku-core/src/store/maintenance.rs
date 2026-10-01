//! Operational hygiene (SPEC-M2.8 §3, §5): the startup sweep, the deferred reindex of an
//! outdated index, retention (`prune`), `forget` and the storage sizes of `status`.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// `reliability_meta` key of the last real prune run.
const LAST_PRUNE: &str = "last_prune";

/// What the startup sweep fixed (SPEC-M2.8 §5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepReport {
    /// `.*.tmp` files of interrupted atomic writes removed from the wiki.
    pub tmp_removed: u64,
    /// Pages whose file differed from (or had no) row: row and index entry rewritten.
    pub pages_healed: Vec<String>,
    /// Rows whose file is gone: row and index entry removed.
    pub rows_removed: Vec<String>,
}

/// Count and bytes of one retention category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneCount {
    /// Files, sessions or snapshots affected.
    pub count: u64,
    /// Bytes freed (for a dry run: the bytes concerned).
    pub bytes: u64,
}

/// Result of `kioku prune` / `POST /api/v1/prune` (SPEC-M2.8 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    /// True when nothing was changed.
    pub dry_run: bool,
    /// Raw logs gzipped (bytes: saved; dry run: their size).
    pub raw_gzipped: PruneCount,
    /// Raw logs deleted (older than twice `raw_days`).
    pub raw_deleted: PruneCount,
    /// Sessions whose observation payloads were reduced to stubs (bytes: payload and text
    /// bytes removed from their rows; the database file shrinks only after a `VACUUM`).
    pub sessions_reduced: PruneCount,
    /// Observation rows reduced.
    pub observations_reduced: u64,
    /// Backups removed beyond `backups_keep`.
    pub backups_removed: PruneCount,
    /// Hook dump files removed.
    pub hook_dumps_removed: PruneCount,
    /// When the run happened (RFC 3339).
    pub at: String,
}

/// Disk usage of the data directory (`kioku status`, `GET /status`; SPEC-M2.8 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageReport {
    /// `db/` (SQLite file, WAL and shared memory).
    pub db_bytes: u64,
    /// `raw/` (raw observation logs, gzipped ones included).
    pub raw_bytes: u64,
    /// `wiki/` (pages and their git history).
    pub wiki_bytes: u64,
    /// `backups/`.
    pub backups_bytes: u64,
    /// `index/`.
    pub index_bytes: u64,
    /// Date (YYYY-MM-DD, UTC) of the oldest raw log's last change.
    pub oldest_raw: Option<String>,
    /// Time of the last real prune run.
    pub last_prune: Option<String>,
}

/// What `forget` removed (or, with `dry_run`, would remove).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReport {
    /// True when nothing was changed.
    pub dry_run: bool,
    /// The project concerned.
    pub project: String,
    /// Sessions removed.
    pub sessions: Vec<String>,
    /// Observations removed.
    pub observations: u64,
    /// Delivery receipts removed.
    pub receipts: u64,
    /// Handoffs removed.
    pub handoffs: u64,
    /// Wiki pages removed (wiki-relative).
    pub pages: Vec<String>,
    /// Raw log files removed (relative to `raw/`).
    pub raw_files: Vec<String>,
    /// The wiki directory on the server (for the history purge commands).
    pub wiki_dir: String,
}

/// Bytes of every file under `dir` (symlinks not followed; 0 when missing).
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Every file below `dir` (recursively), skipping a `.git` directory.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(entry.path());
                }
            } else if ty.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// True when `path` was last changed more than `days` days before `now`.
fn older_than(path: &Path, days: u32, now: SystemTime) -> bool {
    let age = Duration::from_secs(u64::from(days) * 24 * 3600);
    modified(path)
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|a| a > age)
}

/// Gzips `src` to `<src>.gz` (keeping its modification time) and removes `src`; returns the
/// compressed size.
fn gzip_file(src: &Path) -> anyhow::Result<u64> {
    let mut dest = src.as_os_str().to_os_string();
    dest.push(".gz");
    let dest = PathBuf::from(dest);
    let mtime = modified(src);
    let tmp = dest.with_extension("gz.tmp");
    {
        let input =
            std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
        let out =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        std::io::copy(&mut std::io::BufReader::new(input), &mut enc)?;
        let out = enc.finish()?;
        if let Some(t) = mtime {
            out.set_modified(t)?;
        }
        out.sync_all()?;
    }
    std::fs::rename(&tmp, &dest)?;
    std::fs::remove_file(src)?;
    Ok(std::fs::metadata(&dest)?.len())
}

/// The shell commands that remove `paths` from the wiki's git history (SPEC-M2.8 §3:
/// `kioku forget --purge-history` prints them; kioku never rewrites history itself).
pub fn purge_history_commands(wiki_dir: &str, paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let quoted: Vec<String> = paths.iter().map(|p| format!("'{p}'")).collect();
    let filter_repo = paths
        .iter()
        .map(|p| format!("--path '{p}'"))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        "kioku service stop".to_string(),
        format!("cd '{wiki_dir}'"),
        format!("git filter-repo --force --invert-paths {filter_repo}"),
        format!(
            "#   without git-filter-repo: git filter-branch --force --index-filter \"git rm --cached --ignore-unmatch -r -- {}\" -- --all",
            quoted.join(" ")
        ),
        "git reflog expire --expire=now --all && git gc --prune=now --aggressive".to_string(),
        "kioku service start".to_string(),
        "# backups made before this still contain the history (see `kioku prune`)".to_string(),
    ]
}

impl Store {
    /// The startup sweep (SPEC-M2.8 §5), under the write lock: removes `wiki/**/.*.tmp`
    /// left by an interrupted [`write_atomic`], then compares every page file with the hash
    /// in its `pages` row and rewrites the row and index entry of each that differs (a row
    /// whose file is gone is removed). Counts are logged.
    pub fn startup_sweep(&self) -> Result<SweepReport> {
        let _write = self.write_lock.lock();
        let wiki = self.dirs.wiki();
        let mut report = SweepReport::default();
        for file in walk_files(&wiki) {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.')
                && name.ends_with(".tmp")
                && std::fs::remove_file(&file).is_ok()
            {
                report.tmp_removed += 1;
            }
        }
        let rows: BTreeMap<String, Option<String>> = {
            let conn = self.db.lock();
            let mut stmt = conn
                .prepare("SELECT path, hash FROM pages")
                .context("listing page metadata")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .context("reading page metadata")?
                .collect::<rusqlite::Result<_>>()
                .context("reading page metadata")?
        };
        let mut docs = Vec::new();
        let mut on_disk = BTreeSet::new();
        for rel in list_wiki_pages(&wiki)? {
            on_disk.insert(rel.clone());
            let Ok(text) = std::fs::read_to_string(wiki.join(&rel)) else {
                continue;
            };
            if rows.get(&rel).and_then(|h| h.as_deref()) == Some(sha256_hex(&text).as_str()) {
                continue;
            }
            let Ok(page) = Page::parse(&rel, &text) else {
                continue; // reported by diagnostics; reindex skips it too
            };
            let (row, doc) = page_records(&page, &text);
            db::upsert_page(&self.db.lock(), &row)?;
            docs.push(doc);
            report.pages_healed.push(rel);
        }
        report.rows_removed = rows
            .keys()
            .filter(|p| !on_disk.contains(*p))
            .cloned()
            .collect();
        if !report.rows_removed.is_empty() {
            let conn = self.db.lock();
            for p in &report.rows_removed {
                conn.execute("DELETE FROM pages WHERE path=?1", [p])
                    .context("removing a stale page row")?;
            }
        }
        self.index.upsert_many(&docs, false)?;
        self.index.delete_paths(&report.rows_removed)?;
        if report.tmp_removed > 0 || !docs.is_empty() || !report.rows_removed.is_empty() {
            tracing::info!(
                tmp_removed = report.tmp_removed,
                pages_healed = report.pages_healed.len(),
                rows_removed = report.rows_removed.len(),
                "startup sweep repaired the data directory"
            );
        }
        Ok(report)
    }

    /// Rebuilds the index when it was built by an older kioku (SPEC-M2.8 §5: the server
    /// calls this on the blocking pool after it starts listening; search serves the old
    /// index until the rebuild commits). `None` when the index is current.
    pub fn reindex_if_outdated(&self) -> Result<Option<usize>> {
        if !self.index_outdated() {
            return Ok(None);
        }
        tracing::info!(
            built_with = self.index_version(),
            current = INDEX_SCHEMA_VERSION,
            "rebuilding the outdated search index"
        );
        let n = self.reindex()?;
        tracing::info!(pages = n, "search index rebuilt");
        Ok(Some(n))
    }

    /// Applies `[retention]` (SPEC-M2.8 §3): gzips raw logs older than `raw_days` (deletes
    /// them at twice that), reduces the observations of finalized sessions older than
    /// `observations_days` to stubs (their digest is cached first), keeps `backups_keep`
    /// backups and deletes hook dumps older than `hook_dump_days`. `dry_run` changes
    /// nothing and reports what would be done.
    pub fn prune(&self, dry_run: bool) -> Result<PruneReport> {
        let r = self.config.retention.clone();
        let now = SystemTime::now();
        let mut report = PruneReport {
            dry_run,
            at: now_ts(),
            ..PruneReport::default()
        };

        // Raw logs. Lines are appended under the DB lock, so each file is gzipped under it.
        if r.raw_days > 0 {
            for file in walk_files(&self.dirs.raw()) {
                let name = file.to_string_lossy().to_string();
                let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                let (jsonl, gz) = (name.ends_with(".jsonl"), name.ends_with(".jsonl.gz"));
                if (jsonl || gz) && older_than(&file, r.raw_days.saturating_mul(2), now) {
                    if !dry_run {
                        let _db = self.db.lock();
                        std::fs::remove_file(&file)
                            .with_context(|| format!("removing {}", file.display()))?;
                    }
                    report.raw_deleted.count += 1;
                    report.raw_deleted.bytes += size;
                } else if jsonl && older_than(&file, r.raw_days, now) {
                    report.raw_gzipped.count += 1;
                    if dry_run {
                        report.raw_gzipped.bytes += size;
                    } else {
                        let _db = self.db.lock();
                        let packed = gzip_file(&file)
                            .with_context(|| format!("gzipping {}", file.display()))?;
                        report.raw_gzipped.bytes += size.saturating_sub(packed);
                    }
                }
            }
        }

        // Observation payloads of old finalized sessions.
        if r.observations_days > 0 {
            let cutoff =
                util::fmt_ts(util::now() - chrono::Duration::days(i64::from(r.observations_days)));
            let sessions: Vec<Session> = {
                let conn = self.db.lock();
                let ids: Vec<String> = {
                    let mut stmt = conn
                        .prepare(
                            "SELECT s.id FROM sessions s WHERE s.status = 'finalized'
                           AND (SELECT MAX(o.ts) FROM observations o WHERE o.session_id = s.id) < ?1
                           AND EXISTS (SELECT 1 FROM observations o WHERE o.session_id = s.id
                                       AND json_extract(o.payload, '$.stub') IS NULL)
                         ORDER BY s.started_at",
                        )
                        .context("querying the database")?;
                    stmt.query_map([&cutoff], |row| row.get(0))
                        .context("querying the database")?
                        .collect::<rusqlite::Result<_>>()
                        .context("querying the database")?
                };
                let mut out = Vec::new();
                for id in ids {
                    if let Some(s) = db::get_session(&conn, &id)? {
                        out.push(s);
                    }
                }
                out
            };
            for session in sessions {
                // The write lock per session: finalize waits for one session at most. A
                // session reopened since it was listed is left alone.
                let _write = self.write_lock.lock();
                let still_old: bool = self
                    .db
                    .lock()
                    .query_row(
                        "SELECT s.status = 'finalized' AND (SELECT MAX(o.ts) FROM observations o
                           WHERE o.session_id = s.id) < ?2 FROM sessions s WHERE s.id = ?1",
                        params![session.id, cutoff],
                        |r| r.get::<_, Option<bool>>(0),
                    )
                    .optional()
                    .context("querying the database")?
                    .flatten()
                    .unwrap_or(false);
                if !still_old {
                    continue;
                }
                let (rows, bytes) = self.reduce_session(&session, dry_run)?;
                report.sessions_reduced.count += 1;
                report.sessions_reduced.bytes += bytes;
                report.observations_reduced += rows;
            }
        }

        let (count, bytes) = reliability::prune_backups(
            &self.dirs.root().join("backups"),
            self.config.backups_keep(),
            dry_run,
        );
        report.backups_removed = PruneCount { count, bytes };

        if r.hook_dump_days > 0 {
            for file in walk_files(&self.dirs.logs_dir()) {
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.starts_with("hook-dump.jsonl") && older_than(&file, r.hook_dump_days, now) {
                    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                    if !dry_run {
                        std::fs::remove_file(&file)
                            .with_context(|| format!("removing {}", file.display()))?;
                    }
                    report.hook_dumps_removed.count += 1;
                    report.hook_dumps_removed.bytes += size;
                }
            }
        }

        if !dry_run {
            self.db
                .lock()
                .execute(
                    "INSERT OR REPLACE INTO reliability_meta VALUES (?1, ?2)",
                    [LAST_PRUNE, &report.at],
                )
                .context("recording the prune time")?;
            tracing::info!(
                raw_gzipped = report.raw_gzipped.count,
                raw_deleted = report.raw_deleted.count,
                sessions_reduced = report.sessions_reduced.count,
                backups_removed = report.backups_removed.count,
                hook_dumps_removed = report.hook_dumps_removed.count,
                "retention applied"
            );
        }
        Ok(report)
    }

    /// Reduces every observation of `session` to its stub ([`crate::digest::stub_payload`])
    /// after making sure its digest is cached; returns `(rows, bytes)` reduced. The caller
    /// holds the write lock.
    fn reduce_session(&self, session: &Session, dry_run: bool) -> Result<(u64, u64)> {
        let mut conn = self.db.lock();
        if !dry_run {
            // A session without a cached digest is digested before reduction (§3).
            let upto = db::max_seq(&conn, &session.id)?;
            session_digests(&conn, session, upto, false)?;
        }
        let tx = conn.transaction().context("starting transaction")?;
        let rows: Vec<(i64, String, String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, kind, payload, text FROM observations WHERE session_id = ?1
                   AND json_extract(payload, '$.stub') IS NULL",
                )
                .context("querying the database")?;
            stmt.query_map([&session.id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .context("querying the database")?
            .collect::<rusqlite::Result<_>>()
            .context("querying the database")?
        };
        let (mut count, mut bytes) = (0u64, 0u64);
        for (id, kind, payload, text) in rows {
            let kind = ObservationKind::parse(&kind).unwrap_or(ObservationKind::Note);
            let value: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
            let stub = crate::digest::stub_payload(kind, &value, &text).to_string();
            count += 1;
            bytes += (payload.len() + text.len()).saturating_sub(stub.len()) as u64;
            if !dry_run {
                tx.execute(
                    "UPDATE observations SET payload = ?2, text = '' WHERE id = ?1",
                    params![id, stub],
                )
                .context("reducing an observation")?;
            }
        }
        tx.commit().context("committing the reduction")?;
        Ok((count, bytes))
    }

    /// `kioku forget --session <id>` (SPEC-M2.8 §3): removes the session's observations,
    /// receipts, raw log, session page (git commit `kioku: forget session <id>`), the
    /// handoffs it authored and its row; reindexes and rewrites STATE.md.
    pub fn forget_session(&self, id: &str, dry_run: bool) -> Result<ForgetReport> {
        let _write = self.write_lock.lock();
        let (session, mut report) = {
            let conn = self.db.lock();
            let session = db::get_session(&conn, id)?
                .ok_or_else(|| Error::not_found(format!("session {id}")))?;
            let report = self.forget_counts(&conn, std::slice::from_ref(&session), dry_run)?;
            (session, report)
        };
        report.project = session.project_id.clone();
        let page = session_page_path(&session);
        if self.dirs.wiki().join(&page).is_file() {
            report.pages.push(page.clone());
        }
        report.raw_files = raw_files_of(&self.dirs.raw(), &session.project_id, &[id]);
        if dry_run {
            return Ok(report);
        }
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            delete_session_rows(&tx, id)?;
            tx.execute("DELETE FROM pages WHERE path = ?1", [&page])
                .context("querying the database")?;
            tx.execute(
                "DELETE FROM page_redirects WHERE new_path = ?1 OR old_path = ?1",
                [&page],
            )
            .context("querying the database")?;
            tx.commit().context("committing forget")?;
            for rel in &report.raw_files {
                let _ = std::fs::remove_file(self.dirs.raw().join(rel));
            }
        }
        if !report.pages.is_empty() {
            std::fs::remove_file(self.dirs.wiki().join(&page))
                .with_context(|| format!("removing {page}"))?;
            self.git.commit(
                std::slice::from_ref(&page),
                &format!("kioku: forget session {id}"),
            );
        }
        self.reindex_locked()?;
        let project = db::get_project(&self.db.lock(), &session.project_id)?;
        if let Some(project) = project {
            self.write_state(&project)?;
        }
        Ok(report)
    }

    /// `kioku forget --project <id>` (SPEC-M2.8 §3): removes every session (as
    /// [`Store::forget_session`]), handoff, page (`wiki/<project>/`, git commit `kioku:
    /// forget project <id>`), raw log, alias and the project row; reindexes.
    pub fn forget_project(&self, id: &str, dry_run: bool) -> Result<ForgetReport> {
        let _write = self.write_lock.lock();
        let (project, mut report) = {
            let conn = self.db.lock();
            let project_id = resolve_id(&conn, id.trim())?;
            let project = db::get_project(&conn, &project_id)?
                .ok_or_else(|| Error::not_found(format!("project {id}")))?;
            let sessions: Vec<Session> = {
                let mut stmt = conn
                    .prepare("SELECT id FROM sessions WHERE project_id = ?1")
                    .context("querying the database")?;
                let ids: Vec<String> = stmt
                    .query_map([&project.id], |r| r.get(0))
                    .context("querying the database")?
                    .collect::<rusqlite::Result<_>>()
                    .context("querying the database")?;
                let mut out = Vec::new();
                for sid in ids {
                    if let Some(s) = db::get_session(&conn, &sid)? {
                        out.push(s);
                    }
                }
                out
            };
            let mut report = self.forget_counts(&conn, &sessions, dry_run)?;
            report.handoffs = conn
                .query_row(
                    "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
                    [&project.id],
                    |r| r.get::<_, i64>(0),
                )
                .context("querying the database")? as u64;
            (project, report)
        };
        report.project = project.id.clone();
        let prefix = format!("{}/", project.id);
        report.pages = list_wiki_pages(&self.dirs.wiki())?
            .into_iter()
            .filter(|p| p.starts_with(&prefix))
            .collect();
        let raw_dir = self.dirs.raw().join(&project.id);
        report.raw_files = walk_files(&raw_dir)
            .iter()
            .filter_map(|f| f.strip_prefix(self.dirs.raw()).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        if dry_run {
            return Ok(report);
        }
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction().context("starting transaction")?;
            for sid in &report.sessions {
                delete_session_rows(&tx, sid)?;
            }
            tx.execute("DELETE FROM handoffs WHERE project_id = ?1", [&project.id])
                .context("querying the database")?;
            tx.execute(
                "DELETE FROM pages WHERE project_id = ?1 OR substr(path, 1, length(?2)) = ?2",
                params![project.id, prefix],
            )
            .context("querying the database")?;
            tx.execute(
                "DELETE FROM page_redirects WHERE substr(new_path, 1, length(?1)) = ?1
                   OR substr(old_path, 1, length(?1)) = ?1",
                [&prefix],
            )
            .context("querying the database")?;
            tx.execute(
                "DELETE FROM project_aliases WHERE project_id = ?1 OR alias = ?1",
                [&project.id],
            )
            .context("querying the database")?;
            tx.execute("DELETE FROM projects WHERE id = ?1", [&project.id])
                .context("querying the database")?;
            tx.commit().context("committing forget")?;
            if raw_dir.exists() {
                std::fs::remove_dir_all(&raw_dir)
                    .with_context(|| format!("removing {}", raw_dir.display()))?;
            }
        }
        let dir = self.dirs.wiki().join(&project.id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            self.git.commit(
                std::slice::from_ref(&project.id),
                &format!("kioku: forget project {}", project.id),
            );
        }
        self.reindex_locked()?;
        Ok(report)
    }

    /// Counts of rows `forget` removes for `sessions` (handoffs: those they authored).
    fn forget_counts(
        &self,
        conn: &Connection,
        sessions: &[Session],
        dry_run: bool,
    ) -> Result<ForgetReport> {
        let mut report = ForgetReport {
            dry_run,
            wiki_dir: self.dirs.wiki().display().to_string(),
            ..ForgetReport::default()
        };
        for s in sessions {
            let n = |sql: &str| -> anyhow::Result<u64> {
                Ok(conn.query_row(sql, [&s.id], |r| r.get::<_, i64>(0))? as u64)
            };
            report.observations += n("SELECT COUNT(*) FROM observations WHERE session_id = ?1")?;
            report.receipts +=
                n("SELECT COUNT(*) FROM observation_receipts WHERE session_id = ?1")?;
            report.handoffs += n("SELECT COUNT(*) FROM handoffs WHERE session_id = ?1")?;
            report.sessions.push(s.id.clone());
        }
        Ok(report)
    }

    /// Disk usage of the data directory and retention bookkeeping (SPEC-M2.8 §3).
    pub fn storage(&self) -> Result<StorageReport> {
        let root = self.dirs.root();
        let oldest = walk_files(&self.dirs.raw())
            .iter()
            .filter_map(|f| modified(f))
            .min()
            .map(|t| display_date(&util::fmt_ts(chrono::DateTime::<chrono::Utc>::from(t))));
        Ok(StorageReport {
            db_bytes: dir_bytes(&root.join("db")),
            raw_bytes: dir_bytes(&self.dirs.raw()),
            wiki_bytes: dir_bytes(&self.dirs.wiki()),
            backups_bytes: dir_bytes(&root.join("backups")),
            index_bytes: dir_bytes(&root.join("index")),
            oldest_raw: oldest,
            last_prune: self.meta_value(LAST_PRUNE)?,
        })
    }

    /// A `reliability_meta` value.
    fn meta_value(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .query_row(
                "SELECT value FROM reliability_meta WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .context("reading reliability metadata")?)
    }
}

/// Deletes the rows of one session: observations, receipts, the handoffs it authored and
/// the session itself.
fn delete_session_rows(conn: &Connection, id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM observations WHERE session_id = ?1",
        "DELETE FROM observation_receipts WHERE session_id = ?1",
        "DELETE FROM handoffs WHERE session_id = ?1",
        "DELETE FROM sessions WHERE id = ?1",
    ] {
        conn.execute(sql, [id]).context("forgetting a session")?;
    }
    Ok(())
}

/// The raw log files (`.jsonl`, `.jsonl.gz`) of sessions, relative to `raw/`.
fn raw_files_of(raw: &Path, project: &str, sessions: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for s in sessions {
        for ext in ["jsonl", "jsonl.gz"] {
            let rel = format!("{project}/{s}.{ext}");
            if raw.join(&rel).is_file() {
                out.push(rel);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
