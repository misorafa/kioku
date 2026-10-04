//! Wiki pages (spec §6.1): YAML frontmatter that preserves unknown keys, and page path rules.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

use crate::error::{Error, Result};
use crate::project::slug_raw;
use crate::util::sha256_hex;

/// Directory (inside `wiki/`) holding global pages.
pub const GLOBAL_DIR: &str = "_global";

/// Visibility of a page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageScope {
    /// Belongs to one project.
    #[default]
    Project,
    /// Personal, cross-project page under `_global/`.
    Global,
}

impl PageScope {
    /// Lowercase name as stored in frontmatter / SQLite / tantivy.
    pub fn as_str(self) -> &'static str {
        match self {
            PageScope::Project => "project",
            PageScope::Global => "global",
        }
    }

    /// Parses `project` / `global`.
    pub fn parse(s: &str) -> Option<PageScope> {
        match s.trim().to_ascii_lowercase().as_str() {
            "project" => Some(PageScope::Project),
            "global" => Some(PageScope::Global),
            _ => None,
        }
    }
}

/// What produced a page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageKind {
    /// Rule-generated session summary.
    Session,
    /// Free page written via `kioku_write_page`.
    #[default]
    Page,
    /// The project's STATE.md.
    State,
}

impl PageKind {
    /// Lowercase name as stored in frontmatter / SQLite / tantivy.
    pub fn as_str(self) -> &'static str {
        match self {
            PageKind::Session => "session",
            PageKind::Page => "page",
            PageKind::State => "state",
        }
    }

    /// Parses `session` / `page` / `state`.
    pub fn parse(s: &str) -> Option<PageKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "session" => Some(PageKind::Session),
            "page" => Some(PageKind::Page),
            "state" => Some(PageKind::State),
            _ => None,
        }
    }
}

/// Parsed frontmatter; keys kioku does not know are kept in `extra` and written back.
/// `Deserialize` reads the API's JSON form (the `kioku mcp` bridge, M2 §20).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Frontmatter {
    /// Page title.
    pub title: String,
    /// Owning project id (absent for global pages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// `project` | `global`.
    pub scope: PageScope,
    /// `session` | `page` | `state`.
    pub kind: PageKind,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// RFC 3339 creation time.
    pub created: String,
    /// RFC 3339 last update time.
    pub updated: String,
    /// Agent session id (session pages only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Agent name (session pages only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Handoff lane of the session (session pages only, when not the project lane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Machine that ran the session (session pages only, when known; SPEC-M3.0 §6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Unknown keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

const KNOWN_KEYS: [&str; 11] = [
    "title", "project", "scope", "kind", "tags", "created", "updated", "session", "agent", "lane",
    "machine",
];

impl Frontmatter {
    /// Parses YAML frontmatter text leniently (numbers/bools in string fields become strings).
    pub fn from_yaml(text: &str) -> Result<Frontmatter> {
        let value: Value = if text.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            serde_yaml_ng::from_str(text).context("parsing frontmatter YAML")?
        };
        let Value::Mapping(map) = value else {
            return Err(Error::invalid("frontmatter is not a YAML mapping"));
        };
        let mut fm = Frontmatter::default();
        for (k, v) in map {
            let Some(key) = scalar_string(&k) else {
                continue;
            };
            match key.as_str() {
                "title" => fm.title = scalar_string(&v).unwrap_or_default(),
                "project" => fm.project = scalar_string(&v),
                "scope" => {
                    fm.scope = scalar_string(&v)
                        .and_then(|s| PageScope::parse(&s))
                        .unwrap_or_default()
                }
                "kind" => {
                    fm.kind = scalar_string(&v)
                        .and_then(|s| PageKind::parse(&s))
                        .unwrap_or_default()
                }
                "tags" => fm.tags = tags_from_value(&v),
                "created" => fm.created = scalar_string(&v).unwrap_or_default(),
                "updated" => fm.updated = scalar_string(&v).unwrap_or_default(),
                "session" => fm.session = scalar_string(&v),
                "agent" => fm.agent = scalar_string(&v),
                "lane" => fm.lane = scalar_string(&v),
                "machine" => fm.machine = scalar_string(&v),
                _ => {
                    fm.extra.insert(key, v);
                }
            }
        }
        Ok(fm)
    }

    /// Serializes to YAML (known keys in a fixed order, then extras).
    pub fn to_yaml(&self) -> Result<String> {
        let mut map = Mapping::new();
        let mut put = |k: &str, v: Value| {
            map.insert(Value::String(k.to_string()), v);
        };
        put("title", Value::String(self.title.clone()));
        if let Some(p) = &self.project {
            put("project", Value::String(p.clone()));
        }
        put("scope", Value::String(self.scope.as_str().to_string()));
        put("kind", Value::String(self.kind.as_str().to_string()));
        put(
            "tags",
            Value::Sequence(self.tags.iter().cloned().map(Value::String).collect()),
        );
        put("created", Value::String(self.created.clone()));
        put("updated", Value::String(self.updated.clone()));
        if let Some(s) = &self.session {
            put("session", Value::String(s.clone()));
        }
        if let Some(a) = &self.agent {
            put("agent", Value::String(a.clone()));
        }
        if let Some(l) = &self.lane {
            put("lane", Value::String(l.clone()));
        }
        if let Some(m) = &self.machine {
            put("machine", Value::String(m.clone()));
        }
        for (k, v) in &self.extra {
            if !KNOWN_KEYS.contains(&k.as_str()) {
                put(k, v.clone());
            }
        }
        Ok(serde_yaml_ng::to_string(&Value::Mapping(map)).context("serializing frontmatter")?)
    }
}

/// A page as stored in `wiki/`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// SHA-256 of the complete file, for conditional writes.
    #[serde(default)]
    pub revision: String,
    /// Path relative to `wiki/`, with `/` separators.
    pub path: String,
    /// Parsed frontmatter.
    pub frontmatter: Frontmatter,
    /// Markdown body (without frontmatter).
    pub body: String,
}

impl Page {
    /// Parses a full Markdown document; files without frontmatter get defaults derived from `path`.
    pub fn parse(path: &str, text: &str) -> Result<Page> {
        let (yaml, body) = split_frontmatter(text);
        let frontmatter = match yaml {
            Some(y) => Frontmatter::from_yaml(&y)?,
            None => infer_frontmatter(path, &body),
        };
        Ok(Page {
            revision: crate::util::sha256_hex(text),
            path: path.to_string(),
            frontmatter,
            body,
        })
    }

    /// Renders the page back to `---\n<yaml>---\n<body>`.
    pub fn render(&self) -> Result<String> {
        let mut out = String::from("---\n");
        out.push_str(&self.frontmatter.to_yaml()?);
        out.push_str("---\n");
        out.push_str(&self.body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

/// Splits `---\n…\n---\n` frontmatter from the body; `None` when the text has none.
pub fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text.to_string());
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let yaml = rest[..offset].to_string();
            let body = rest[offset + line.len()..].to_string();
            return (Some(yaml), body);
        }
        offset += line.len();
    }
    (None, text.to_string())
}

fn infer_frontmatter(path: &str, body: &str) -> Frontmatter {
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or_default();
    let global = first == GLOBAL_DIR;
    let file = path.rsplit('/').next().unwrap_or(path);
    let kind = if file == "STATE.md" {
        PageKind::State
    } else if path.split('/').nth(1) == Some("sessions") {
        PageKind::Session
    } else {
        PageKind::Page
    };
    let title = body
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| file.trim_end_matches(".md").to_string());
    Frontmatter {
        title,
        project: if global {
            None
        } else {
            Some(first.to_string())
        },
        scope: if global {
            PageScope::Global
        } else {
            PageScope::Project
        },
        kind,
        ..Frontmatter::default()
    }
}

fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Tagged(t) => scalar_string(&t.value),
        _ => None,
    }
}

fn tags_from_value(v: &Value) -> Vec<String> {
    match v {
        Value::Sequence(items) => items.iter().filter_map(scalar_string).collect(),
        Value::String(s) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// File-name slug for a page title: ASCII slug, plus a short hash whenever slugging dropped
/// characters (anything but ASCII alphanumerics joined by single ` `, `-` or `_`).
///
/// Titles would otherwise collapse to the same (or an empty) slug and overwrite each other:
/// `設計メモ` → `page-1a2b3c`, `Rust の設計` → `rust-9f8e7d`, `C++ tips` → `c-tips-4d5e6f`
/// (while `C tips` stays `c-tips`). Since SPEC-M3.4 §2 this is the name of ASCII titles and
/// the name pages written before M3.4 keep; new non-ASCII titles use [`title_slug`].
pub fn page_slug(title: &str) -> String {
    let base = slug_raw(title);
    let hash = &sha256_hex(title.trim())[..6];
    if base.is_empty() {
        format!("page-{hash}")
    } else if slug_is_lossless(title.trim()) {
        base
    } else {
        format!("{base}-{hash}")
    }
}

/// True when `title` is ASCII alphanumeric words joined by single ` `, `-` or `_`, i.e. the
/// slug keeps every character that tells two titles apart (apart from case).
fn slug_is_lossless(title: &str) -> bool {
    let mut prev_sep = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            prev_sep = false;
        } else if matches!(c, ' ' | '-' | '_') && !prev_sep {
            prev_sep = true;
        } else {
            return false;
        }
    }
    !prev_sep
}

/// Validates a wiki-relative path: relative, no `..`/`.`/empty components, no backslashes.
pub fn validate_rel_path(path: &str) -> Result<String> {
    let p = path.trim();
    if p.is_empty() {
        return Err(Error::invalid("empty path"));
    }
    if p.starts_with('/') || p.contains('\\') || p.contains(':') {
        return Err(Error::invalid(format!("path must be relative: {p}")));
    }
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    if parts.iter().any(|c| *c == ".." || *c == ".") {
        return Err(Error::invalid(format!("path must not contain '..': {p}")));
    }
    if parts.iter().any(|c| c.starts_with('.')) {
        return Err(Error::invalid(format!(
            "hidden path components are not allowed: {p}"
        )));
    }
    Ok(parts.join("/"))
}

/// Longest `slug` accepted by `kioku_write_page` (SPEC-M3.4 §2).
pub const SLUG_MAX: usize = 64;

/// A title whose ASCII part is shorter than this gets a date-prefixed name (SPEC-M3.4 §2).
const READABLE_MIN: usize = 3;

/// The 6-hex hash of a title that keeps two titles apart in their file names.
pub fn title_hash(title: &str) -> String {
    sha256_hex(title.trim())[..6].to_string()
}

/// Validates an agent-given `slug` (SPEC-M3.4 §2): ASCII letters/digits words joined by
/// single `-`, 1–[`SLUG_MAX`] characters; uppercase is folded. Returns the lowercased slug.
pub fn validate_slug(raw: &str) -> Result<String> {
    let slug = raw.trim().to_ascii_lowercase();
    let well_formed = !slug.is_empty()
        && slug.len() <= SLUG_MAX
        && slug
            .split('-')
            .all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric()));
    if !well_formed {
        return Err(Error::invalid(format!(
            "slug must be ASCII letters/digits words joined by single '-', 1-{SLUG_MAX} characters (e.g. write-test-2) / slug は英数字の単語を 1 つの - でつないだ 1〜{SLUG_MAX} 文字: {raw:?}"
        )));
    }
    Ok(slug)
}

/// File name (without `.md`) of a new page written by title alone (SPEC-M3.4 §2): a title
/// [`page_slug`] keeps whole (ASCII words) keeps that slug; a title whose ASCII part after
/// NFKC is shorter than 3 characters becomes `<date>-<hash>` (`date` = `YYYY-MM-DD`, so a
/// listing is at least chronological); anything else is the NFKC-folded ASCII part plus the
/// hash.
pub fn title_slug(title: &str, date: &str) -> String {
    let trimmed = title.trim();
    if slug_is_lossless(trimmed) {
        return page_slug(title);
    }
    let base = slug_raw(&crate::index::nfkc(trimmed));
    let hash = title_hash(title);
    if base.len() < READABLE_MIN {
        format!("{date}-{hash}")
    } else {
        format!("{base}-{hash}")
    }
}

/// True when `name` is a file name [`title_slug`] gives a title with this `hash` on some
/// day: `YYYY-MM-DD-<hash>.md`.
pub fn is_dated_name_for(name: &str, hash: &str) -> bool {
    let Some(date) = name
        .strip_suffix(".md")
        .and_then(|s| s.strip_suffix(hash))
        .and_then(|s| s.strip_suffix('-'))
    else {
        return false;
    };
    date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
}

/// Directory (wiki-relative) of a scope's pages: `_global` or `<project>/pages`.
pub fn scope_root(scope: PageScope, project: Option<&str>) -> Result<String> {
    Ok(match scope {
        PageScope::Global => GLOBAL_DIR.to_string(),
        PageScope::Project => {
            let p = project.ok_or_else(|| Error::invalid("scope=project requires a project"))?;
            format!("{p}/pages")
        }
    })
}

/// Resolves where `kioku_write_page` writes (spec §6.1 path rules, SPEC-M3.4 §2): the
/// explicit `path`, else `<scope dir>/<slug>.md`, else the name [`title_slug`] derives
/// (`date` = today, `YYYY-MM-DD`). `slug` and `path` together are an error.
///
/// Explicit project-scope paths are confined to `<project>/pages/` so callers cannot
/// overwrite STATE.md or session pages.
pub fn resolve_write_path(
    title: &str,
    scope: PageScope,
    project: Option<&str>,
    explicit: Option<&str>,
    slug: Option<&str>,
    date: &str,
) -> Result<String> {
    let root = scope_root(scope, project)?;
    if let Some(raw) = slug {
        if explicit.is_some() {
            return Err(Error::invalid(
                "give either slug or path, not both / slug と path は同時に指定できません",
            ));
        }
        return validate_rel_path(&format!("{root}/{}.md", validate_slug(raw)?));
    }
    let path = match explicit {
        None => format!("{root}/{}.md", title_slug(title, date)),
        Some(raw) => {
            let rel = validate_rel_path(raw)?;
            let scope_prefix = match scope {
                PageScope::Global => format!("{GLOBAL_DIR}/"),
                PageScope::Project => format!("{}/", project.unwrap_or_default()),
            };
            let full = if rel.starts_with(&format!("{root}/")) {
                rel
            } else if rel.starts_with(&scope_prefix) {
                return Err(Error::invalid(format!(
                    "path must be inside {root}/: {rel}"
                )));
            } else {
                format!("{root}/{rel}")
            };
            if full.ends_with(".md") {
                full
            } else {
                format!("{full}.md")
            }
        }
    };
    validate_rel_path(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ntitle: セッション 2026-09-25 引き継ぎサーバーの設計\nproject: kioku-3f9a1c2e\nscope: project\nkind: session\ntags: [rust, mcp]\ncreated: 2026-09-25T02:14:00Z\nupdated: 2026-09-25T03:40:11Z\nsession: 0c2f\nagent: claude-code\ncustom_key: 42\nnested:\n  a: [1, 2]\n  b: テキスト\n---\n## 本文\n\n内容です。\n";

    #[test]
    fn frontmatter_roundtrip_preserves_unknown_keys() {
        let page = Page::parse("kioku-3f9a1c2e/sessions/x.md", SAMPLE).unwrap();
        let fm = &page.frontmatter;
        assert_eq!(fm.title, "セッション 2026-09-25 引き継ぎサーバーの設計");
        assert_eq!(fm.project.as_deref(), Some("kioku-3f9a1c2e"));
        assert_eq!(fm.scope, PageScope::Project);
        assert_eq!(fm.kind, PageKind::Session);
        assert_eq!(fm.tags, vec!["rust", "mcp"]);
        assert_eq!(fm.created, "2026-09-25T02:14:00Z");
        assert_eq!(fm.session.as_deref(), Some("0c2f"));
        assert_eq!(fm.extra.len(), 2);
        assert!(fm.extra.contains_key("custom_key"));
        assert_eq!(page.body, "## 本文\n\n内容です。\n");

        let rendered = page.render().unwrap();
        let again = Page::parse(&page.path, &rendered).unwrap();
        assert_eq!(again.frontmatter, page.frontmatter);
        assert_eq!(again.body, page.body);
        assert_eq!(again.path, page.path);
        assert_eq!(again.revision, crate::util::sha256_hex(&rendered));
        assert!(rendered.contains("custom_key: 42"));
        assert!(rendered.contains("b: テキスト"));
        // rendering is a fixed point
        assert_eq!(again.render().unwrap(), rendered);
    }

    #[test]
    fn numeric_looking_values_stay_strings() {
        let page = Page::parse(
            "_global/x.md",
            "---\ntitle: 2026\nsession: 12345678\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(page.frontmatter.title, "2026");
        assert_eq!(page.frontmatter.session.as_deref(), Some("12345678"));
        let again = Page::parse("_global/x.md", &page.render().unwrap()).unwrap();
        assert_eq!(again.frontmatter.title, "2026");
    }

    #[test]
    fn missing_frontmatter_is_inferred() {
        let p = Page::parse("_global/notes.md", "# 私のメモ\n本文").unwrap();
        assert_eq!(p.frontmatter.title, "私のメモ");
        assert_eq!(p.frontmatter.scope, PageScope::Global);
        assert_eq!(p.frontmatter.project, None);
        let s = Page::parse("proj-1/STATE.md", "x").unwrap();
        assert_eq!(s.frontmatter.kind, PageKind::State);
        assert_eq!(s.frontmatter.project.as_deref(), Some("proj-1"));
        let s = Page::parse("proj-1/sessions/2026-01-01-abc.md", "x").unwrap();
        assert_eq!(s.frontmatter.kind, PageKind::Session);
    }

    #[test]
    fn page_slugs_do_not_collide_for_japanese_titles() {
        assert_eq!(page_slug("Design Notes"), "design-notes");
        let a = page_slug("設計メモ");
        let b = page_slug("運用メモ");
        assert!(a.starts_with("page-"));
        assert_ne!(a, b);
        assert!(page_slug("Rust の設計").starts_with("rust-"));
    }

    #[test]
    fn page_slugs_do_not_collide_when_characters_are_dropped() {
        assert_eq!(page_slug("C tips"), "c-tips");
        let cpp = page_slug("C++ tips");
        assert!(
            cpp.starts_with("c-tips-") && cpp.len() == "c-tips-".len() + 6,
            "{cpp}"
        );
        assert_ne!(page_slug("C# tips"), cpp);
        assert_ne!(page_slug("v1.2 notes"), page_slug("v12 notes"));
        assert_eq!(page_slug("v12 notes"), "v12-notes");
        for plain in ["Design Notes", "design-notes", "snake_case_name", "Tips"] {
            assert!(!page_slug(plain).contains(char::is_uppercase));
            assert_eq!(page_slug(plain), slug_raw(plain), "{plain}");
        }
        assert_ne!(
            page_slug("a  b"),
            "a-b",
            "double separator is dropped information"
        );
        // 日本語タイトルも従来どおりハッシュ付き
        assert!(page_slug("設計メモ").starts_with("page-"));
    }

    #[test]
    fn write_path_rules() {
        let p = resolve_write_path("Design Notes", PageScope::Global, None, None, None, D).unwrap();
        assert_eq!(p, "_global/design-notes.md");
        let p =
            resolve_write_path("Design", PageScope::Project, Some("k-1"), None, None, D).unwrap();
        assert_eq!(p, "k-1/pages/design.md");
        let p = resolve_write_path(
            "x",
            PageScope::Project,
            Some("k-1"),
            Some("arch/db"),
            None,
            D,
        )
        .unwrap();
        assert_eq!(p, "k-1/pages/arch/db.md");
        let p = resolve_write_path(
            "x",
            PageScope::Project,
            Some("k-1"),
            Some("k-1/pages/a.md"),
            None,
            D,
        )
        .unwrap();
        assert_eq!(p, "k-1/pages/a.md");
        assert!(
            resolve_write_path(
                "x",
                PageScope::Project,
                Some("k-1"),
                Some("k-1/STATE.md"),
                None,
                D
            )
            .is_err()
        );
        assert!(
            resolve_write_path(
                "x",
                PageScope::Project,
                Some("k-1"),
                Some("../a.md"),
                None,
                D
            )
            .is_err()
        );
        assert!(
            resolve_write_path(
                "x",
                PageScope::Project,
                Some("k-1"),
                Some("/etc/x.md"),
                None,
                D
            )
            .is_err()
        );
        assert!(
            resolve_write_path("x", PageScope::Global, None, Some("a/../../b"), None, D).is_err()
        );
        assert!(resolve_write_path("x", PageScope::Project, None, None, None, D).is_err());
        let p = resolve_write_path("x", PageScope::Global, None, Some("tips/rust.md"), None, D)
            .unwrap();
        assert_eq!(p, "_global/tips/rust.md");
    }

    const D: &str = "2026-10-04";

    /// SPEC-M3.4 §2: valid, uppercase folded, invalid characters, too long, with `path`.
    #[test]
    fn slug_validation() {
        assert_eq!(validate_slug("write-test-2").unwrap(), "write-test-2");
        assert_eq!(validate_slug("Write-Test-2").unwrap(), "write-test-2");
        assert_eq!(validate_slug(" x ").unwrap(), "x");
        let max = "a".repeat(SLUG_MAX);
        assert_eq!(validate_slug(&max).unwrap(), max);
        for bad in [
            "",
            "-a",
            "a-",
            "a--b",
            "a_b",
            "a b",
            "a/b",
            "../a",
            "a.md",
            "書き込み",
            "ｗｒｉｔｅ",
            &"a".repeat(SLUG_MAX + 1),
        ] {
            let err = validate_slug(bad).unwrap_err();
            assert!(err.to_string().contains("slug must be"), "{bad}: {err}");
        }
        let p = resolve_write_path(
            "書き込みテスト",
            PageScope::Project,
            Some("k-1"),
            None,
            Some("Write-Test-2"),
            D,
        )
        .unwrap();
        assert_eq!(p, "k-1/pages/write-test-2.md");
        let p = resolve_write_path("メモ", PageScope::Global, None, None, Some("memo"), D).unwrap();
        assert_eq!(p, "_global/memo.md");
        let both = resolve_write_path(
            "メモ",
            PageScope::Project,
            Some("k-1"),
            Some("a.md"),
            Some("a"),
            D,
        )
        .unwrap_err();
        assert!(both.to_string().contains("not both"), "{both}");
        assert!(resolve_write_path("メモ", PageScope::Global, None, None, Some("a_b"), D).is_err());
    }

    /// SPEC-M3.4 §2: a Japanese-only title is date-prefixed, an ASCII title keeps its slug,
    /// a readable ASCII part (also after NFKC) is kept with the hash.
    #[test]
    fn title_slugs_for_new_pages() {
        let hash = title_hash("書き込みテスト");
        assert_eq!(
            title_slug("書き込みテスト", D),
            format!("2026-10-04-{hash}")
        );
        assert_ne!(title_slug("設計メモ", D), title_slug("運用メモ", D));
        // 1–2 ASCII characters are not readable either
        let t = "書き込みテスト2（メモ）";
        assert_eq!(title_slug(t, D), format!("2026-10-04-{}", title_hash(t)));
        // ASCII titles keep the old slug, even short ones
        for ascii in [
            "Design Notes",
            "Go",
            "x",
            "C tips",
            "C++ tips",
            "v1.2 notes",
        ] {
            assert_eq!(title_slug(ascii, D), page_slug(ascii), "{ascii}");
        }
        // a readable ASCII part stays readable
        let t = "書き込みテスト2(claude.ai から)";
        assert_eq!(title_slug(t, D), format!("2-claude-ai-{}", title_hash(t)));
        // full-width ASCII is folded by NFKC
        let t = "ＡＰＩの設計";
        assert_eq!(title_slug(t, D), format!("api-{}", title_hash(t)));
        // dated names are recognised for the same title only
        let name = format!("{}.md", title_slug("書き込みテスト", D));
        assert!(is_dated_name_for(&name, &hash));
        assert!(is_dated_name_for(&format!("2025-01-31-{hash}.md"), &hash));
        assert!(!is_dated_name_for(&name, &title_hash("設計メモ")));
        assert!(!is_dated_name_for(&format!("page-{hash}.md"), &hash));
        assert!(!is_dated_name_for(&format!("2025-13-31-{hash}.md"), &hash));
        assert!(!is_dated_name_for(&format!("2025-01-31-{hash}"), &hash));
    }
}
