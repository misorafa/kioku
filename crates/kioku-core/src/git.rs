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

    /// Stages `paths` (relative to the root) and commits them with `message`, if anything changed.
    pub fn commit(&self, paths: &[String], message: &str) {
        if !self.enabled || paths.is_empty() {
            return;
        }
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        if !self.run(&add) {
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
            tracing::warn!(message, "git commit failed");
        }
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
}
