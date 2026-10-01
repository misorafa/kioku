//! The project identity cache of the SessionStart hook (SPEC-M2.7 §8):
//! `state/projects.json` maps a working directory to its identity and default branch, valid
//! while the repository's `HEAD` and `config` (and a `.kioku.toml`, when that is where the
//! identity came from) keep their modification times, for at most 24 hours. A hit needs no
//! git for the identity; the lane still costs one `git symbolic-ref`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{GitBudget, GitOut, PROJECT_FILE, ProjectIdentity, identify_with, lane_for};
use crate::error::Result;

/// File name inside `<kioku dir>/state/`.
pub const CACHE_FILE: &str = "projects.json";
/// An entry older than this is recomputed even when nothing changed.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// Entries kept (the oldest are dropped first).
pub const MAX_ENTRIES: usize = 256;

/// One cached working directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedProject {
    /// Project id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Project root.
    pub root: String,
    /// Normalized remote.
    #[serde(default)]
    pub remote: Option<String>,
    /// Default branch (`None`: none / unknown yet).
    #[serde(default)]
    pub default_branch: Option<String>,
    /// When the entry was computed (unix seconds).
    pub at: i64,
    /// Files whose modification time (ns since the epoch) must be unchanged.
    #[serde(default)]
    pub stamps: Vec<(String, u64)>,
}

impl CachedProject {
    /// The identity part.
    pub fn identity(&self) -> ProjectIdentity {
        ProjectIdentity {
            id: self.id.clone(),
            name: self.name.clone(),
            root: self.root.clone(),
            remote: self.remote.clone(),
        }
    }
}

/// The whole cache file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCache {
    /// cwd → entry.
    #[serde(default)]
    pub entries: BTreeMap<String, CachedProject>,
}

impl ProjectCache {
    /// Reads the cache; missing or corrupt → empty (the cache is advisory).
    pub fn load(file: &Path) -> ProjectCache {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Writes the cache atomically (temp file + rename); errors are ignored.
    pub fn save(&self, file: &Path) {
        let Some(dir) = file.parent() else {
            return;
        };
        if crate::util::create_private_dir(dir).is_err() {
            return;
        }
        let tmp = dir.join(format!("{CACHE_FILE}.{}.tmp", std::process::id()));
        let Ok(text) = serde_json::to_string(self) else {
            return;
        };
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Adds `entry` under `cwd`, dropping the oldest entries beyond [`MAX_ENTRIES`].
    pub fn insert(&mut self, cwd: String, entry: CachedProject) {
        self.entries.insert(cwd, entry);
        while self.entries.len() > MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }
}

/// Modification time of `path` in ns since the epoch.
fn mtime(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64)
}

/// `HEAD` and `config` of the repository rooted at `root`: `.git/…` for a checkout, the
/// worktree's gitdir (`HEAD`) and the common dir (`config`) for a linked worktree. Empty
/// when `root` has no `.git`.
pub fn git_files(root: &Path) -> Vec<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return vec![dot.join("HEAD"), dot.join("config")];
    }
    let Ok(text) = std::fs::read_to_string(&dot) else {
        return Vec::new();
    };
    let Some(gitdir) = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))
        .map(|g| root.join(g.trim()))
    else {
        return Vec::new();
    };
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|c| gitdir.join(c.trim()))
        .unwrap_or_else(|| gitdir.clone());
    vec![gitdir.join("HEAD"), common.join("config")]
}

/// The files an identity depends on, with their current modification times; `None` when
/// one is missing (then the identity is not cached).
fn stamps_for(identity: &ProjectIdentity, cwd: &Path) -> Option<Vec<(String, u64)>> {
    let root = Path::new(&identity.root);
    let mut files = git_files(root);
    if let Some(f) = super::project_file_above(cwd) {
        files.push(f);
    }
    if files.is_empty() {
        // Not a repository and no .kioku.toml: nothing tells us when it changes.
        return None;
    }
    files
        .into_iter()
        .map(|f| Some((f.display().to_string(), mtime(&f)?)))
        .collect()
}

/// True while `entry` is younger than [`MAX_AGE`] at `now` (unix seconds), its stamps are
/// unchanged, and no `.kioku.toml` appeared above `cwd` that it did not come from.
pub fn is_fresh(entry: &CachedProject, cwd: &Path, now: i64) -> bool {
    if now - entry.at > MAX_AGE.as_secs() as i64 || now < entry.at {
        return false;
    }
    if !entry
        .stamps
        .iter()
        .all(|(f, t)| mtime(Path::new(f)) == Some(*t))
    {
        return false;
    }
    match super::project_file_above(cwd) {
        Some(f) => {
            let f = f.display().to_string();
            entry.stamps.iter().any(|(s, _)| *s == f)
        }
        None => !entry.stamps.iter().any(|(s, _)| s.ends_with(PROJECT_FILE)),
    }
}

/// The default branch within `git`'s budget: `Ok(None)` when there is none, `Err(())` when
/// git ran out of time (unknown; not cached).
fn default_branch_with(dir: &Path, git: &GitBudget) -> std::result::Result<Option<String>, ()> {
    match git.run(
        dir,
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
    ) {
        GitOut::Text(head) => {
            let name = head.strip_prefix("origin/").unwrap_or(&head);
            if !name.is_empty() {
                return Ok(Some(name.to_string()));
            }
        }
        GitOut::TimedOut => return Err(()),
        GitOut::Nothing => {}
    }
    for b in ["main", "master"] {
        match git.run(
            dir,
            &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
        ) {
            GitOut::Text(_) => return Ok(Some(b.to_string())),
            GitOut::TimedOut => return Err(()),
            GitOut::Nothing => {}
        }
    }
    Ok(None)
}

/// Identity and handoff lane of `cwd` within `git`'s budget, through the cache in
/// `cache_file` (no cache when `None`). A hit costs one `git symbolic-ref` (the lane); a
/// miss runs [`identify_with`] and the default-branch lookup, then stores the result. The
/// lane is `None` when git runs out of time for it (the identity never is a guess: a
/// timed-out identity is an error).
pub fn identify_and_lane(
    cwd: &Path,
    cache_file: Option<&Path>,
    git: &GitBudget,
) -> Result<(ProjectIdentity, Option<String>)> {
    let key = cwd.display().to_string();
    let now = crate::util::now().timestamp();
    let mut cache = cache_file.map(ProjectCache::load).unwrap_or_default();
    let hit = cache
        .entries
        .get(&key)
        .filter(|e| is_fresh(e, cwd, now))
        .cloned();
    let (entry, store) = match hit {
        Some(e) => (e, false),
        None => {
            let identity = identify_with(cwd, git)?;
            let default = default_branch_with(cwd, git);
            let stamps = stamps_for(&identity, cwd);
            let entry = CachedProject {
                id: identity.id,
                name: identity.name,
                root: identity.root,
                remote: identity.remote,
                default_branch: default.clone().ok().flatten(),
                at: now,
                stamps: stamps.clone().unwrap_or_default(),
            };
            // Cache only what can be validated, and only a known default branch.
            (entry, stamps.is_some() && default.is_ok())
        }
    };
    let lane = match (
        &entry.default_branch,
        git.run(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]),
    ) {
        (Some(default), GitOut::Text(branch)) => lane_for(&branch, default),
        _ => None,
    };
    if store && let Some(file) = cache_file {
        cache.insert(key, entry.clone());
        cache.save(file);
    }
    Ok((entry.identity(), lane))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn repo(dir: &Path) -> bool {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["-c", "init.defaultBranch=main", "init", "-q"])
            && git(
                dir,
                &["remote", "add", "origin", "https://github.com/u/記憶.git"],
            )
            && git(
                dir,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "x",
                ],
            )
    }

    /// A `git` shim that appends one line per call to `<dir>/calls`, optionally sleeps,
    /// then runs the real git.
    #[cfg(unix)]
    fn shim(dir: &Path, sleep: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let real = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let path = dir.join("git-shim");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n{sleep}\nexec '{real}' \"$@\"\n",
                dir.join("calls").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[cfg(unix)]
    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// SPEC-M2.7 §8: a slow git never holds the hook past its budget; the identity is then
    /// an error, never a path-derived guess.
    #[cfg(unix)]
    #[test]
    fn a_slow_git_is_cut_off_within_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("作業");
        if !repo(&work) {
            return;
        }
        let slow = shim(tmp.path(), "sleep 5");
        let started = std::time::Instant::now();
        let budget = GitBudget::with_program(&slow, Duration::from_millis(50));
        let err = identify_with(&work, &budget).unwrap_err();
        assert!(err.to_string().contains("did not answer in time"), "{err}");
        let r = identify_and_lane(&work, Some(&tmp.path().join(CACHE_FILE)), &budget);
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
        assert!(!tmp.path().join(CACHE_FILE).exists(), "nothing cached");
    }

    /// SPEC-M2.7 §8: a cache hit needs no git for the identity, only `symbolic-ref HEAD`
    /// for the lane; a checkout (HEAD changes) invalidates it.
    #[cfg(unix)]
    #[test]
    fn a_cache_hit_skips_identify_and_a_checkout_invalidates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("repo");
        if !repo(&work) {
            return;
        }
        let counted = shim(tmp.path(), "");
        let file = tmp.path().join("state").join(CACHE_FILE);
        let budget = || GitBudget::with_program(&counted, Duration::from_secs(10));

        let (first, lane) = identify_and_lane(&work, Some(&file), &budget()).unwrap();
        assert_eq!(lane, None);
        assert_eq!(first, super::super::identify(&work).unwrap());
        assert!(first.remote.is_some());
        let miss = calls(tmp.path()).len();
        assert!(miss >= 3, "{:?}", calls(tmp.path()));
        assert!(file.is_file());

        let (again, lane) = identify_and_lane(&work, Some(&file), &budget()).unwrap();
        assert_eq!(again, first);
        assert_eq!(lane, None);
        let hit: Vec<String> = calls(tmp.path())[miss..].to_vec();
        assert_eq!(hit.len(), 1, "{hit:?}");
        assert!(hit[0].ends_with("symbolic-ref --short -q HEAD"), "{hit:?}");

        // A branch checkout rewrites HEAD: recomputed, and the lane follows the branch.
        std::thread::sleep(Duration::from_millis(20));
        assert!(git(&work, &["checkout", "-q", "-b", "feature/検索"]));
        let before = calls(tmp.path()).len();
        let (same, lane) = identify_and_lane(&work, Some(&file), &budget()).unwrap();
        assert_eq!(same.id, first.id);
        assert_eq!(lane.as_deref(), Some("feature/検索"));
        assert!(
            calls(tmp.path()).len() - before > 1,
            "a miss runs identify again"
        );
    }

    #[test]
    fn freshness_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("HEAD");
        std::fs::write(&f, "ref: refs/heads/main\n").unwrap();
        let t = mtime(&f).unwrap();
        let entry = CachedProject {
            id: "p-12345678".into(),
            name: "p".into(),
            root: tmp.path().display().to_string(),
            remote: None,
            default_branch: Some("main".into()),
            at: 1_000_000,
            stamps: vec![(f.display().to_string(), t)],
        };
        assert!(is_fresh(&entry, tmp.path(), 1_000_000 + 3600));
        assert!(
            !is_fresh(&entry, tmp.path(), 1_000_000 + 25 * 3600),
            "too old"
        );
        let mut changed = entry.clone();
        changed.stamps[0].1 += 1;
        assert!(!is_fresh(&changed, tmp.path(), 1_000_001));
        // A .kioku.toml that appeared later invalidates a git-derived entry.
        std::fs::write(tmp.path().join(PROJECT_FILE), "project = \"x\"\n").unwrap();
        assert!(!is_fresh(&entry, tmp.path(), 1_000_001));
        // Eviction keeps the newest MAX_ENTRIES.
        let mut cache = ProjectCache::default();
        for i in 0..(MAX_ENTRIES as i64 + 3) {
            cache.insert(
                format!("/w/{i}"),
                CachedProject {
                    at: i,
                    ..entry.clone()
                },
            );
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(!cache.entries.contains_key("/w/0"));
        assert!(
            cache
                .entries
                .contains_key(&format!("/w/{}", MAX_ENTRIES + 2))
        );
    }
}
