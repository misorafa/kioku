//! Project identity (spec §4): `.kioku.toml`, git remote, or path → stable project id; the
//! handoff lane of a working directory (M2.4 §1) and the root / id helpers behind project
//! aliases (M2.4 §2). Every git call of the hook path is bounded by a deadline, and
//! [`cache`] remembers identities so SessionStart rarely needs git at all (SPEC-M2.7 §8).

pub mod cache;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::util::sha256_hex;

/// Name of the per-repo override file.
pub const PROJECT_FILE: &str = ".kioku.toml";

/// Who a working directory belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIdentity {
    /// Stable id, e.g. `kioku-3f9a1c2e`.
    pub id: String,
    /// Human name: `.kioku.toml` `name`, else the remote's repo name when the id comes
    /// from the remote, else the basename of the project root.
    pub name: String,
    /// Absolute project root.
    pub root: String,
    /// Normalized git remote (`host/owner/repo`), if any.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Deserialize)]
struct ProjectFile {
    project: String,
    name: Option<String>,
}

/// Deadline of [`identify`] when the caller has none (commands, not hooks).
pub const IDENTIFY_DEADLINE: Duration = Duration::from_secs(30);

/// Bounded git calls sharing one deadline: each call gets the time that is left.
#[derive(Clone, Debug)]
pub struct GitBudget {
    /// The git program (`git`; a shim in tests).
    pub program: String,
    /// When the budget runs out.
    pub end: Instant,
}

/// Outcome of one bounded git call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitOut {
    /// Success with non-empty (trimmed) stdout.
    Text(String),
    /// Failure, empty output or no git at all.
    Nothing,
    /// The budget ran out before git answered (it was killed).
    TimedOut,
}

impl GitBudget {
    /// `git` with `budget` from now.
    pub fn new(budget: Duration) -> GitBudget {
        GitBudget::with_program("git", budget)
    }

    /// `program` (a git shim in tests) with `budget` from now.
    pub fn with_program(program: &str, budget: Duration) -> GitBudget {
        GitBudget {
            program: program.to_string(),
            end: Instant::now() + budget,
        }
    }

    /// Time left.
    pub fn remaining(&self) -> Duration {
        self.end.saturating_duration_since(Instant::now())
    }

    /// `git -C <dir> <args>` within the remaining time.
    pub fn run(&self, dir: &Path, args: &[&str]) -> GitOut {
        let left = self.remaining();
        if left.is_zero() {
            return GitOut::TimedOut;
        }
        let mut cmd = crate::util::quiet_command(&self.program);
        cmd.arg("-C").arg(dir).args(args);
        let started = Instant::now();
        match crate::util::output_with_deadline(cmd, left) {
            Some(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() {
                    GitOut::Nothing
                } else {
                    GitOut::Text(s)
                }
            }
            Some(_) => GitOut::Nothing,
            None if started.elapsed() >= left => GitOut::TimedOut,
            None => GitOut::Nothing,
        }
    }

    /// [`GitBudget::run`] where a timeout is an error (an identity must never be guessed).
    fn required(&self, dir: &Path, args: &[&str]) -> Result<Option<String>> {
        match self.run(dir, args) {
            GitOut::Text(s) => Ok(Some(s)),
            GitOut::Nothing => Ok(None),
            GitOut::TimedOut => Err(anyhow::anyhow!(
                "git {} did not answer in time in {}",
                args.join(" "),
                dir.display()
            )
            .into()),
        }
    }
}

/// Identifies the project that `cwd` belongs to (spec §4 priority order), allowing git
/// [`IDENTIFY_DEADLINE`].
pub fn identify(cwd: &Path) -> Result<ProjectIdentity> {
    identify_within(cwd, IDENTIFY_DEADLINE)
}

/// [`identify`] with every git call inside `deadline` (SPEC-M2.7 §8). When git does not
/// answer in time this fails rather than fall back to a path-derived id, which would file
/// the session under a wrong project.
pub fn identify_within(cwd: &Path, deadline: Duration) -> Result<ProjectIdentity> {
    identify_with(cwd, &GitBudget::new(deadline))
}

/// [`identify`] using `git`'s budget.
pub fn identify_with(cwd: &Path, git: &GitBudget) -> Result<ProjectIdentity> {
    let cwd = crate::util::canonical_plain(cwd)
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;

    if let Some(found) = find_project_file(&cwd, git)? {
        return Ok(found);
    }

    if let Some(root) = git.required(&cwd, &["rev-parse", "--show-toplevel"])? {
        let root = PathBuf::from(root);
        let root = crate::util::canonical_plain(&root).unwrap_or(root);
        let dir_name = basename(&root);
        let remote = git
            .required(&root, &["remote", "get-url", "origin"])?
            .map(|r| normalize_remote(&r));
        let (id, name) = match &remote {
            Some(r) => (id_from_remote(&dir_name, r), name_from_remote(&dir_name, r)),
            None => (id_from_path(&dir_name, &root), dir_name),
        };
        return Ok(ProjectIdentity {
            id,
            name,
            root: root.display().to_string(),
            remote,
        });
    }

    let name = basename(&cwd);
    Ok(ProjectIdentity {
        id: id_from_path(&name, &cwd),
        name,
        root: cwd.display().to_string(),
        remote: None,
    })
}

/// `[a-z0-9-]` slug; ASCII separators become `-`, non-ASCII is dropped; empty → `proj`.
pub fn slug(s: &str) -> String {
    let raw = slug_raw(s);
    if raw.is_empty() {
        "proj".to_string()
    } else {
        raw
    }
}

/// Like [`slug`] but returns an empty string instead of `proj`.
pub fn slug_raw(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            Some(c.to_ascii_lowercase())
        } else if c.is_ascii() {
            Some('-')
        } else {
            None
        };
        if let Some(m) = mapped {
            if m == '-' && (out.is_empty() || out.ends_with('-')) {
                continue;
            }
            out.push(m);
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Normalizes a git remote URL to `host/path`: no scheme, credentials, port, `.git`; host lowercased.
pub fn normalize_remote(url: &str) -> String {
    let url = url.trim();
    let (host_part, path_part) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(authority);
        (host.to_string(), path.to_string())
    } else if let Some((left, path)) = url.split_once(':') {
        // scp-like: [user@]host:path
        let host = left.rsplit_once('@').map(|(_, h)| h).unwrap_or(left);
        (host.to_string(), path.to_string())
    } else {
        return url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_string();
    };
    let host = match host_part.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => host_part,
    }
    .to_lowercase();
    let path = path_part.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

/// Project id derived from a normalized remote.
///
/// The slug comes from the repository name in the remote (not the local directory name) so
/// that clones living in differently named directories still map to the same memory.
pub fn id_from_remote(name: &str, remote: &str) -> String {
    let repo = remote.rsplit('/').next().unwrap_or(name);
    let base = if slug_raw(repo).is_empty() {
        slug(name)
    } else {
        slug(repo)
    };
    format!("{base}-{}", &sha256_hex(remote)[..8])
}

/// Display name matching [`id_from_remote`]: the remote's repository name when the id is
/// built from it, else the local directory name.
pub fn name_from_remote(dir_name: &str, remote: &str) -> String {
    match remote.rsplit('/').next() {
        Some(repo) if !slug_raw(repo).is_empty() => repo.to_string(),
        _ => dir_name.to_string(),
    }
}

/// Project id derived from a canonical root path.
pub fn id_from_path(name: &str, root: &Path) -> String {
    format!(
        "{}-{}",
        slug(name),
        &sha256_hex(&root.display().to_string())[..8]
    )
}

/// True when `id` is safe to use as a directory name (`[a-z0-9-_.]`, no leading dot or `_`).
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        // `_global` (and any `_…` name) is reserved for kioku's own wiki directories.
        && !id.starts_with('_')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// The nearest `.kioku.toml` at or above `start` (no git; cache validation).
pub fn project_file_above(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(PROJECT_FILE))
        .find(|c| c.is_file())
}

fn find_project_file(start: &Path, git: &GitBudget) -> Result<Option<ProjectIdentity>> {
    for dir in start.ancestors() {
        let candidate = dir.join(PROJECT_FILE);
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)
                .with_context(|| format!("reading {}", candidate.display()))?;
            let parsed: ProjectFile = toml::from_str(&text)
                .with_context(|| format!("parsing {}", candidate.display()))?;
            let id = if is_valid_id(&parsed.project) {
                parsed.project.clone()
            } else {
                slug(&parsed.project)
            };
            let name = parsed.name.unwrap_or_else(|| basename(dir));
            return Ok(Some(ProjectIdentity {
                id,
                name,
                root: dir.display().to_string(),
                remote: git
                    .required(dir, &["remote", "get-url", "origin"])?
                    .map(|r| normalize_remote(&r)),
            }));
        }
    }
    Ok(None)
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = crate::util::quiet_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// `git rev-parse --show-toplevel` run in `cwd`; `None` outside a repository or without git.
pub fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    git_output(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// True when `id` has the shape of a path- or remote-derived id: `<slug>-<8 lowercase hex>`.
pub fn is_derived_id(id: &str) -> bool {
    match id.rsplit_once('-') {
        Some((head, hash)) => {
            !head.is_empty()
                && hash.len() == 8
                && hash
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        }
        None => false,
    }
}

/// A root path for comparison: without Windows' `\\?\` prefix and trailing separators.
pub fn comparable_root(root: &str) -> String {
    let plain = crate::util::plain_path(Path::new(root.trim()))
        .display()
        .to_string();
    let trimmed = plain.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        plain
    } else {
        trimmed.to_string()
    }
}

/// Last component of a root path written with `/` or `\` separators (on any platform).
pub fn root_basename(root: &str) -> String {
    comparable_root(root)
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("root")
        .to_string()
}

/// True when `identity.id` is exactly the remote-derived id of its root and remote (it came
/// from git, not from `.kioku.toml`).
pub fn is_remote_derived(identity: &ProjectIdentity) -> bool {
    identity
        .remote
        .as_deref()
        .is_some_and(|r| id_from_remote(&root_basename(&identity.root), r) == identity.id)
}

/// Longest lane name kept verbatim (M2.4 §1.1); longer names get a hash suffix.
pub const MAX_LANE_CHARS: usize = 200;

/// A lane name as stored: trimmed, empty → `None` (the project lane), longer than
/// [`MAX_LANE_CHARS`] → cut and suffixed with `-<8 hex of sha256(name)>`.
pub fn normalize_lane(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    if name.chars().count() <= MAX_LANE_CHARS {
        return Some(name.to_string());
    }
    let head: String = name.chars().take(MAX_LANE_CHARS - 9).collect();
    Some(format!("{head}-{}", &sha256_hex(name)[..8]))
}

/// `git -C <dir> <args>` with a deadline; trimmed stdout on success, `None` otherwise.
fn git_output_within(dir: &Path, args: &[&str], deadline: Duration) -> Option<String> {
    let mut cmd = crate::util::quiet_command("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = crate::util::output_with_deadline(cmd, deadline)?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// The branch checked out in `dir` (`git symbolic-ref --short -q HEAD`); `None` on a
/// detached HEAD, outside a repository or without git.
pub fn current_branch(dir: &Path, deadline: Duration) -> Option<String> {
    git_output_within(dir, &["symbolic-ref", "--short", "-q", "HEAD"], deadline)
}

/// The repository's default branch (M2.4 §1.2): `origin/HEAD`, else `main`, else `master`
/// if such a local branch exists; `None` when unknown.
pub fn default_branch(dir: &Path, deadline: Duration) -> Option<String> {
    if let Some(head) = git_output_within(
        dir,
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
        deadline,
    ) {
        let name = head.strip_prefix("origin/").unwrap_or(&head);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    ["main", "master"].into_iter().find_map(|b| {
        git_output_within(
            dir,
            &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")],
            deadline,
        )
        .map(|_| b.to_string())
    })
}

/// The handoff lane of a session started in `dir` (M2.4 §1.1): the checked-out branch when
/// it is not the default branch; `None` (the project lane) on the default branch, on a
/// detached HEAD, outside git, or when the default branch is unknown. Each git call gets
/// `deadline`.
pub fn lane(dir: &Path, deadline: Duration) -> Option<String> {
    let branch = current_branch(dir, deadline)?;
    let default = default_branch(dir, deadline)?;
    lane_for(&branch, &default)
}

/// The lane of `branch` given the repository's default branch.
pub fn lane_for(branch: &str, default: &str) -> Option<String> {
    if branch == default {
        return None;
    }
    normalize_lane(branch)
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

    const DEADLINE: Duration = Duration::from_secs(10);

    fn git_commit(dir: &Path) -> bool {
        git(
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

    #[test]
    fn lane_follows_the_branch_except_on_the_default_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // outside git: no lane, no default branch
        assert_eq!(lane(&repo, DEADLINE), None);
        assert_eq!(default_branch(&repo, DEADLINE), None);
        if !git(&repo, &["-c", "init.defaultBranch=main", "init", "-q"]) {
            return;
        }
        // unborn main: the default branch is unknown yet → project lane
        assert_eq!(current_branch(&repo, DEADLINE).as_deref(), Some("main"));
        assert_eq!(default_branch(&repo, DEADLINE), None);
        assert_eq!(lane(&repo, DEADLINE), None);
        assert!(git_commit(&repo));
        assert_eq!(default_branch(&repo, DEADLINE).as_deref(), Some("main"));
        assert_eq!(lane(&repo, DEADLINE), None, "default branch = project lane");

        assert!(git(&repo, &["checkout", "-q", "-b", "feature/検索"]));
        assert_eq!(lane(&repo, DEADLINE).as_deref(), Some("feature/検索"));

        // detached HEAD → project lane
        assert!(git(&repo, &["checkout", "-q", "--detach"]));
        assert_eq!(current_branch(&repo, DEADLINE), None);
        assert_eq!(lane(&repo, DEADLINE), None);

        // a second worktree on its own branch has its own lane; the main one has none
        assert!(git(&repo, &["checkout", "-q", "main"]));
        let wt = tmp.path().join("wt");
        assert!(git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task-b",
                wt.to_str().unwrap()
            ]
        ));
        assert_eq!(lane(&wt, DEADLINE).as_deref(), Some("task-b"));
        assert_eq!(lane(&wt.join("."), DEADLINE).as_deref(), Some("task-b"));
        assert_eq!(lane(&repo, DEADLINE), None);

        // origin/HEAD wins over main/master
        assert!(git(
            &repo,
            &["update-ref", "refs/remotes/origin/develop", "HEAD"]
        ));
        assert!(git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/develop"
            ]
        ));
        assert_eq!(default_branch(&repo, DEADLINE).as_deref(), Some("develop"));
        assert_eq!(lane(&repo, DEADLINE).as_deref(), Some("main"));
    }

    #[test]
    fn master_is_the_fallback_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        if !git(
            tmp.path(),
            &["-c", "init.defaultBranch=master", "init", "-q"],
        ) {
            return;
        }
        assert!(git_commit(tmp.path()));
        assert_eq!(
            default_branch(tmp.path(), DEADLINE).as_deref(),
            Some("master")
        );
        assert_eq!(lane(tmp.path(), DEADLINE), None);
        assert!(git(tmp.path(), &["checkout", "-q", "-b", "fix-1"]));
        assert_eq!(lane(tmp.path(), DEADLINE).as_deref(), Some("fix-1"));
        // neither main nor master (and no origin/HEAD) → lanes are never recorded
        assert!(git(tmp.path(), &["branch", "-q", "-m", "master", "trunk"]));
        assert_eq!(default_branch(tmp.path(), DEADLINE), None);
        assert_eq!(lane(tmp.path(), DEADLINE), None);
    }

    #[test]
    fn lane_names_are_trimmed_and_capped() {
        assert_eq!(normalize_lane("  feat/x \n").as_deref(), Some("feat/x"));
        assert_eq!(normalize_lane("   "), None);
        let long = "枝".repeat(300);
        let n = normalize_lane(&long).unwrap();
        assert_eq!(n.chars().count(), MAX_LANE_CHARS);
        assert!(n.starts_with("枝枝"));
        assert_ne!(n, normalize_lane(&format!("{long}x")).unwrap());
        let exact = "a".repeat(MAX_LANE_CHARS);
        assert_eq!(normalize_lane(&exact).unwrap(), exact);
    }

    #[test]
    fn alias_helpers() {
        assert!(is_derived_id("kioku-71002b89"));
        assert!(is_derived_id("ai-agents-shared-memory-02036d30"));
        assert!(!is_derived_id("my-proj"));
        assert!(!is_derived_id("x-71002B89"));
        assert!(!is_derived_id("-71002b89"));
        assert_eq!(comparable_root("/home/u/kioku/"), "/home/u/kioku");
        assert_eq!(comparable_root(r"\\?\C:\src\kioku\"), r"C:\src\kioku");
        assert_eq!(comparable_root("/"), "/");
        assert_eq!(root_basename(r"C:\src\記憶"), "記憶");
        assert_eq!(root_basename("/home/u/kioku/"), "kioku");
        let remote = "github.com/misorafa/kioku";
        let id = ProjectIdentity {
            id: id_from_remote("AI_agents_shared_memory", remote),
            name: "kioku".into(),
            root: "/Users/u/AI_agents_shared_memory".into(),
            remote: Some(remote.into()),
        };
        assert!(is_remote_derived(&id));
        let toml = ProjectIdentity {
            id: "my-proj".into(),
            ..id.clone()
        };
        assert!(!is_remote_derived(&toml));
        let no_remote = ProjectIdentity { remote: None, ..id };
        assert!(!is_remote_derived(&no_remote));
    }

    #[test]
    fn reserved_ids_are_rejected() {
        assert!(is_valid_id("kioku-3f9a1c2e"));
        assert!(is_valid_id("a_b"));
        for id in ["_global", "_x", ".hidden", "", "a/b"] {
            assert!(!is_valid_id(id), "{id}");
        }
        // a .kioku.toml naming a reserved id is slugged instead
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(PROJECT_FILE), "project = \"_global\"\n").unwrap();
        let id = identify(tmp.path()).unwrap().id;
        assert_eq!(id, "global");
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slug("My Project_v2"), "my-project-v2");
        assert_eq!(slug("--a---b--"), "a-b");
        assert_eq!(slug("記憶"), "proj");
        assert_eq!(slug("kioku記憶サーバー"), "kioku");
        assert_eq!(slug("Rust の 設計"), "rust");
        assert_eq!(slug_raw("日本語"), "");
    }

    #[test]
    fn remote_normalization() {
        let want = "github.com/foo/bar";
        assert_eq!(normalize_remote("git@GitHub.com:foo/bar.git"), want);
        assert_eq!(normalize_remote("https://github.com/foo/bar.git"), want);
        assert_eq!(
            normalize_remote("https://user:secret@github.com/foo/bar"),
            want
        );
        assert_eq!(
            normalize_remote("ssh://git@github.com:22/foo/bar.git"),
            want
        );
        assert_eq!(normalize_remote("https://github.com/foo/bar/"), want);
    }

    #[test]
    fn same_remote_same_id_across_clones() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("clone-a");
        let b = tmp.path().join("other-name");
        for (dir, url) in [
            (&a, "git@github.com:acme/widget.git"),
            (&b, "https://token@github.com/acme/widget"),
        ] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            if !git(dir, &["init", "-q"]) {
                eprintln!("git not available; skipping");
                return;
            }
            assert!(git(dir, &["remote", "add", "origin", url]));
        }
        let ia = identify(&a.join("src")).unwrap();
        let ib = identify(&b).unwrap();
        assert_eq!(ia.id, ib.id);
        assert!(ia.id.starts_with("widget-"), "{}", ia.id);
        assert_eq!(ia.remote.as_deref(), Some("github.com/acme/widget"));
        // The name follows the remote (like the id), not the local directory name.
        assert_eq!(ia.name, "widget");
        assert_eq!(ib.name, "widget");
        // stable across calls
        assert_eq!(identify(&a).unwrap(), ia);
    }

    #[test]
    fn name_from_remote_rules() {
        assert_eq!(
            name_from_remote("proj", "github.com/me/chord-life"),
            "chord-life"
        );
        assert_eq!(
            name_from_remote("proj", "github.com/me/Chord_Life"),
            "Chord_Life"
        );
        // No ASCII in the repo name → the id falls back to the directory slug, so does the name.
        assert_eq!(
            name_from_remote("記憶-dir", "github.com/me/記憶"),
            "記憶-dir"
        );
    }

    #[test]
    fn remote_name_differs_from_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        if !git(&dir, &["init", "-q"]) {
            return;
        }
        assert!(git(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:me/chord-life.git"
            ]
        ));
        let id = identify(&dir).unwrap();
        assert!(id.id.starts_with("chord-life-"), "{}", id.id);
        assert_eq!(id.name, "chord-life");

        // `.kioku.toml` `name` still wins.
        std::fs::write(
            dir.join(PROJECT_FILE),
            "project = \"cl\"\nname = \"コード帳\"\n",
        )
        .unwrap();
        let id = identify(&dir).unwrap();
        assert_eq!((id.id.as_str(), id.name.as_str()), ("cl", "コード帳"));
    }

    #[test]
    fn path_fallback_is_stable_and_path_dependent() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("記憶 notes");
        let b = tmp.path().join("x").join("記憶 notes");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let ia = identify(&a).unwrap();
        assert_eq!(ia, identify(&a).unwrap());
        assert!(ia.id.starts_with("notes-"), "{}", ia.id);
        assert_eq!(ia.remote, None);
        assert_eq!(ia.name, "記憶 notes");
        assert_ne!(ia.id, identify(&b).unwrap().id);
    }

    #[test]
    fn git_without_remote_uses_toplevel_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(root.join("deep/er")).unwrap();
        if !git(&root, &["init", "-q"]) {
            return;
        }
        let top = identify(&root).unwrap();
        let deep = identify(&root.join("deep/er")).unwrap();
        assert_eq!(top, deep);
        assert_eq!(top.name, "repo");
        assert_eq!(top.remote, None);
    }

    #[test]
    fn project_file_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(PROJECT_FILE),
            "project = \"my-proj\"\nname = \"マイプロジェクト\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let id = identify(&tmp.path().join("a/b")).unwrap();
        assert_eq!(id.id, "my-proj");
        assert_eq!(id.name, "マイプロジェクト");
        assert_eq!(
            id.root,
            crate::util::canonical_plain(tmp.path())
                .unwrap()
                .display()
                .to_string()
        );
    }
}
