//! Session migration, consistent snapshots, recovery and operational integrity checks.

use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A second backup within this window is refused (SPEC-M2.7 §12).
pub const BACKUP_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Read-only operational evidence, available through the diagnostics endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReliabilityReport {
    /// Last server receipt, independent of client clocks.
    pub last_received: Option<String>,
    /// Last completed backup time.
    pub last_backup: Option<String>,
    /// Failure of the last attempted wiki commit, if any.
    pub git_error: Option<String>,
    /// Paths with missing, changed or unindexed page contents.
    pub inconsistent_pages: Vec<String>,
    /// Pages that cannot be read or parsed (reindex skips them; fix or remove by hand).
    #[serde(default)]
    pub unparseable_pages: Vec<String>,
    /// Whether indexed document count and metadata agree.
    pub index_count_matches: bool,
}

/// One file in a backup manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupFile {
    /// File length in bytes.
    pub bytes: u64,
    /// SHA-256 of the bytes.
    pub sha256: String,
}

/// Snapshot format with the wiki history as `wiki.bundle` (SPEC-M2.8 §4); format 1 copied
/// `wiki/.git` and is still restored.
pub const BACKUP_FORMAT: u32 = 2;

/// Name of the git bundle of the wiki history inside a snapshot.
pub const WIKI_BUNDLE: &str = "wiki.bundle";

#[cfg(test)]
thread_local! {
    /// How long the last backup on this thread held the write lock (SPEC-M2.8 §4 test).
    pub(crate) static BACKUP_LOCK_HELD: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// Completed portable snapshot of wiki, database and raw observations (no credentials).
///
/// Format 2 (SPEC-M2.8 §4): `wiki/` holds the working tree (no `.git`), copied under the
/// write lock; `wiki.bundle` holds the history, made after the lock was released, so it may
/// be a few commits *ahead* of the copied files (commits made in between). `wiki_head` is the
/// bundle's HEAD.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Snapshot format version.
    pub format: u32,
    /// Snapshot time in UTC.
    pub created: String,
    /// Server-local location (informational; never trusted during restore).
    pub path: String,
    /// Inventory, including the SQLite snapshot and git history.
    pub files: BTreeMap<String, BackupFile>,
    /// Expected SQLite table row counts after restore.
    pub counts: BTreeMap<String, u64>,
    /// HEAD commit of `wiki.bundle` (format 2; `None` without wiki history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiki_head: Option<String>,
}

const TABLES: [&str; 8] = [
    "projects",
    "sessions",
    "observations",
    "handoffs",
    "pages",
    "project_aliases",
    "observation_receipts",
    "page_redirects",
];

fn counts(conn: &Connection) -> anyhow::Result<BTreeMap<String, u64>> {
    TABLES
        .into_iter()
        .map(|t| Ok((t.to_string(), db::count(conn, t)?)))
        .collect()
}

fn files(root: &Path, at: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(at).with_context(|| format!("listing {}", at.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        anyhow::ensure!(
            !ty.is_symlink(),
            "symlink refused: {}",
            entry.path().display()
        );
        if ty.is_dir() {
            files(root, &entry.path(), out)?;
        } else if ty.is_file() {
            out.push(entry.path().strip_prefix(root)?.to_path_buf());
        } else {
            anyhow::bail!("special file refused: {}", entry.path().display());
        }
    }
    Ok(())
}

fn inventory(root: &Path) -> anyhow::Result<BTreeMap<String, BackupFile>> {
    let mut paths = Vec::new();
    files(root, root, &mut paths)?;
    let mut result = BTreeMap::new();
    for rel in paths {
        if rel == Path::new("manifest.json") {
            continue;
        }
        let mut file = std::fs::File::open(root.join(&rel))?;
        let mut digest = Sha256::new();
        let mut bytes = 0;
        let mut buf = [0u8; 65536];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf)?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
            bytes += n as u64;
        }
        result.insert(
            rel.to_string_lossy().replace('\\', "/"),
            BackupFile {
                bytes,
                sha256: format!("{:x}", digest.finalize()),
            },
        );
    }
    Ok(result)
}

fn copy_tree(from: &Path, into: &Path) -> anyhow::Result<()> {
    if !from.exists() {
        return Ok(());
    }
    anyhow::ensure!(
        !std::fs::symlink_metadata(from)?.file_type().is_symlink(),
        "symlink source refused"
    );
    let mut paths = Vec::new();
    files(from, from, &mut paths)?;
    for rel in paths {
        let dest = into.join(&rel);
        util::create_private_dir(dest.parent().context("missing parent")?)?;
        let source = from.join(&rel);
        let mut input = std::fs::File::open(&source)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&dest)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        std::fs::set_permissions(&dest, input.metadata()?.permissions())?;
    }
    Ok(())
}

/// Copies the wiki working tree (everything but `.git` and `.*.tmp` leftovers) into `into`.
fn copy_wiki_files(from: &Path, into: &Path) -> anyhow::Result<()> {
    util::create_private_dir(into)?;
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" || (name.starts_with('.') && name.ends_with(".tmp")) {
            continue;
        }
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree(&entry.path(), &into.join(&name))?;
        } else if ty.is_file() {
            let dest = into.join(&name);
            std::fs::copy(entry.path(), &dest)?;
            std::fs::File::open(&dest)?.sync_all()?;
        } else {
            anyhow::bail!("special file refused: {}", entry.path().display());
        }
    }
    Ok(())
}

/// Copies every file under `from` over `into`, replacing files that exist there.
fn overlay_tree(from: &Path, into: &Path) -> anyhow::Result<()> {
    let mut paths = Vec::new();
    files(from, from, &mut paths)?;
    for rel in paths {
        let dest = into.join(&rel);
        util::create_private_dir(dest.parent().context("missing parent")?)?;
        std::fs::copy(from.join(&rel), &dest)
            .with_context(|| format!("restoring {}", rel.display()))?;
    }
    Ok(())
}

impl Store {
    /// One-time move of session pages to their collision-free names (SPEC-M2.6 §1), with a
    /// redirect from each old path. Runs until it completes once (`reliability_meta`
    /// `session_pages_v2`); a page that cannot move is skipped with a warning and stays
    /// readable at its old path. Missing pages are never regenerated: finalizing again would
    /// issue a new rules handoff for a session that ended long ago.
    pub(super) fn migrate_session_pages(&self) -> Result<()> {
        const DONE: &str = "session_pages_v2";
        let done: Option<String> = self
            .db
            .lock()
            .query_row(
                "SELECT value FROM reliability_meta WHERE key=?1",
                [DONE],
                |r| r.get(0),
            )
            .optional()
            .context("reading migration state")?;
        if done.is_some() {
            return Ok(());
        }
        let _write = self.write_lock.lock();
        let wiki = self.dirs.wiki();
        let mut moved = 0usize;
        for path in list_wiki_pages(&wiki)? {
            match self.migrate_one_session_page(&wiki, &path) {
                Ok(true) => moved += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(%path, error = %e, "session page left at its old path"),
            }
        }
        // Commit and reindex on every unfinished run, so an interrupted earlier run (files
        // moved, index or git not yet updated) is completed by the next start.
        self.git
            .commit(&[".".to_string()], "kioku: migrate session page names");
        self.reindex_locked()?;
        self.db
            .lock()
            .execute(
                "INSERT OR REPLACE INTO reliability_meta VALUES (?1, ?2)",
                [DONE, &now_ts()],
            )
            .context("recording migration state")?;
        if moved > 0 {
            tracing::info!(moved, "session pages moved to collision-free names");
        }
        Ok(())
    }

    /// Moves one legacy session page; true when it moved. New copy and redirect are durable
    /// before the old file is removed, so an interruption never loses a page.
    fn migrate_one_session_page(&self, wiki: &Path, path: &str) -> anyhow::Result<bool> {
        let text = std::fs::read_to_string(wiki.join(path))?;
        let page = Page::parse(path, &text)?;
        if page.frontmatter.kind != PageKind::Session {
            return Ok(false);
        }
        let Some(id) = &page.frontmatter.session else {
            return Ok(false);
        };
        let Some(session) = db::get_session(&self.db.lock(), id)? else {
            return Ok(false);
        };
        let target = session_page_path(&session);
        if target == path {
            return Ok(false);
        }
        if !wiki.join(&target).exists() {
            write_atomic(&wiki.join(&target), &text)?;
        }
        self.db.lock().execute(
            "INSERT OR REPLACE INTO page_redirects VALUES (?1,?2)",
            params![path, target],
        )?;
        std::fs::remove_file(wiki.join(path))?;
        Ok(true)
    }

    /// Takes a consistent server-local snapshot and publishes it only after completion.
    ///
    /// Lock scope (SPEC-M2.6 §2, SPEC-M2.8 §4): the DB lock only for `VACUUM INTO` and
    /// recording the raw log lengths (raw lines are appended under the DB lock, so those
    /// prefixes match the snapshot); the write lock while the wiki's working tree (without
    /// `.git`) is copied. The history is bundled (`git bundle create --all`), raw prefixes
    /// are copied and everything is hashed after both locks are released; the bundle may
    /// therefore be a few commits ahead of the copied pages.
    ///
    /// A backup within [`BACKUP_MIN_INTERVAL`] of the last one is refused
    /// ([`Error::Conflict`]); after a successful one only the newest
    /// [`Config::backups_keep`] snapshots are kept (SPEC-M2.7 §12, SPEC-M2.8 §3).
    pub fn backup(&self) -> Result<BackupManifest> {
        if let Some(last) = self.meta("last_backup")?
            && let Some(at) = util::parse_ts(&last)
        {
            let age = util::now().signed_duration_since(at);
            if age >= chrono::Duration::zero()
                && age < chrono::Duration::from_std(BACKUP_MIN_INTERVAL).unwrap_or_default()
            {
                return Err(Error::Conflict(format!(
                    "a backup was made at {last}, less than {} s ago; wait a minute and try again",
                    BACKUP_MIN_INTERVAL.as_secs()
                )));
            }
        }
        let root = self.dirs.root().join("backups");
        util::create_private_dir(&root).context("creating backup root")?;
        let id = util::generate_token();
        let stage = root.join(format!(".{id}.tmp"));
        let dest = root.join(&id);
        let result = (|| -> anyhow::Result<BackupManifest> {
            let raw_lengths;
            let counts;
            {
                let _write = self.write_lock.lock();
                let held = std::time::Instant::now();
                remove_stale_stages(&root);
                util::create_private_dir(&stage.join("db")).context("creating snapshot stage")?;
                {
                    let conn = self.db.lock();
                    conn.execute(
                        "VACUUM INTO ?1",
                        [stage.join("db/kioku.sqlite").to_string_lossy().as_ref()],
                    )?;
                    counts = self::counts(&conn)?;
                    raw_lengths = file_lengths(&self.dirs.raw())?;
                }
                copy_wiki_files(&self.dirs.wiki(), &stage.join("wiki"))?;
                #[cfg(test)]
                BACKUP_LOCK_HELD.with(|c| c.set(held.elapsed()));
                let _ = held;
            }
            // Outside the write lock: commits made meanwhile only put the bundle ahead.
            let wiki_head = self.git.bundle(&stage.join(WIKI_BUNDLE))?;
            copy_prefixes(&self.dirs.raw(), &stage.join("raw"), &raw_lengths)?;
            let manifest = BackupManifest {
                format: BACKUP_FORMAT,
                created: now_ts(),
                path: dest.display().to_string(),
                files: inventory(&stage)?,
                counts,
                wiki_head,
            };
            let file = stage.join("manifest.json");
            std::fs::write(&file, serde_json::to_vec_pretty(&manifest)?)?;
            std::fs::OpenOptions::new()
                .write(true)
                .open(file)?
                .sync_all()?;
            std::fs::rename(&stage, &dest)?;
            self.db.lock().execute(
                "INSERT OR REPLACE INTO reliability_meta VALUES ('last_backup', ?1)",
                [&manifest.created],
            )?;
            Ok(manifest)
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&stage);
        }
        let manifest = result.context("creating backup")?;
        prune_backups(&root, self.config.backups_keep(), false);
        Ok(manifest)
    }

    /// A `reliability_meta` value.
    fn meta(&self, key: &str) -> Result<Option<String>> {
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

    /// Checks storage health without changing files, metadata or the search index. Only the
    /// metadata snapshot is read under the DB lock; files are read and hashed without any
    /// lock, so a write racing the check can show up as a momentary mismatch.
    pub fn reliability(&self) -> Result<ReliabilityReport> {
        let (last_received, last_backup, rows) = {
            let conn = self.db.lock();
            let meta = |key: &str| -> anyhow::Result<Option<String>> {
                Ok(conn
                    .query_row(
                        "SELECT value FROM reliability_meta WHERE key=?1",
                        [key],
                        |r| r.get(0),
                    )
                    .optional()?)
            };
            let mut stmt = conn
                .prepare("SELECT path, hash FROM pages")
                .context("listing page metadata")?;
            let rows: BTreeMap<String, Option<String>> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .context("reading page metadata")?
                .collect::<rusqlite::Result<_>>()
                .context("reading page metadata")?;
            (meta("last_received")?, meta("last_backup")?, rows)
        };
        let mut mismatches = Vec::new();
        let mut unparseable = Vec::new();
        let mut documents = Vec::new();
        let mut disk = std::collections::BTreeSet::new();
        for path in list_wiki_pages(&self.dirs.wiki())? {
            disk.insert(path.clone());
            let page = std::fs::read_to_string(self.dirs.wiki().join(&path))
                .ok()
                .and_then(|t| Page::parse(&path, &t).ok().map(|p| (p, t)));
            // reindex skips pages it cannot read or parse; report them, not as drift.
            let Some((page, text)) = page else {
                unparseable.push(path);
                continue;
            };
            let (row, doc) = page_records(&page, &text);
            if rows.get(&path).and_then(|h| h.as_deref()) != Some(row.hash.as_str()) {
                mismatches.push(path);
            }
            documents.push(doc);
        }
        mismatches.extend(rows.keys().filter(|p| !disk.contains(*p)).cloned());
        mismatches.extend(self.index.inconsistent_paths(&documents)?);
        mismatches.sort();
        mismatches.dedup();
        Ok(ReliabilityReport {
            last_received,
            last_backup,
            git_error: self.git.last_error(),
            inconsistent_pages: mismatches,
            unparseable_pages: unparseable,
            index_count_matches: self.index.num_docs() == rows.len() as u64,
        })
    }
}

/// Removes the oldest completed snapshots in `root` (by manifest time) until `keep` remain;
/// returns `(count, bytes)` removed (or, with `dry_run`, that would be). Best effort: a
/// snapshot that cannot be removed stays and is not counted.
pub(super) fn prune_backups(root: &Path, keep: usize, dry_run: bool) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    let mut snapshots: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let manifest: BackupManifest =
                serde_json::from_slice(&std::fs::read(e.path().join("manifest.json")).ok()?)
                    .ok()?;
            Some((manifest.created, e.path()))
        })
        .collect();
    if snapshots.len() <= keep {
        return (0, 0);
    }
    snapshots.sort();
    let excess = snapshots.len() - keep;
    let (mut count, mut bytes) = (0, 0);
    for (_, dir) in snapshots.into_iter().take(excess) {
        let size = super::maintenance::dir_bytes(&dir);
        if dry_run {
            count += 1;
            bytes += size;
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                count += 1;
                bytes += size;
            }
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "could not remove an old backup")
            }
        }
    }
    (count, bytes)
}

/// Removes `.<id>.tmp` stages left by an interrupted backup (the caller holds the write
/// lock, so no backup is in progress).
fn remove_stale_stages(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name.ends_with(".tmp") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Lengths of every file under `root` (relative paths); empty when `root` does not exist.
fn file_lengths(root: &Path) -> anyhow::Result<Vec<(PathBuf, u64)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    files(root, root, &mut paths)?;
    paths
        .into_iter()
        .map(|rel| Ok((rel.clone(), std::fs::metadata(root.join(&rel))?.len())))
        .collect()
}

/// Copies the first `len` bytes of each listed file (append-only logs that may have grown).
fn copy_prefixes(from: &Path, into: &Path, lengths: &[(PathBuf, u64)]) -> anyhow::Result<()> {
    for (rel, len) in lengths {
        let dest = into.join(rel);
        util::create_private_dir(dest.parent().context("missing parent")?)?;
        let input = std::fs::File::open(from.join(rel))
            .with_context(|| format!("opening {}", from.join(rel).display()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&dest)?;
        let copied = std::io::copy(&mut std::io::Read::take(input, *len), &mut output)?;
        anyhow::ensure!(copied == *len, "{} shrank during backup", rel.display());
        output.sync_all()?;
    }
    Ok(())
}

/// Format 2 (SPEC-M2.8 §4): `git clone` the staged `wiki.bundle` into `wiki/`, then
/// overlay the copied working tree. Verifies that the clone's HEAD is the manifest's and
/// that every copied page is known to the history (its content is in some commit — the
/// bundle may be ahead of the copy, never behind); a page that is not is reported as a
/// warning (an uncommitted hand edit), never dropped. Without `git`, the pages are restored
/// without history.
fn restore_wiki_history(stage: &Path, head: Option<&str>) -> anyhow::Result<()> {
    let bundle = stage.join(WIKI_BUNDLE);
    if !crate::git::git_available() {
        tracing::warn!("git not found: the wiki is restored without its history");
        std::fs::remove_file(&bundle)?;
        return Ok(());
    }
    let pages = stage.join(".wiki-pages");
    std::fs::rename(stage.join("wiki"), &pages)?;
    let wiki = stage.join("wiki");
    crate::git::run_git(
        stage,
        &[
            "clone",
            "-q",
            &bundle.to_string_lossy(),
            &wiki.to_string_lossy(),
        ],
    )
    .context("cloning the wiki bundle")?;
    let _ = crate::git::run_git(&wiki, &["remote", "remove", "origin"]);
    let got = crate::git::run_git(&wiki, &["rev-parse", "HEAD"])?;
    if let Some(want) = head {
        anyhow::ensure!(
            got == want,
            "wiki bundle HEAD {got} does not match the manifest ({want})"
        );
    }
    // The working tree becomes exactly the copied pages: files the bundle has beyond them
    // were committed after the copy (the database snapshot does not know them).
    let mut copied = Vec::new();
    files(&pages, &pages, &mut copied)?;
    let copied: std::collections::BTreeSet<String> = copied
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let mut checked_out = Vec::new();
    files(&wiki, &wiki, &mut checked_out)?;
    for rel in checked_out {
        let name = rel.to_string_lossy().replace('\\', "/");
        if !name.starts_with(".git/") && !copied.contains(&name) {
            std::fs::remove_file(wiki.join(&rel))?;
        }
    }
    overlay_tree(&pages, &wiki)?;
    for rel in &copied {
        let oid = crate::git::run_git(&wiki, &["hash-object", "--", rel])?;
        if crate::git::run_git(&wiki, &["cat-file", "-e", &oid]).is_err() {
            tracing::warn!(path = %rel, "restored page is not in the wiki history (an uncommitted edit?)");
        }
    }
    std::fs::remove_dir_all(&pages)?;
    std::fs::remove_file(&bundle)?;
    // Record the snapshot's pages on top of the bundle's history (only when they differ).
    crate::git::run_git(&wiki, &["add", "-A", "."])?;
    if crate::git::run_git(&wiki, &["diff", "--cached", "--quiet"]).is_err() {
        let name = format!("user.name={}", crate::git::AUTHOR_NAME);
        let email = format!("user.email={}", crate::git::AUTHOR_EMAIL);
        crate::git::run_git(
            &wiki,
            &[
                "-c",
                &name,
                "-c",
                &email,
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--no-verify",
                "-m",
                "kioku: restore backup",
            ],
        )?;
    }
    Ok(())
}

/// Restores a checked snapshot into a new directory, rebuilding and verifying its index.
pub fn restore_backup(source: &Path, into: &Path) -> anyhow::Result<BackupManifest> {
    anyhow::ensure!(
        std::fs::symlink_metadata(into).is_err(),
        "restore destination already exists"
    );
    anyhow::ensure!(
        !std::fs::symlink_metadata(source)?.file_type().is_symlink(),
        "symlink backup root refused"
    );
    let manifest: BackupManifest =
        serde_json::from_slice(&std::fs::read(source.join("manifest.json"))?)?;
    anyhow::ensure!(
        manifest.format == 1 || manifest.format == BACKUP_FORMAT,
        "unsupported backup format"
    );
    let actual = inventory(source)?;
    anyhow::ensure!(
        actual.len() == manifest.files.len(),
        "backup inventory differs"
    );
    for (path, expected) in &manifest.files {
        anyhow::ensure!(
            path.starts_with("wiki/")
                || path.starts_with("raw/")
                || path == "db/kioku.sqlite"
                || (path == WIKI_BUNDLE && manifest.format >= 2),
            "unexpected backup path"
        );
        let got = actual.get(path).context("missing backup file")?;
        anyhow::ensure!(
            got.bytes == expected.bytes && got.sha256 == expected.sha256,
            "backup checksum mismatch: {path}"
        );
    }
    let parent = into
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    anyhow::ensure!(
        !parent.canonicalize()?.starts_with(source.canonicalize()?),
        "restore destination must be outside the backup"
    );
    let stage = parent.join(format!(".kioku-restore-{}", util::generate_token()));
    util::create_private_dir(&stage).context("creating snapshot stage")?;
    let result = (|| -> anyhow::Result<()> {
        for dir in ["wiki", "raw", "db"] {
            copy_tree(&source.join(dir), &stage.join(dir))?;
        }
        let bundle = source.join(WIKI_BUNDLE);
        if manifest.files.contains_key(WIKI_BUNDLE) {
            std::fs::copy(&bundle, stage.join(WIKI_BUNDLE)).context("copying the wiki bundle")?;
        }
        // Recheck the staged copy, so changing a source during copy cannot bypass checks.
        let staged = inventory(&stage)?;
        anyhow::ensure!(
            serde_json::to_value(&staged)? == serde_json::to_value(&manifest.files)?,
            "snapshot changed during restore"
        );
        if manifest.files.contains_key(WIKI_BUNDLE) {
            restore_wiki_history(&stage, manifest.wiki_head.as_deref())?;
        }
        let conn = Connection::open(stage.join("db/kioku.sqlite"))?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        anyhow::ensure!(
            integrity == "ok",
            "SQLite integrity check failed: {integrity}"
        );
        anyhow::ensure!(
            !conn.prepare("PRAGMA foreign_key_check")?.exists([])?,
            "SQLite foreign key check failed"
        );
        anyhow::ensure!(
            counts(&conn)? == manifest.counts,
            "restored row counts differ"
        );
        drop(conn);
        let store = Store::open(Config::for_data_dir(&stage))?;
        // `pages` is rebuilt from the restored wiki (the index is not in the backup), so it
        // follows the wiki rather than the live table, which may have drifted; pages that
        // cannot be parsed are skipped by that rebuild and reported, never fatal.
        let without_pages = |mut c: BTreeMap<String, u64>| {
            c.remove("pages");
            c
        };
        anyhow::ensure!(
            without_pages(counts(&store.db.lock())?) == without_pages(manifest.counts.clone()),
            "restored row counts changed during rebuild"
        );
        let report = store.reliability()?;
        anyhow::ensure!(
            report.inconsistent_pages.is_empty() && report.index_count_matches,
            "restored index differs: {:?}",
            report.inconsistent_pages
        );
        for path in &report.unparseable_pages {
            tracing::warn!(%path, "restored page cannot be parsed; it is not searchable");
        }
        drop(store);
        anyhow::ensure!(
            !into.exists(),
            "restore destination appeared during verification"
        );
        std::fs::rename(&stage, into)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result.context("restoring backup")?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(store: &Store, id: &str, lane: Option<&str>) -> SessionStartRequest {
        let req = SessionStartRequest {
            session_id: id.into(),
            agent: "test".into(),
            cwd: "/test".into(),
            source: "startup".into(),
            project: ProjectIdentity {
                id: "test-project".into(),
                name: "試験".into(),
                root: "/test".into(),
                remote: None,
            },
            lane: lane.map(str::to_string),
        };
        store.start_session(&req).unwrap();
        req
    }
    fn obs(id: &str) -> NewObservation {
        NewObservation {
            event_id: Some("event-1".into()),
            session_id: id.into(),
            kind: ObservationKind::Prompt,
            ts: None,
            payload: serde_json::json!({"prompt":"日本語の検索と引き継ぎ"}),
        }
    }
    fn handoff(store: &Store, id: &str) -> Handoff {
        store
            .write_handoff(&HandoffInput {
                project: "test-project".into(),
                session: Some(id.into()),
                summary: "日本語の引き継ぎ".into(),
                next_steps: vec![],
                open_questions: vec![],
                decisions: vec![],
            })
            .unwrap()
    }

    #[test]
    fn delivery_receipt_survives_restart_and_rejects_different_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let store = Store::open(cfg.clone()).unwrap();
        start(&store, "s1", None);
        let mut original = obs("s1");
        original.payload =
            serde_json::from_str(r#"{"prompt":"日本語の検索","tool_name":"test"}"#).unwrap();
        assert_eq!(store.add_observation(&original).unwrap(), 1);
        assert_eq!(store.add_observation(&original).unwrap(), 1);
        let mut reordered = original.clone();
        reordered.payload =
            serde_json::from_str(r#"{"tool_name":"test","prompt":"日本語の検索"}"#).unwrap();
        assert_eq!(store.add_observation(&reordered).unwrap(), 1);
        let mut changed = original.clone();
        changed.payload = serde_json::json!({"prompt":"別の指示"});
        assert!(matches!(
            store.add_observation(&changed),
            Err(Error::Conflict(_))
        ));
        drop(store);
        let store = Store::open(cfg).unwrap();
        assert_eq!(store.add_observation(&original).unwrap(), 1);
        assert_eq!(store.observations("s1").unwrap().len(), 1);
    }

    /// Writes `text` at a legacy (pre-M2.6) session page path and clears the migration flag,
    /// as on a store created by an older version.
    fn legacy_page(store: &Store, root: &Path, rel: &str, text: &str) {
        let file = root.join("wiki").join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
        store
            .db
            .lock()
            .execute(
                "DELETE FROM reliability_meta WHERE key='session_pages_v2'",
                [],
            )
            .unwrap();
    }

    #[test]
    fn same_prefix_session_ids_get_distinct_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let mut paths = Vec::new();
        for id in ["01a0efaf-one", "01a0efaf-two"] {
            start(&store, id, None);
            store.add_observation(&obs(id)).unwrap();
            paths.push(store.finalize_session(id).unwrap().session_page.unwrap());
        }
        assert_ne!(paths[0], paths[1]);
        assert!(paths[0].contains("-01a0efaf-"), "{}", paths[0]);
        assert!(
            store
                .search("引き継ぎ", &SearchScope::All, 10)
                .unwrap()
                .len()
                >= 2
        );
    }

    /// Regression (review of ca8afe5): the migration regenerated missing session pages by
    /// finalizing again, which issued a fresh unaccepted rules handoff for a session that
    /// ended long ago; the next session got the stale summary and the real handoff was
    /// consumed. Migration now only moves pages.
    #[test]
    fn migration_moves_legacy_pages_once_without_issuing_handoffs() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let store = Store::open(cfg.clone()).unwrap();
        for id in ["01a0efaf-one", "01a0efaf-two"] {
            start(&store, id, None);
            store.add_observation(&obs(id)).unwrap();
            store.finalize_session(id).unwrap();
        }
        // Both rules handoffs consumed by a later session; a newer agent handoff pending.
        start(&store, "later", None);
        let real = handoff(&store, "later");
        let two = store.session("01a0efaf-two").unwrap();
        let current = session_page_path(&two);
        let text = std::fs::read_to_string(tmp.path().join("wiki").join(&current)).unwrap();
        // Simulate the pre-M2.6 layout: one colliding legacy file, the other page lost.
        let old = format!(
            "test-project/sessions/{}-01a0efaf.md",
            display_date(&now_ts())
        );
        std::fs::remove_file(tmp.path().join("wiki").join(&current)).unwrap();
        let one = session_page_path(&store.session("01a0efaf-one").unwrap());
        std::fs::remove_file(tmp.path().join("wiki").join(&one)).unwrap();
        legacy_page(&store, tmp.path(), &old, &text);
        let handoffs = db::count(&store.db.lock(), "handoffs").unwrap();
        drop(store);

        let store = Store::open(cfg.clone()).unwrap();
        assert!(tmp.path().join("wiki").join(&current).is_file());
        assert!(!tmp.path().join("wiki").join(&old).exists());
        assert_eq!(
            store
                .read_page(&old)
                .unwrap()
                .frontmatter
                .session
                .as_deref(),
            Some("01a0efaf-two")
        );
        assert!(
            !tmp.path().join("wiki").join(&one).exists(),
            "never regenerated"
        );
        assert_eq!(db::count(&store.db.lock(), "handoffs").unwrap(), handoffs);
        assert_eq!(
            store
                .pending_handoff_routed("test-project", false, None, None)
                .unwrap()
                .handoff
                .unwrap()
                .id,
            real.id
        );
        let report = store.reliability().unwrap();
        assert!(report.inconsistent_pages.is_empty(), "{report:?}");

        // Once done it never runs again: a legacy-looking file added later stays put.
        let late = format!(
            "test-project/sessions/{}-01a0efa0.md",
            display_date(&now_ts())
        );
        std::fs::write(tmp.path().join("wiki").join(&late), &text).unwrap();
        drop(store);
        let _store = Store::open(cfg).unwrap();
        assert!(tmp.path().join("wiki").join(&late).is_file());
    }

    #[test]
    fn a_page_that_cannot_migrate_never_stops_the_store_from_opening() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let store = Store::open(cfg.clone()).unwrap();
        legacy_page(
            &store,
            tmp.path(),
            "test-project/sessions/2026-09-01-broken.md",
            "---\nkind: session\nsession: [unclosed\n---\n壊れたページ\n",
        );
        drop(store);
        let store = Store::open(cfg).unwrap();
        assert!(
            tmp.path()
                .join("wiki/test-project/sessions/2026-09-01-broken.md")
                .is_file()
        );
        let done: Option<String> = store
            .db
            .lock()
            .query_row(
                "SELECT value FROM reliability_meta WHERE key='session_pages_v2'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(done.is_some());
    }

    /// Regression (review of ca8afe5): a SessionStart with a known session id (compact /
    /// resume) must not get the handoff it accepted earlier again; an offline replay start
    /// must not consume the pending one.
    #[test]
    fn resumed_and_replayed_starts_do_not_reissue_or_consume_handoffs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        start(&store, "writer", Some("feature"));
        let h = handoff(&store, "writer");
        let mut req = SessionStartRequest {
            session_id: "reader".into(),
            agent: "test".into(),
            cwd: "/test".into(),
            source: "startup".into(),
            project: ProjectIdentity {
                id: "test-project".into(),
                name: "試験".into(),
                root: "/test".into(),
                remote: None,
            },
            lane: Some("feature".into()),
        };
        assert_eq!(
            store
                .start_session(&req)
                .unwrap()
                .pending_handoff
                .unwrap()
                .id,
            h.id
        );
        req.source = "compact".into();
        assert!(store.start_session(&req).unwrap().pending_handoff.is_none());

        let newer = handoff(&store, "writer");
        let mut replay = req.clone();
        replay.session_id = "offline".into();
        replay.source = OFFLINE_REPLAY_SOURCE.into();
        assert!(
            store
                .start_session(&replay)
                .unwrap()
                .pending_handoff
                .is_none()
        );
        assert_eq!(
            store
                .pending_handoff_routed("test-project", false, None, Some("feature"))
                .unwrap()
                .handoff
                .unwrap()
                .id,
            newer.id
        );
    }

    #[test]
    fn conditional_page_updates_detect_stale_and_concurrent_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(Config::for_data_dir(tmp.path())).unwrap());
        let req = WritePageRequest {
            title: "共有メモ".into(),
            content: "初期版".into(),
            ..Default::default()
        };
        let path = store.write_page(&req).unwrap();
        let revision = store.read_page(&path).unwrap().revision;
        let handles: Vec<_> = ["変更A", "変更B"]
            .into_iter()
            .map(|body| {
                let store = store.clone();
                let revision = revision.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    store.write_page(&WritePageRequest {
                        title: "共有メモ".into(),
                        content: body.into(),
                        path: Some(path),
                        expected_revision: Some(revision),
                        ..Default::default()
                    })
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(Error::Conflict(_))))
                .count(),
            1
        );
    }

    /// Regression (review of ca8afe5): "" meant create-only, so clients that send "" for every
    /// optional argument could no longer update pages. "" is now unconditional.
    #[test]
    fn empty_expected_revision_is_an_unconditional_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let mut req = WritePageRequest {
            title: "空の版".into(),
            content: "一".into(),
            expected_revision: Some(String::new()),
            ..Default::default()
        };
        let path = store.write_page(&req).unwrap();
        req.content = "二".into();
        assert_eq!(store.write_page(&req).unwrap(), path);
        assert!(store.read_page(&path).unwrap().body.contains("二"));
    }

    /// Regression (review of ca8afe5): a `_global/` path written with a project, or a page
    /// moved by `project merge`, resolved to a path that did not exist, so every retry got
    /// "page changed; read again" forever.
    #[test]
    fn conditional_writes_follow_global_paths_and_redirects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        start(&store, "s", None);
        let global = store
            .write_page(&WritePageRequest {
                title: "全体メモ".into(),
                content: "共通".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(global.starts_with("_global/"));
        let rev = store.read_page(&global).unwrap().revision;
        let written = store
            .write_page(&WritePageRequest {
                title: "全体メモ".into(),
                content: "更新".into(),
                project: Some("test-project".into()),
                path: Some(global.clone()),
                expected_revision: Some(rev),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(written, global);

        let page = store
            .write_page(&WritePageRequest {
                title: "設計".into(),
                content: "旧".into(),
                project: Some("test-project".into()),
                ..Default::default()
            })
            .unwrap();
        let moved = page.replace("/pages/", "/pages/moved-");
        std::fs::rename(
            tmp.path().join("wiki").join(&page),
            tmp.path().join("wiki").join(&moved),
        )
        .unwrap();
        store
            .db
            .lock()
            .execute(
                "INSERT INTO page_redirects VALUES (?1, ?2)",
                params![page, moved],
            )
            .unwrap();
        let rev = store.read_page(&page).unwrap().revision;
        let written = store
            .write_page(&WritePageRequest {
                title: "設計".into(),
                content: "新".into(),
                project: Some("test-project".into()),
                path: Some(page.clone()),
                expected_revision: Some(rev),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(written, moved);

        let err = store
            .write_page(&WritePageRequest {
                title: "無い".into(),
                content: "x".into(),
                project: Some("test-project".into()),
                path: Some("none.md".into()),
                expected_revision: Some("abc".into()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn snapshot_restores_lanes_aliases_receipts_and_japanese_search() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(&tmp.path().join("live"))).unwrap();
        start(&store, "writer", Some("feature/検索"));
        let original = obs("writer");
        store.add_observation(&original).unwrap();
        let h = handoff(&store, "writer");
        store.finalize_session("writer").unwrap();
        db::upsert_alias(&store.db.lock(), "alias-project", "test-project", &now_ts()).unwrap();
        let manifest = store.backup().unwrap();
        assert!(store.reliability().unwrap().last_backup.is_some());
        let into = tmp.path().join("restored");
        restore_backup(Path::new(&manifest.path), &into).unwrap();
        assert!(restore_backup(Path::new(&manifest.path), &into).is_err());
        let restored = Store::open(Config::for_data_dir(&into)).unwrap();
        assert_eq!(
            restored.resolve_project_id("alias-project").unwrap(),
            "test-project"
        );
        assert_eq!(restored.add_observation(&original).unwrap(), 1);
        assert_eq!(
            restored
                .pending_handoff_routed("test-project", false, None, Some("feature/検索"))
                .unwrap()
                .handoff
                .unwrap()
                .id,
            h.id
        );
        assert!(
            !restored
                .search("引き継ぎ", &SearchScope::All, 3)
                .unwrap()
                .is_empty()
        );
        let db = Path::new(&manifest.path).join("db/kioku.sqlite");
        std::fs::write(db, "corrupt").unwrap();
        assert!(restore_backup(Path::new(&manifest.path), &tmp.path().join("bad")).is_err());
        assert!(!tmp.path().join("bad").exists());
    }

    /// Regression (review of ca8afe5): one unparseable page, or a page row whose file was
    /// removed by hand, made every restore fail with "restored index differs".
    #[test]
    fn a_backup_with_an_unparseable_page_or_drift_still_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        let store = Store::open(Config::for_data_dir(&live)).unwrap();
        let kept = store
            .write_page(&WritePageRequest {
                title: "残る記憶".into(),
                content: "日本語の本文".into(),
                ..Default::default()
            })
            .unwrap();
        let gone = store
            .write_page(&WritePageRequest {
                title: "消えた記憶".into(),
                content: "x".into(),
                ..Default::default()
            })
            .unwrap();
        std::fs::remove_file(live.join("wiki").join(&gone)).unwrap();
        std::fs::write(
            live.join("wiki/_global/broken.md"),
            "---\ntitle: [unclosed\n---\n壊れた\n",
        )
        .unwrap();
        let report = store.reliability().unwrap();
        assert_eq!(
            report.unparseable_pages,
            vec!["_global/broken.md".to_string()]
        );
        assert_eq!(report.inconsistent_pages, vec![gone]);
        let manifest = store.backup().unwrap();
        let into = tmp.path().join("restored");
        restore_backup(Path::new(&manifest.path), &into).unwrap();
        let restored = Store::open(Config::for_data_dir(&into)).unwrap();
        assert!(restored.read_page(&kept).is_ok());
        assert_eq!(
            restored.search("日本語", &SearchScope::All, 3).unwrap()[0].path,
            kept
        );
    }

    /// SPEC-M2.7 §12: a second backup within 60 s is refused; only `backup_keep` snapshots
    /// stay, the oldest go.
    #[test]
    fn backups_are_rate_limited_and_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_data_dir(tmp.path());
        cfg.server.backup_keep = 5;
        // SPEC-M2.8 §3: `[retention] backups_keep` supersedes `[server] backup_keep`.
        cfg.retention.backups_keep = Some(2);
        let store = Store::open(cfg).unwrap();
        start(&store, "s1", None);
        let first = store.backup().unwrap();
        assert!(matches!(store.backup(), Err(Error::Conflict(m)) if m.contains("wait a minute")));
        let set_last = |ts: &str| {
            store
                .db
                .lock()
                .execute(
                    "INSERT OR REPLACE INTO reliability_meta VALUES ('last_backup', ?1)",
                    [ts],
                )
                .unwrap();
        };
        let mut made = vec![first.path.clone()];
        for _ in 0..2 {
            set_last("2020-01-01T00:00:00Z");
            // Distinct, ordered manifest times.
            std::thread::sleep(std::time::Duration::from_millis(1100));
            made.push(store.backup().unwrap().path);
        }
        let left: Vec<String> = std::fs::read_dir(tmp.path().join("backups"))
            .unwrap()
            .flatten()
            .map(|e| e.path().display().to_string())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(!Path::new(&made[0]).exists(), "the oldest went");
        assert!(Path::new(&made[1]).exists() && Path::new(&made[2]).exists());
    }

    /// SPEC-M2.8 §4: the wiki history is bundled outside the write lock, so a backup holds
    /// it only briefly while another thread keeps finalizing; the restore clones the bundle,
    /// overlays the copied pages and keeps their history.
    #[test]
    fn backup_holds_the_write_lock_briefly_while_finalizing_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            Store::open(Config::for_data_dir(&tmp.path().join("live"))).unwrap(),
        );
        start(&store, "busy", None);
        store.add_observation(&obs("busy")).unwrap();
        store.finalize_session("busy").unwrap();
        for i in 0..30 {
            store
                .write_page(&WritePageRequest {
                    title: format!("履歴のある記憶 {i}"),
                    content: format!("日本語の本文 {i}"),
                    ..Default::default()
                })
                .unwrap();
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut e = obs("busy");
                    e.event_id = Some(format!("busy-{n}"));
                    e.payload = serde_json::json!({"prompt": format!("並行する指示 {n}")});
                    store.add_observation(&e).unwrap();
                    store.finalize_session("busy").unwrap();
                    n += 1;
                }
                n
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let manifest = store.backup().unwrap();
        let held = BACKUP_LOCK_HELD.with(|c| c.get());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let turns = worker.join().unwrap();
        assert!(turns > 0);
        assert!(
            held < std::time::Duration::from_secs(1),
            "write lock held {held:?}"
        );
        assert_eq!(manifest.format, BACKUP_FORMAT);
        assert!(
            !manifest.files.keys().any(|p| p.starts_with("wiki/.git/")),
            "no .git copy"
        );
        let git = crate::git::git_available();
        if git {
            assert!(manifest.files.contains_key(WIKI_BUNDLE));
            assert!(manifest.wiki_head.is_some());
        }
        let into = tmp.path().join("restored");
        restore_backup(Path::new(&manifest.path), &into).unwrap();
        let restored = Store::open(Config::for_data_dir(&into)).unwrap();
        assert!(
            !restored
                .search("履歴", &SearchScope::All, 3)
                .unwrap()
                .is_empty()
        );
        assert!(
            restored
                .reliability()
                .unwrap()
                .inconsistent_pages
                .is_empty()
        );
        drop(restored);
        if git {
            let wiki = into.join("wiki");
            let log = crate::git::run_git(&wiki, &["log", "--format=%s"]).unwrap();
            assert!(log.lines().count() >= 30, "{log}");
            assert!(
                crate::git::run_git(&wiki, &["status", "--porcelain"])
                    .unwrap()
                    .is_empty(),
                "the restored wiki is clean"
            );
            assert!(crate::git::run_git(&wiki, &["remote"]).unwrap().is_empty());
            // A tampered HEAD in the manifest is refused.
            let mut bad = manifest.clone();
            bad.wiki_head = Some("0".repeat(40));
            std::fs::write(
                Path::new(&manifest.path).join("manifest.json"),
                serde_json::to_vec(&bad).unwrap(),
            )
            .unwrap();
            let err =
                restore_backup(Path::new(&manifest.path), &tmp.path().join("bad")).unwrap_err();
            assert!(format!("{err:#}").contains("does not match"), "{err:#}");
            assert!(!tmp.path().join("bad").exists());
        }
    }

    #[test]
    fn an_interrupted_backup_stage_is_cleaned_up_by_the_next_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let stale = tmp.path().join("backups/.deadbeef.tmp");
        std::fs::create_dir_all(stale.join("wiki")).unwrap();
        store.backup().unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn snapshot_and_concurrent_receipts_have_matching_raw_and_database_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            Store::open(Config::for_data_dir(&tmp.path().join("live"))).unwrap(),
        );
        start(&store, "concurrent", None);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer = {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for n in 0..20 {
                    let mut e = obs("concurrent");
                    e.event_id = Some(format!("event-{n}"));
                    store.add_observation(&e).unwrap();
                }
            })
        };
        barrier.wait();
        let manifest = store.backup().unwrap();
        writer.join().unwrap();
        let restored = tmp.path().join("restored");
        restore_backup(Path::new(&manifest.path), &restored).unwrap();
        let raw = std::fs::read_to_string(restored.join("raw/test-project/concurrent.jsonl"))
            .unwrap_or_default();
        assert_eq!(raw.lines().count() as u64, manifest.counts["observations"]);
        assert_eq!(store.observations("concurrent").unwrap().len(), 20);
    }

    #[test]
    fn diagnostics_find_stale_index_content_even_when_counts_match() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let path = store
            .write_page(&WritePageRequest {
                title: "日本語の記憶".into(),
                content: "引き継ぎ".into(),
                ..Default::default()
            })
            .unwrap();
        let text = std::fs::read_to_string(tmp.path().join("wiki").join(&path)).unwrap();
        let page = store.read_page(&path).unwrap();
        let (_, mut doc) = page_records(&page, &text);
        doc.body = "誤った検索内容".into();
        store.index.upsert(&doc).unwrap();
        let report = store.reliability().unwrap();
        assert!(report.index_count_matches);
        assert_eq!(report.inconsistent_pages, vec![path.clone()]);
        store.reindex().unwrap();
        assert!(store.reliability().unwrap().inconsistent_pages.is_empty());
        assert_eq!(
            store.search("引き継ぎ", &SearchScope::All, 1).unwrap()[0].path,
            path
        );
    }

    #[test]
    fn diagnostics_detect_external_edits_and_missing_files_without_repairing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let path = store
            .write_page(&WritePageRequest {
                title: "記憶".into(),
                content: "本文".into(),
                ..Default::default()
            })
            .unwrap();
        let file = tmp.path().join("wiki").join(&path);
        std::fs::write(&file, "手動編集").unwrap();
        assert_eq!(
            store.reliability().unwrap().inconsistent_pages,
            vec![path.clone()]
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "手動編集");
        std::fs::remove_file(file).unwrap();
        assert_eq!(store.reliability().unwrap().inconsistent_pages, vec![path]);
    }

    /// SPEC-M2.7 §6: one process per data directory; the lock goes with the store.
    #[test]
    fn a_second_open_of_the_same_directory_fails_until_the_first_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let store = Store::open(cfg.clone()).unwrap();
        let err = Store::open(cfg.clone())
            .err()
            .expect("second open must fail");
        let msg = err.to_string();
        assert!(msg.contains("another kioku is using"), "{msg}");
        assert!(msg.contains(&tmp.path().display().to_string()), "{msg}");
        // Other directories are unaffected.
        let other = tempfile::tempdir().unwrap();
        drop(Store::open(Config::for_data_dir(other.path())).unwrap());
        drop(store);
        let again = Store::open(cfg).unwrap();
        assert!(tmp.path().join(crate::store::LOCK_FILE).is_file());
        drop(again);
        assert!(
            tmp.path().join(crate::store::LOCK_FILE).is_file(),
            "the lock file is never deleted"
        );
    }

    /// SPEC-M2.7 §6: a page that reached disk and SQLite but not the index (flagged
    /// `needs_reindex`) is indexed at the next start, and the flag is cleared.
    #[test]
    fn needs_reindex_heals_the_index_at_the_next_open() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_data_dir(tmp.path());
        let store = Store::open(cfg.clone()).unwrap();
        store
            .write_page(&WritePageRequest {
                title: "既存".into(),
                content: "最初のページ".into(),
                ..Default::default()
            })
            .unwrap();
        // What a failed index upsert leaves behind: the file, no index entry, the flag.
        let file = tmp.path().join("wiki/_global/索引漏れ.md");
        std::fs::write(
            &file,
            "---\ntitle: 索引漏れ\nscope: global\nkind: page\ncreated: 2026-10-01T00:00:00Z\nupdated: 2026-10-01T00:00:00Z\n---\n形態素解析の自己修復\n",
        )
        .unwrap();
        store
            .db
            .lock()
            .execute(
                "INSERT OR REPLACE INTO reliability_meta VALUES ('needs_reindex', '1')",
                [],
            )
            .unwrap();
        assert!(
            store
                .search("自己修復", &SearchScope::All, 3)
                .unwrap()
                .is_empty()
        );
        drop(store);
        let store = Store::open(cfg).unwrap();
        let hits = store.search("自己修復", &SearchScope::All, 3).unwrap();
        assert_eq!(hits[0].path, "_global/索引漏れ.md");
        assert!(!store.needs_reindex().unwrap());
    }
}
