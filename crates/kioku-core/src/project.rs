//! Project identity (spec §4): `.kioku.toml`, git remote, or path → stable project id.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Human name (basename of the project root unless overridden).
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

/// Identifies the project that `cwd` belongs to (spec §4 priority order).
pub fn identify(cwd: &Path) -> Result<ProjectIdentity> {
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;

    if let Some(found) = find_project_file(&cwd)? {
        return Ok(found);
    }

    if let Some(root) = git_toplevel(&cwd) {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let name = basename(&root);
        let remote = git_remote(&root).map(|r| normalize_remote(&r));
        let id = match &remote {
            Some(r) => id_from_remote(&name, r),
            None => id_from_path(&name, &root),
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

/// Project id derived from a canonical root path.
pub fn id_from_path(name: &str, root: &Path) -> String {
    format!(
        "{}-{}",
        slug(name),
        &sha256_hex(&root.display().to_string())[..8]
    )
}

/// True when `id` is safe to use as a directory name (`[a-z0-9-_.]`, no leading dot).
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn find_project_file(start: &Path) -> Result<Option<ProjectIdentity>> {
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
                remote: git_remote(dir).map(|r| normalize_remote(&r)),
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
    let out = Command::new("git")
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

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    git_output(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn git_remote(root: &Path) -> Option<String> {
    git_output(root, &["remote", "get-url", "origin"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
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
        assert_eq!(ia.name, "clone-a");
        assert_eq!(ib.name, "other-name");
        // stable across calls
        assert_eq!(identify(&a).unwrap(), ia);
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
            std::fs::canonicalize(tmp.path())
                .unwrap()
                .display()
                .to_string()
        );
    }
}
