//! Delimited, kioku-managed text blocks (M2 §4.6, §7): the Codex `config.toml` MCP block and
//! the instruction snippet in AGENTS.md / GEMINI.md / CLAUDE.md. Only the bytes between the
//! markers are ever rewritten; everything outside a block is left exactly as it was.
//!
//! No format-preserving TOML editor is used (no `toml_edit` dependency): the file is parsed
//! with `toml` only to validate it before and after the edit.

use std::path::Path;

use anyhow::Context;
use kioku_core::Lang;
use kioku_core::strings::{fill, strings};

use super::{backup_path, rewrite_text, write_text};

/// Begin / end marker lines of one kind of managed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Markers {
    /// The full begin line we write.
    pub begin: &'static str,
    /// Prefix that identifies a begin line (so older versions of the line still match).
    pub begin_prefix: &'static str,
    /// The end line.
    pub end: &'static str,
}

/// Markers of the Codex `config.toml` block (M2 §4.5).
pub const TOML_MARKERS: Markers = Markers {
    begin: "# >>> kioku (managed by `kioku install codex`; edits inside this block are overwritten) >>>",
    begin_prefix: "# >>> kioku",
    end: "# <<< kioku <<<",
};

/// Markers of the Markdown instruction block (M2 §7).
pub const MD_MARKERS: Markers = Markers {
    begin: "<!-- kioku:begin v1 (managed by `kioku install`; edits inside are overwritten) -->",
    begin_prefix: "<!-- kioku:begin",
    end: "<!-- kioku:end -->",
};

/// Byte ranges `[start, end)` of every managed block in `text`, each from the start of its
/// begin line to the end of its end line (newline included). A begin line without an end
/// line is an error: the file is then not touched.
pub fn block_ranges(text: &str, m: Markers) -> anyhow::Result<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut open: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        match open {
            None if bare.trim_start().starts_with(m.begin_prefix) => open = Some(offset),
            Some(start) if bare.trim() == m.end => {
                out.push((start, offset + line.len()));
                open = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    if open.is_some() {
        anyhow::bail!("a kioku block has a begin marker but no `{}` line", m.end);
    }
    Ok(out)
}

/// Wraps `body` in the markers; the result ends with a newline.
pub fn wrap(body: &str, m: Markers) -> String {
    let body = body.trim_end_matches('\n');
    format!("{}\n{body}\n{}\n", m.begin, m.end)
}

/// Start of the removal range for a block at `start`: one blank line before it goes too.
fn with_blank_line_before(text: &str, start: usize) -> usize {
    if text[..start].ends_with("\n\n") {
        start - 1
    } else if text[..start].ends_with("\r\n\r\n") {
        start - 2
    } else {
        start
    }
}

/// `text` with the managed block set to `block` (a [`wrap`]ped block): the first existing
/// block is replaced in place and any further ones removed; with none, `block` is appended
/// after one blank line.
pub fn upsert_block(text: &str, block: &str, m: Markers) -> anyhow::Result<String> {
    let ranges = block_ranges(text, m)?;
    let Some(&(first_start, first_end)) = ranges.first() else {
        if text.is_empty() {
            return Ok(block.to_string());
        }
        let mut out = text.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(block);
        return Ok(out);
    };
    let mut out = String::with_capacity(text.len() + block.len());
    out.push_str(&text[..first_start]);
    out.push_str(block);
    let mut pos = first_end;
    for &(start, end) in &ranges[1..] {
        let cut = with_blank_line_before(text, start).max(pos);
        out.push_str(&text[pos..cut]);
        pos = end;
    }
    out.push_str(&text[pos..]);
    Ok(out)
}

/// `text` without any managed block (each with one blank line before it) and whether one
/// was found.
pub fn remove_blocks(text: &str, m: Markers) -> anyhow::Result<(String, bool)> {
    let ranges = block_ranges(text, m)?;
    if ranges.is_empty() {
        return Ok((text.to_string(), false));
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for &(start, end) in &ranges {
        let cut = with_blank_line_before(text, start).max(pos);
        out.push_str(&text[pos..cut]);
        pos = end;
    }
    out.push_str(&text[pos..]);
    Ok((out, true))
}

/// What happened to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOutcome {
    /// Nothing to do.
    Unchanged,
    /// Created or rewritten (dry run: would be).
    Written,
    /// Removed because kioku created it and nothing else is left (dry run: would be).
    Deleted,
}

/// Reads a text file; `None` when it does not exist.
pub fn read_text(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `after` when it differs from `before` (backup once, mode kept).
pub fn save_text(
    path: &Path,
    before: Option<&str>,
    after: &str,
    secret: bool,
    dry_run: bool,
) -> anyhow::Result<FileOutcome> {
    if before == Some(after) {
        return Ok(FileOutcome::Unchanged);
    }
    if !dry_run {
        write_text(path, before.is_some(), after, secret)?;
    }
    Ok(FileOutcome::Written)
}

/// Writes `after` after a removal: a file left blank that kioku created (no `.kioku-bak`,
/// which is written before the first change of a pre-existing file) is deleted instead.
pub fn save_after_removal(path: &Path, after: &str, dry_run: bool) -> anyhow::Result<FileOutcome> {
    if after.trim().is_empty() && !backup_path(&super::resolve_target(path)).exists() {
        if !dry_run {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
        return Ok(FileOutcome::Deleted);
    }
    if !dry_run {
        rewrite_text(path, after)?;
    }
    Ok(FileOutcome::Written)
}

/// Body of the instruction snippet (M2 §7) for `lang`; `project_id` fills the project line
/// of a project-level snippet, `None` gives the global wording.
pub fn instructions_body(lang: Lang, project_id: Option<&str>) -> String {
    let t = strings(lang);
    let project_line = match project_id {
        Some(id) => format!("`{id}`"),
        None => t.instructions_global_project.to_string(),
    };
    fill(t.instructions_body, &[("project_line", &project_line)])
}

/// Inserts or refreshes the instruction block in the Markdown file at `path`.
pub fn install_md_block(path: &Path, body: &str, dry_run: bool) -> anyhow::Result<FileOutcome> {
    let before = read_text(path)?;
    let after = upsert_block(
        before.as_deref().unwrap_or(""),
        &wrap(body, MD_MARKERS),
        MD_MARKERS,
    )
    .with_context(|| format!("{}: not touching it", path.display()))?;
    save_text(path, before.as_deref(), &after, false, dry_run)
}

/// Removes the instruction block from the Markdown file at `path` (if any).
pub fn uninstall_md_block(path: &Path, dry_run: bool) -> anyhow::Result<FileOutcome> {
    let Some(before) = read_text(path)? else {
        return Ok(FileOutcome::Unchanged);
    };
    let (after, found) = remove_blocks(&before, MD_MARKERS)
        .with_context(|| format!("{}: not touching it", path.display()))?;
    if !found {
        return Ok(FileOutcome::Unchanged);
    }
    save_after_removal(path, &after, dry_run)
}

/// Frontmatter `description` of kioku's Cursor rule (M2 §5.7).
pub const MDC_DESCRIPTION: &str =
    "kioku shared memory — read the handoff at start, write one before finishing";

/// Full content of `.cursor/rules/kioku.mdc` (owned entirely by kioku).
pub fn mdc_content(body: &str) -> String {
    format!(
        "---\ndescription: {MDC_DESCRIPTION}\nalwaysApply: true\n---\n{}\n",
        body.trim_end_matches('\n')
    )
}

/// True when `text` is a rule file kioku wrote.
pub fn is_our_mdc(text: &str) -> bool {
    text.starts_with(&format!("---\ndescription: {MDC_DESCRIPTION}\n"))
}

// ---------------------------------------------------------------------------------------
// Codex config.toml
// ---------------------------------------------------------------------------------------

/// TOML basic string literal for `s`.
pub fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// TOML string for a path: a literal string (`'C:\…\kioku.exe'`) when it holds a backslash
/// and can be one (SPEC-M2.2 §7.2: Windows paths stay readable), else [`toml_string`].
pub fn toml_path_string(s: &str) -> String {
    if s.contains('\\') && !s.contains('\'') && !s.chars().any(char::is_control) {
        format!("'{s}'")
    } else {
        toml_string(s)
    }
}

/// How Codex reaches kioku's MCP tools (M2 §20.2). Owned strings (CLAUDE.md rule 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexMcp {
    /// `command = "<bin>"`, `args = ["mcp"]`: the `kioku mcp` stdio bridge (default).
    Stdio(String),
    /// `url` and, with a token, bearer `http_headers` (v0.3 form, `--mcp-http`).
    Http(String, Option<String>),
}

impl CodexMcp {
    /// The stdio form for binary `bin`.
    pub fn stdio(bin: &str) -> CodexMcp {
        CodexMcp::Stdio(bin.to_string())
    }

    /// The HTTP form for `url`, with a bearer `token` when given.
    pub fn http(url: &str, token: Option<&str>) -> CodexMcp {
        CodexMcp::Http(url.to_string(), token.map(str::to_string))
    }

    /// True when the parsed config's `mcp_servers.kioku` is exactly this form.
    fn matches(&self, t: &toml::Table) -> bool {
        let get = |k: &str| table_get(t, &["mcp_servers", "kioku", k]);
        match self {
            CodexMcp::Stdio(bin) => {
                get("command").and_then(toml::Value::as_str) == Some(bin.as_str())
                    && get("args")
                        .and_then(toml::Value::as_array)
                        .is_some_and(|a| a.len() == 1 && a[0].as_str() == Some("mcp"))
            }
            CodexMcp::Http(url, _) => {
                get("url").and_then(toml::Value::as_str) == Some(url.as_str())
            }
        }
    }
}

/// The Codex managed block: `[mcp_servers.kioku]` in the given form, plus
/// `[features] hooks = true` when `enable_hooks` (M2 §4.1, §4.5, §20.2).
pub fn codex_block(mcp: &CodexMcp, enable_hooks: bool) -> String {
    let mut body = String::from("[mcp_servers.kioku]\n");
    match mcp {
        CodexMcp::Stdio(bin) => {
            body.push_str(&format!(
                "command = {}\nargs = [\"mcp\"]\n",
                toml_path_string(bin)
            ));
        }
        CodexMcp::Http(url, token) => {
            body.push_str(&format!("url = {}\n", toml_string(url)));
            if let Some(token) = token.as_deref() {
                body.push_str(&format!(
                    "http_headers = {{ Authorization = {} }}\n",
                    toml_string(&format!("Bearer {token}"))
                ));
            }
        }
    }
    if enable_hooks {
        body.push_str("[features]\nhooks = true\n");
    }
    wrap(&body, TOML_MARKERS)
}

/// Which part of a Codex managed block a line belongs to (see [`normalize_codex_blocks`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    /// Before the first table header inside the block.
    Prefix,
    /// `[mcp_servers.kioku]` (or a sub-table of it): kioku's own.
    Ours,
    /// A `[features]` table inside the block: ours when it holds only `hooks = true`.
    Features,
    /// Any other table: written by someone else (toml_edit appends new tables before the
    /// document's trailing comment, i.e. before our end marker).
    Foreign,
}

/// Dotted name of a `[table]` / `[[array]]` header line (quotes and spaces removed; good
/// enough to recognise our two tables), or `None` for any other line.
fn header_name(line: &str) -> Option<String> {
    let t = line.trim();
    if !t.starts_with('[') {
        return None;
    }
    let array = t.starts_with("[[");
    let inner = t.trim_start_matches('[');
    let end = inner.find(']')?;
    let name: String = inner[..end]
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '"' | '\''))
        .collect();
    Some(if array { format!("[[{name}") } else { name })
}

/// True for a key line of our `[features]` table (`hooks = true`, any spacing / comment).
fn is_hooks_true(line: &str) -> bool {
    let bare = line.split('#').next().unwrap_or_default();
    let compact: String = bare.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "hooks=true"
}

/// `text` with every Codex managed block reduced to kioku's own content (M2 §4.6):
/// tables inside the markers that kioku did not write — Codex's `toml_edit` writer appends new
/// tables (`[projects."…"]`, `[hooks.state.…]`) before the trailing end-marker comment — are
/// moved, verbatim, to just after the end marker; key lines before the block's first table
/// header (they belong to the table above the block) move to just before the begin marker.
/// Text without such lines is returned unchanged.
pub fn normalize_codex_blocks(text: &str) -> anyhow::Result<String> {
    let ranges = block_ranges(text, TOML_MARKERS)?;
    if ranges.is_empty() {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (start, end) in ranges {
        out.push_str(&text[pos..start]);
        pos = end;
        let lines: Vec<&str> = text[start..end].split_inclusive('\n').collect();
        let (begin, rest) = lines.split_first().expect("a block has a begin line");
        let (end_line, inner) = rest.split_last().expect("a block has an end line");
        let mut prefix = String::new();
        let mut ours = String::new();
        let mut salvaged = String::new();
        let mut segment = Segment::Prefix;
        let mut features: Vec<&str> = Vec::new();
        let flush_features =
            |features: &mut Vec<&str>, ours: &mut String, salvaged: &mut String| {
                if features.is_empty() {
                    return;
                }
                let only_hooks = features[1..].iter().all(|l| {
                    let t = l.trim();
                    t.is_empty() || t.starts_with('#') || is_hooks_true(t)
                });
                let target = if only_hooks { ours } else { salvaged };
                for l in features.drain(..) {
                    target.push_str(l);
                }
            };
        for &line in inner {
            if let Some(name) = header_name(line) {
                flush_features(&mut features, &mut ours, &mut salvaged);
                segment = if name == "mcp_servers.kioku" || name.starts_with("mcp_servers.kioku.") {
                    Segment::Ours
                } else if name == "features" {
                    Segment::Features
                } else {
                    Segment::Foreign
                };
            }
            let trimmed = line.trim();
            match segment {
                Segment::Prefix => {
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        prefix.push_str(line);
                    }
                }
                Segment::Ours => ours.push_str(line),
                Segment::Features => features.push(line),
                Segment::Foreign => salvaged.push_str(line),
            }
        }
        flush_features(&mut features, &mut ours, &mut salvaged);
        if !prefix.is_empty() {
            out.push_str(&prefix);
            if !prefix.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str(begin);
        out.push_str(&ours);
        out.push_str(end_line);
        let salvaged = salvaged.trim_end_matches(['\n', '\r']);
        if !salvaged.is_empty() {
            if !end_line.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            out.push_str(salvaged);
            out.push('\n');
        }
    }
    out.push_str(&text[pos..]);
    Ok(out)
}

fn parse_toml(text: &str) -> anyhow::Result<toml::Table> {
    toml::from_str::<toml::Table>(text).map_err(|e| anyhow::anyhow!("{e}"))
}

fn table_get<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut v = t.get(*first)?;
    for key in rest {
        v = v.as_table()?.get(*key)?;
    }
    Some(v)
}

/// Result of editing Codex's `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexConfigChange {
    /// What happened to the file.
    pub outcome: FileOutcome,
    /// Warnings / info lines (never the token); a skipped edit carries the snippet.
    pub notes: Vec<String>,
}

/// Info / warning lines about a parsed Codex config (M2 §4.1): hooks feature disabled.
pub fn codex_feature_warnings(table: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["hooks", "codex_hooks"] {
        if table_get(table, &["features", key]).and_then(toml::Value::as_bool) == Some(false) {
            out.push(format!(
                "warning: Codex hooks are disabled ([features] {key} = false); kioku's hooks will not run until you remove that line"
            ));
        }
    }
    out
}

/// True when a Codex `config.toml` defines inline hooks (a `[hooks]` table with anything but
/// the trust state `hooks.state`).
pub fn codex_has_inline_hooks(table: &toml::Table) -> bool {
    table
        .get("hooks")
        .and_then(toml::Value::as_table)
        .is_some_and(|h| h.keys().any(|k| k != "state"))
}

/// Reads and parses a `config.toml`; `None` when missing or unparseable.
pub fn read_codex_config(path: &Path) -> Option<toml::Table> {
    let text = read_text(path).ok()??;
    parse_toml(&text).ok()
}

/// Installs the managed block into the Codex `config.toml` at `path` (M2 §4.6).
pub fn install_codex_config(
    path: &Path,
    mcp: CodexMcp,
    enable_hooks_feature: bool,
    dry_run: bool,
) -> anyhow::Result<CodexConfigChange> {
    let before = read_text(path)?;
    let text = before.as_deref().unwrap_or("");
    let mut notes = Vec::new();
    let manual = |why: String, mut notes: Vec<String>, block: &str| {
        notes.push(format!(
            "{why}\nAdd this to {} yourself to register the MCP server:\n{}",
            path.display(),
            block.trim_end()
        ));
        CodexConfigChange {
            outcome: FileOutcome::Unchanged,
            notes,
        }
    };
    // 1. The file must parse.
    let parsed = match parse_toml(text) {
        Ok(t) => t,
        Err(e) => {
            let block = codex_block(&mcp, enable_hooks_feature);
            return Ok(manual(
                format!(
                    "{} is not valid TOML ({}); not touching it",
                    path.display(),
                    e.to_string().lines().next().unwrap_or_default()
                ),
                notes,
                &block,
            ));
        }
    };
    notes.extend(codex_feature_warnings(&parsed));
    // Tables another writer put inside our block move out of it first (M2 §4.6).
    let normalized = match normalize_codex_blocks(text) {
        Ok(n) => n,
        Err(e) => {
            let block = codex_block(&mcp, enable_hooks_feature);
            return Ok(manual(
                format!("{}: {e:#}; not touching it", path.display()),
                notes,
                &block,
            ));
        }
    };
    let text = normalized.as_str();
    let ranges = match block_ranges(text, TOML_MARKERS) {
        Ok(r) => r,
        Err(e) => {
            let block = codex_block(&mcp, enable_hooks_feature);
            return Ok(manual(
                format!("{}: {e:#}; not touching it", path.display()),
                notes,
                &block,
            ));
        }
    };
    let (foreign_text, _) = remove_blocks(text, TOML_MARKERS)?;
    let foreign = parse_toml(&foreign_text).unwrap_or_default();
    // [features] hooks = true is sticky once kioku wrote it.
    let ours_had_feature = ranges.iter().any(|&(s, e)| {
        parse_toml(&text[s..e])
            .ok()
            .and_then(|t| table_get(&t, &["features", "hooks"]).and_then(toml::Value::as_bool))
            == Some(true)
    });
    let mut feature = enable_hooks_feature || ours_had_feature;
    if feature && foreign.contains_key("features") {
        feature = false;
        if table_get(&foreign, &["features", "hooks"]).and_then(toml::Value::as_bool) != Some(true)
        {
            notes.push(format!(
                "{} already has a [features] table: add `hooks = true` under it yourself",
                path.display()
            ));
        }
    }
    let block = codex_block(&mcp, feature);
    // 3. A foreign `mcp_servers.kioku` (table or inline) is not ours to replace.
    if table_get(&foreign, &["mcp_servers", "kioku"]).is_some() {
        return Ok(manual(
            format!(
                "warning: {} already defines mcp_servers.kioku outside kioku's managed block; not touching it",
                path.display()
            ),
            notes,
            &block,
        ));
    }
    // 2 / 4. Replace the block in place, or append it.
    let after = upsert_block(text, &block, TOML_MARKERS)?;
    // 5. The result must parse and carry our entry.
    let ok = parse_toml(&after).is_ok_and(|t| mcp.matches(&t));
    if !ok {
        return Ok(manual(
            format!(
                "adding kioku's block would not leave {} as valid TOML with mcp_servers.kioku; not touching it",
                path.display()
            ),
            notes,
            &block,
        ));
    }
    if before.as_deref() == Some(after.as_str()) {
        return Ok(CodexConfigChange {
            outcome: FileOutcome::Unchanged,
            notes,
        });
    }
    if !dry_run && write_text(path, before.is_some(), &after, true)?.made_private {
        notes.push(super::made_private_note(path));
    }
    Ok(CodexConfigChange {
        outcome: FileOutcome::Written,
        notes,
    })
}

/// Removes the managed block from the Codex `config.toml` at `path` (bytes outside it kept).
pub fn uninstall_codex_config(path: &Path, dry_run: bool) -> anyhow::Result<FileOutcome> {
    let Some(before) = read_text(path)? else {
        return Ok(FileOutcome::Unchanged);
    };
    let (after, found) = normalize_codex_blocks(&before)
        .and_then(|n| remove_blocks(&n, TOML_MARKERS))
        .with_context(|| format!("{}: not touching it", path.display()))?;
    if !found {
        return Ok(FileOutcome::Unchanged);
    }
    save_after_removal(path, &after, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:7391/mcp";

    const ODD: &str = "# my codex config\nmodel = \"gpt-5-codex\"   # trailing comment\n\n[projects.\"/Users/me/src/a b\"]\ntrust_level = \"trusted\"\n\n[mcp_servers.other]\ncommand = \"npx\"\nargs = [ \"-y\",\n  \"other-mcp\" ]  \n\n# last comment\n";

    #[test]
    fn toml_block_round_trips_with_comments_and_other_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, ODD).unwrap();

        let r =
            install_codex_config(&path, CodexMcp::http(URL, Some("tok")), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Written);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(ODD),
            "bytes outside the block are identical"
        );
        let t = parse_toml(&text).unwrap();
        assert_eq!(
            table_get(&t, &["mcp_servers", "kioku", "url"])
                .unwrap()
                .as_str(),
            Some(URL)
        );
        assert_eq!(
            table_get(
                &t,
                &["mcp_servers", "kioku", "http_headers", "Authorization"]
            )
            .unwrap()
            .as_str(),
            Some("Bearer tok")
        );
        assert_eq!(
            table_get(&t, &["mcp_servers", "other", "command"])
                .unwrap()
                .as_str(),
            Some("npx")
        );
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), ODD);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }

        // Second run: byte-identical, no write.
        let r =
            install_codex_config(&path, CodexMcp::http(URL, Some("tok")), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Unchanged);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // A new URL / token replaces the block in place; the user's later additions stay.
        let mut edited = text.clone();
        edited.push_str("\n[tui]\nnotifications = true\n");
        std::fs::write(&path, &edited).unwrap();
        let r = install_codex_config(
            &path,
            CodexMcp::http("https://kioku.lan/mcp", Some("t2")),
            false,
            false,
        )
        .unwrap();
        assert_eq!(r.outcome, FileOutcome::Written);
        let replaced = std::fs::read_to_string(&path).unwrap();
        assert_eq!(block_ranges(&replaced, TOML_MARKERS).unwrap().len(), 1);
        assert!(replaced.contains("https://kioku.lan/mcp") && !replaced.contains(URL));
        assert!(replaced.ends_with("[tui]\nnotifications = true\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640, "an existing file keeps its mode");
        }

        // Uninstall restores the original bytes (plus the user's own later edit).
        assert_eq!(
            uninstall_codex_config(&path, false).unwrap(),
            FileOutcome::Written
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{ODD}\n[tui]\nnotifications = true\n")
        );
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), ODD);
        assert_eq!(
            uninstall_codex_config(&path, false).unwrap(),
            FileOutcome::Unchanged
        );
    }

    #[test]
    fn toml_uninstall_restores_exact_bytes_and_new_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, ODD).unwrap();
        install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("http_headers"), "no token → no header");
        uninstall_codex_config(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ODD);

        // A config.toml kioku created holds the token (0600) and disappears on uninstall.
        let fresh = dir.path().join("new").join("config.toml");
        install_codex_config(&fresh, CodexMcp::http(URL, Some("tok")), false, false).unwrap();
        assert!(
            std::fs::read_to_string(&fresh)
                .unwrap()
                .starts_with(TOML_MARKERS.begin)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(
            uninstall_codex_config(&fresh, false).unwrap(),
            FileOutcome::Deleted
        );
        assert!(!fresh.exists());
    }

    #[cfg(unix)]
    #[test]
    fn toml_adding_the_token_makes_a_readable_config_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"o3\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r =
            install_codex_config(&path, CodexMcp::http(URL, Some("tok")), false, false).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
        assert!(r.notes[0].contains("0600") && !r.notes[0].contains("tok\""));
        // Without a token nothing is tightened.
        std::fs::write(&path, "model = \"o3\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r = install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        assert!(r.notes.is_empty());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn toml_foreign_unparseable_and_inline_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for foreign in [
            "[mcp_servers.kioku]\nurl = \"http://elsewhere/mcp\"\n",
            "mcp_servers.kioku = { url = \"http://elsewhere/mcp\" }\n",
            "[mcp_servers]\nkioku = { command = \"kioku-mcp\" }\n",
        ] {
            std::fs::write(&path, foreign).unwrap();
            let r = install_codex_config(
                &path,
                CodexMcp::http(URL, Some("secret-token")),
                false,
                false,
            )
            .unwrap();
            assert_eq!(r.outcome, FileOutcome::Unchanged, "{foreign}");
            let note = r.notes.join("\n");
            assert!(
                note.contains("warning") && note.contains("[mcp_servers.kioku]"),
                "{note}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), foreign);
        }
        // Unparseable.
        std::fs::write(&path, "model = \n[[[").unwrap();
        let r = install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Unchanged);
        assert!(r.notes[0].contains("not valid TOML"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = \n[[[");
        // An inline root `mcp_servers = {…}` cannot take a `[mcp_servers.kioku]` table: the
        // re-parse check refuses the edit.
        let inline = "mcp_servers = { other = { command = \"x\" } }\n";
        std::fs::write(&path, inline).unwrap();
        let r = install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Unchanged);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);
        // A begin marker without an end marker.
        let broken = format!("{}\n[mcp_servers.kioku]\nurl = \"x\"\n", TOML_MARKERS.begin);
        std::fs::write(&path, &broken).unwrap();
        let r = install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Unchanged);
        assert!(uninstall_codex_config(&path, false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn toml_hooks_feature_is_sticky_and_respects_a_foreign_features_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"o3\"\n").unwrap();
        install_codex_config(&path, CodexMcp::http(URL, None), true, false).unwrap();
        let t = parse_toml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            table_get(&t, &["features", "hooks"]).unwrap().as_bool(),
            Some(true)
        );
        // Reinstall without the flag keeps it (it is inside our block).
        let r = install_codex_config(&path, CodexMcp::http(URL, None), false, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Unchanged);

        let foreign = "[features]\nweb_search = true\nhooks = false\n";
        std::fs::write(&path, foreign).unwrap();
        let r = install_codex_config(&path, CodexMcp::http(URL, None), true, false).unwrap();
        assert_eq!(r.outcome, FileOutcome::Written);
        let notes = r.notes.join("\n");
        assert!(notes.contains("hooks = false"), "{notes}");
        assert!(notes.contains("add `hooks = true`"), "{notes}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(foreign));
        assert_eq!(text.matches("[features]").count(), 1);
        assert!(parse_toml(&text).is_ok());
    }

    /// Codex's toml_edit writer appends new tables before the document's trailing comment —
    /// our end marker — so they land inside the block. They must survive every rewrite.
    #[test]
    fn toml_tables_codex_appended_inside_the_block_survive_install_and_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let foreign_tail = "[projects.\"/Users/me/src/app\"]\ntrust_level = \"trusted\"\n\n[hooks.state.x]\ntrusted_hash = \"sha256:abc\"\n";
        for original in ["", "model = \"o3\"\n"] {
            std::fs::write(&path, original).unwrap();
            let _ = std::fs::remove_file(backup_path(&path));
            install_codex_config(&path, CodexMcp::http(URL, Some("tok")), true, false).unwrap();
            let installed = std::fs::read_to_string(&path).unwrap();
            // Simulate Codex (toml_edit) adding trust tables at the end of the document.
            let cut = installed.rfind(TOML_MARKERS.end).unwrap();
            let edited = format!("{}\n{foreign_tail}{}", &installed[..cut], &installed[cut..]);
            let t = parse_toml(&edited).unwrap();
            assert!(table_get(&t, &["projects", "/Users/me/src/app"]).is_some());
            std::fs::write(&path, &edited).unwrap();

            // install → install → uninstall: the foreign tables are kept every time.
            let r = install_codex_config(&path, CodexMcp::http(URL, Some("tok")), false, false)
                .unwrap();
            assert_eq!(r.outcome, FileOutcome::Written, "{original:?}");
            let once = std::fs::read_to_string(&path).unwrap();
            let block_end = once.find(TOML_MARKERS.end).unwrap();
            assert!(
                once[block_end..].contains(foreign_tail),
                "moved after the block: {once}"
            );
            let t = parse_toml(&once).unwrap();
            assert_eq!(
                table_get(&t, &["projects", "/Users/me/src/app", "trust_level"])
                    .unwrap()
                    .as_str(),
                Some("trusted")
            );
            assert_eq!(
                table_get(&t, &["hooks", "state", "x", "trusted_hash"])
                    .unwrap()
                    .as_str(),
                Some("sha256:abc")
            );
            assert_eq!(
                table_get(&t, &["features", "hooks"]).unwrap().as_bool(),
                Some(true),
                "our sticky [features] stays ours"
            );
            let r = install_codex_config(&path, CodexMcp::http(URL, Some("tok")), false, false)
                .unwrap();
            assert_eq!(r.outcome, FileOutcome::Unchanged);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), once);

            uninstall_codex_config(&path, false).unwrap();
            let left = std::fs::read_to_string(&path).unwrap();
            assert!(!left.contains("kioku"), "{left}");
            assert!(left.contains(foreign_tail), "{left}");
            assert!(left.starts_with(original));
            let t = parse_toml(&left).unwrap();
            assert!(table_get(&t, &["hooks", "state", "x", "trusted_hash"]).is_some());
            assert!(t.get("features").is_none());
        }
    }

    #[test]
    fn normalize_moves_foreign_features_and_prefix_keys_out_of_the_block() {
        let text = format!(
            "[tui]\n{}\nnotifications = true\n[mcp_servers.kioku]\nurl = \"x\"\n[features]\nhooks = true\nweb_search = true\n{}\n",
            TOML_MARKERS.begin, TOML_MARKERS.end
        );
        let n = normalize_codex_blocks(&text).unwrap();
        assert_eq!(
            n,
            format!(
                "[tui]\nnotifications = true\n{}\n[mcp_servers.kioku]\nurl = \"x\"\n{}\n\n[features]\nhooks = true\nweb_search = true\n",
                TOML_MARKERS.begin, TOML_MARKERS.end
            )
        );
        assert_eq!(parse_toml(&n).unwrap(), parse_toml(&text).unwrap());
        // A clean block is left byte-identical.
        let clean = codex_block(&CodexMcp::http(URL, None), true);
        assert_eq!(normalize_codex_blocks(&clean).unwrap(), clean);
    }

    #[test]
    fn inline_hooks_detection_ignores_trust_state() {
        let t = parse_toml("[hooks.state.\"x\"]\ntrusted_hash = \"sha256:1\"\n").unwrap();
        assert!(!codex_has_inline_hooks(&t));
        let t = parse_toml("[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ncommand = \"x\"\n").unwrap();
        assert!(codex_has_inline_hooks(&t));
    }

    #[test]
    fn toml_string_escapes() {
        assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        let t = parse_toml(&format!("k = {}", toml_string("x\"\\\n\u{1}y"))).unwrap();
        assert_eq!(t["k"].as_str(), Some("x\"\\\n\u{1}y"));
    }

    #[test]
    fn md_block_insert_replace_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let original = "# Project rules\n\n- use pnpm\n- 日本語でコメントを書く\n";
        std::fs::write(&path, original).unwrap();
        let body = instructions_body(Lang::Ja, None);
        assert_eq!(
            install_md_block(&path, &body, false).unwrap(),
            FileOutcome::Written
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(original));
        assert!(text.contains("`kioku project id` の出力"));
        assert_eq!(
            install_md_block(&path, &body, false).unwrap(),
            FileOutcome::Unchanged
        );
        // Replaced in place (language switch), never duplicated; text after it survives.
        let with_tail = format!("{text}\n## Later section\n");
        std::fs::write(&path, &with_tail).unwrap();
        install_md_block(
            &path,
            &instructions_body(Lang::En, Some("app-12345678")),
            false,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(MD_MARKERS.end).count(), 1);
        assert!(text.contains("(project id: `app-12345678`)"));
        assert!(text.ends_with("\n## Later section\n"));
        // Duplicated blocks (e.g. pasted twice) collapse to one.
        let doubled = format!("{text}\n{}", wrap("old", MD_MARKERS));
        std::fs::write(&path, &doubled).unwrap();
        install_md_block(&path, &body, false).unwrap();
        assert_eq!(
            block_ranges(&std::fs::read_to_string(&path).unwrap(), MD_MARKERS)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            uninstall_md_block(&path, false).unwrap(),
            FileOutcome::Written
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{original}\n## Later section\n")
        );

        // A file kioku created is deleted when the block goes.
        let fresh = dir.path().join("sub").join("GEMINI.md");
        install_md_block(&fresh, &body, false).unwrap();
        assert_eq!(
            uninstall_md_block(&fresh, false).unwrap(),
            FileOutcome::Deleted
        );
        assert!(!fresh.exists());
    }

    #[test]
    fn snippet_is_small_and_mdc_is_exact() {
        for lang in [Lang::Ja, Lang::En] {
            let block = wrap(
                &instructions_body(lang, Some("chord-life-ace9dc4a")),
                MD_MARKERS,
            );
            assert!(block.len() < 1024, "{lang:?}: {} bytes", block.len());
            assert_eq!(block.matches("\n- ").count(), 4, "four bullets");
        }
        let body = instructions_body(Lang::Ja, Some("chord-life-ace9dc4a"));
        let mdc = mdc_content(&body);
        assert_eq!(
            mdc,
            format!(
                "---\ndescription: kioku shared memory — read the handoff at start, write one before finishing\nalwaysApply: true\n---\n{body}"
            )
        );
        assert!(is_our_mdc(&mdc));
        assert!(!is_our_mdc("---\ndescription: mine\n---\n"));
    }
}
