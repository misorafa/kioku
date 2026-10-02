//! Git commit helper for the wiki: shells out to `git`, tolerates its absence (warns once).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Commit author name.
pub const AUTHOR_NAME: &str = "kioku";
/// Commit author email.
pub const AUTHOR_EMAIL: &str = "kioku@localhost";

/// The wiki repository. Every failure is logged, never returned: git is best-effort.
#[derive(Clone, Debug)]
pub struct Git {
    root: PathBuf,
    enabled: bool,
    last_error: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// Failed `git add` / `git commit` runs since this process opened the repository
    /// (`kioku_git_commit_failures_total`, SPEC-M3.2 §1).
    failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// True when a `git` executable can be run (checked once per process; warns once if not).
pub fn git_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let ok = crate::util::quiet_command("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            tracing::warn!("git not found on PATH; wiki changes will not be committed");
        }
        ok
    })
}

impl Git {
    /// Opens (and `git init`s if needed) the repository at `root`.
    pub fn open(root: &Path) -> Git {
        let enabled = git_available();
        let git = Git {
            root: root.to_path_buf(),
            enabled,
            last_error: Default::default(),
            failures: Default::default(),
        };
        if enabled && !root.join(".git").exists() {
            let ok = git.run(&["-c", "init.defaultBranch=main", "init", "-q"]);
            if !ok {
                tracing::warn!(root = %root.display(), "git init failed; continuing without commits");
                return Git {
                    enabled: false,
                    ..git
                };
            }
        }
        git
    }

    /// Whether commits are actually made.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Failed `git add` / `git commit` runs since the repository was opened.
    pub fn commit_failures(&self) -> u64 {
        self.failures.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn count_failure(&self, what: &str) {
        self.failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.last_error.lock() = Some(what.to_string());
    }

    /// Failure of the most recent commit attempt.
    pub fn last_error(&self) -> Option<String> {
        if let Some(e) = self.last_error.lock().clone() {
            return Some(e);
        }
        if !self.enabled {
            return Some("git history is disabled".into());
        }
        // Read-only: without --no-optional-locks `git status` may rewrite .git/index, and a
        // deadline kill mid-write would leave index.lock behind and break every later commit.
        let mut cmd = crate::util::quiet_command("git");
        cmd.arg("-C").arg(&self.root).args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain",
        ]);
        match crate::util::output_with_deadline(cmd, std::time::Duration::from_secs(2)) {
            Some(out) if out.status.success() && out.stdout.is_empty() => None,
            Some(out) if out.status.success() => Some("wiki contains uncommitted changes".into()),
            _ => Some("cannot inspect wiki git history".into()),
        }
    }

    /// Stages `paths` (relative to the root) and commits them with `message`, if anything changed.
    pub fn commit(&self, paths: &[String], message: &str) {
        if !self.enabled || paths.is_empty() {
            return;
        }
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        if !self.run(&add) {
            self.count_failure("git add failed");
            tracing::warn!(?paths, "git add failed");
            return;
        }
        let mut diff = vec!["diff", "--cached", "--quiet", "--"];
        diff.extend(paths.iter().map(String::as_str));
        if self.run(&diff) {
            return; // nothing staged for these paths
        }
        let name = format!("user.name={AUTHOR_NAME}");
        let email = format!("user.email={AUTHOR_EMAIL}");
        let mut commit = vec![
            "-c",
            name.as_str(),
            "-c",
            email.as_str(),
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            message,
            "--",
        ];
        commit.extend(paths.iter().map(String::as_str));
        if !self.run(&commit) {
            self.count_failure("git commit failed");
            tracing::warn!(message, "git commit failed");
        } else {
            *self.last_error.lock() = None;
        }
    }

    /// `git bundle create <dest> --all` (SPEC-M2.8 §4: the wiki history of a backup, made
    /// without the write lock); returns the bundle's HEAD commit id, `None` when there is
    /// no history to bundle (git disabled, or no commit yet).
    pub fn bundle(&self, dest: &Path) -> anyhow::Result<Option<String>> {
        if !self.enabled || !self.root.join(".git").exists() {
            return Ok(None);
        }
        if run_git(&self.root, &["rev-parse", "--verify", "-q", "HEAD"]).is_err() {
            return Ok(None);
        }
        let dest_s = dest.to_string_lossy().to_string();
        run_git(&self.root, &["bundle", "create", "-q", &dest_s, "--all"])?;
        let heads = run_git(&self.root, &["bundle", "list-heads", &dest_s])?;
        Ok(heads
            .lines()
            .find(|l| l.ends_with(" HEAD"))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string))
    }

    fn run(&self, args: &[&str]) -> bool {
        match crate::util::quiet_command("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
        {
            Ok(out) => {
                if !out.status.success() {
                    tracing::debug!(
                        ?args,
                        stderr = %String::from_utf8_lossy(&out.stderr),
                        "git command failed"
                    );
                }
                out.status.success()
            }
            Err(e) => {
                tracing::warn!(error = %e, "running git failed");
                false
            }
        }
    }
}

/// Runs `git -C <dir> <args>` and returns its trimmed stdout; a non-zero exit is an error
/// carrying stderr.
pub fn run_git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = crate::util::quiet_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("running git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn commits_only_when_changed() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let git = Git::open(tmp.path());
        assert!(git.enabled());
        std::fs::write(tmp.path().join("a.md"), "one").unwrap();
        git.commit(&["a.md".to_string()], "kioku: page a.md");
        git.commit(&["a.md".to_string()], "kioku: page a.md");
        let log = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["log", "--format=%an <%ae> %s"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.contains("kioku <kioku@localhost> kioku: page a.md"));
    }

    /// SPEC-M3.2 §1: a failed commit counts towards `kioku_git_commit_failures_total`.
    #[test]
    fn failed_commits_are_counted() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let git = Git::open(tmp.path());
        assert_eq!(git.commit_failures(), 0);
        // A leftover index lock makes `git add` fail.
        std::fs::write(tmp.path().join(".git").join("index.lock"), "").unwrap();
        std::fs::write(tmp.path().join("記憶.md"), "一").unwrap();
        git.commit(&["記憶.md".to_string()], "kioku: page");
        assert_eq!(git.commit_failures(), 1);
        assert!(git.last_error().is_some());
        // Clones share the counter (the store hands its Git to background work).
        let clone = git.clone();
        clone.commit(&["記憶.md".to_string()], "kioku: page");
        assert_eq!(git.commit_failures(), 2);
    }
}
