//! Rule-based (zero-LLM) session digest (spec §7.2) and its auto-generated handoff section.
//!
//! A digest is built by folding observations in seq order ([`SessionDigest::extend`]); its
//! [`DigestTally`] keeps the lossless running totals, so a cached digest extended with the
//! newer observations equals a digest built from scratch (SPEC-M2.8 §1). [`stub_payload`]
//! reduces an observation to exactly what the fold reads (SPEC-M2.8 §3).

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::carry::HANDOFF_REPLY_MAX;
use crate::handoff::Handoff;
use crate::session::{ASSISTANT_MAX, Observation, ObservationKind};
use crate::strings::{Lang, fill, strings};
use crate::util::{one_line, parse_ts, truncate_chars};

/// Max chars kept per prompt.
pub const PROMPT_MAX: usize = 300;
/// Max chars kept per command line.
pub const COMMAND_MAX: usize = 160;
/// Distinct commands kept from the start of the session (SPEC-M2.8 §6).
pub const COMMANDS_FIRST: usize = 10;
/// Distinct commands kept from the end of the session (SPEC-M2.8 §6).
pub const COMMANDS_LAST: usize = 20;
/// Max distinct commands kept ([`COMMANDS_FIRST`] + [`COMMANDS_LAST`]).
pub const COMMANDS_MAX: usize = COMMANDS_FIRST + COMMANDS_LAST;
/// Max read paths kept.
pub const READS_MAX: usize = 20;

const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// A path with how often it was touched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCount {
    /// Path, relative to the project root when possible.
    pub path: String,
    /// Number of tool calls touching it.
    pub count: u32,
}

/// The lossless running totals behind a digest's truncated lists, so a digest can be
/// extended with newer observations (SPEC-M2.8 §1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DigestTally {
    /// Root the paths were made relative to (a cached digest is only extended with the
    /// same root).
    pub root: Option<String>,
    /// Highest observation seq folded in (0 = none).
    pub seq: i64,
    /// Edited files with counts, in first-seen order.
    pub edits: Vec<FileCount>,
    /// Read files with counts, in first-seen order.
    pub reads: Vec<FileCount>,
    /// Every distinct command first line, in first-seen order.
    pub commands: Vec<String>,
}

/// What happened in a session, derived purely from its observations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionDigest {
    /// Prompts in order, each cleaned ([`clean_prompt`]) and truncated to 300 chars.
    pub prompts: Vec<String>,
    /// Edited files, most edits first.
    pub files: Vec<FileCount>,
    /// Read files, most reads first (top 20).
    pub reads: Vec<String>,
    /// First lines of Bash commands, deduplicated in first-seen order (the first 10 and
    /// the last 20 kept).
    pub commands: Vec<String>,
    /// Messages of `git commit` commands, deduplicated (first kept).
    pub git_commits: Vec<String>,
    /// Tool calls that look like they failed (heuristic).
    pub errors: u32,
    /// The last agent-written handoff of the session (filled in by the store).
    pub agent_handoff: Option<Handoff>,
    /// Number of prompts.
    pub prompt_count: u32,
    /// Number of tool uses.
    pub tool_use_count: u32,
    /// Timestamp of the first observation.
    pub first_ts: Option<String>,
    /// Timestamp of the last observation.
    pub last_ts: Option<String>,
    /// Seconds between the first and last observation.
    pub duration_secs: i64,
    /// Running totals the lists above are derived from.
    #[serde(default)]
    pub tally: DigestTally,
    /// The agent's last reply (`assistant` observation, SPEC-M3.0 §3), ≤ 2,000 chars.
    #[serde(default)]
    pub last_reply: Option<String>,
    /// Number of `assistant` observations.
    #[serde(default)]
    pub reply_count: u32,
}

/// Tool uses that make a change worth a new rules handoff on their own (SPEC-M3.0 §4).
pub const MEANINGFUL_TOOL_USES: u32 = 5;

/// What the digest reads from one tool use, from a full payload or a stub alike.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ToolFacts {
    tool: String,
    /// Edited paths (edit tools) or the read path (`Read`).
    paths: Vec<String>,
    /// First line of a Bash command, truncated to [`COMMAND_MAX`].
    command: Option<String>,
    /// Message of a `git commit` in that command.
    git_commit: Option<String>,
    is_error: bool,
}

fn is_stub(payload: &Value) -> bool {
    payload.get("stub").and_then(Value::as_bool) == Some(true)
}

fn tool_facts(payload: &Value) -> ToolFacts {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if is_stub(payload) {
        let mut paths: Vec<String> = payload
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect();
        if let Some(more) = payload.get("paths").and_then(Value::as_array) {
            paths = more
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        let s = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
        return ToolFacts {
            tool,
            paths,
            command: s("command"),
            git_commit: s("git_commit"),
            is_error: payload.get("is_error").and_then(Value::as_bool) == Some(true),
        };
    }
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let mut facts = ToolFacts {
        is_error: payload.get("tool_response").is_some_and(is_error_response),
        ..ToolFacts::default()
    };
    if EDIT_TOOLS.contains(&tool.as_str()) {
        facts.paths = edit_paths(input);
    } else if tool == "Read" {
        facts.paths = input_path(input).into_iter().collect();
    } else if tool == "Bash"
        && let Some(cmd) = input.get("command").and_then(Value::as_str)
    {
        let first = truncate_chars(cmd.lines().next().unwrap_or("").trim(), COMMAND_MAX);
        facts.command = (!first.is_empty()).then_some(first);
        facts.git_commit = git_commit_message(cmd);
    }
    facts.tool = tool;
    facts
}

/// The reply text the digest keeps for an `assistant` observation (stub or full payload).
fn reply_text(obs: &Observation) -> String {
    let text = obs
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or(&obs.text);
    truncate_chars(text.trim(), ASSISTANT_MAX)
}

/// The prompt text the digest keeps for a `prompt` observation.
fn prompt_text(obs: &Observation) -> String {
    let text = obs
        .payload
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or(&obs.text);
    truncate_chars(&clean_prompt(text), PROMPT_MAX)
}

/// The reduced payload of an observation (SPEC-M2.8 §3): exactly what the digest reads, so
/// a digest of reduced observations equals the digest of the originals. A tool use keeps
/// `tool_name`, its path(s) or first command line, the commit message and `is_error`; a
/// prompt keeps its cleaned, truncated text; anything else keeps nothing.
pub fn stub_payload(kind: ObservationKind, payload: &Value, text: &str) -> Value {
    if is_stub(payload) {
        return payload.clone();
    }
    match kind {
        ObservationKind::Prompt => {
            let obs = Observation {
                id: 0,
                session_id: String::new(),
                project_id: String::new(),
                seq: 0,
                kind,
                ts: String::new(),
                payload: payload.clone(),
                text: text.to_string(),
            };
            json!({"stub": true, "prompt": prompt_text(&obs)})
        }
        ObservationKind::ToolUse => {
            let f = tool_facts(payload);
            let mut out = json!({"stub": true, "tool_name": f.tool, "is_error": f.is_error});
            if let Some(first) = f.paths.first() {
                out["path"] = json!(first);
            }
            if f.paths.len() > 1 {
                out["paths"] = json!(f.paths);
            }
            if let Some(c) = f.command {
                out["command"] = json!(c);
            }
            if let Some(m) = f.git_commit {
                out["git_commit"] = json!(m);
            }
            out
        }
        ObservationKind::Assistant => {
            let obs = Observation {
                id: 0,
                session_id: String::new(),
                project_id: String::new(),
                seq: 0,
                kind,
                ts: String::new(),
                payload: payload.clone(),
                text: text.to_string(),
            };
            json!({"stub": true, "text": reply_text(&obs)})
        }
        ObservationKind::Stop | ObservationKind::Compact | ObservationKind::Note => {
            json!({"stub": true})
        }
    }
}

impl SessionDigest {
    /// Builds a digest from observations (in seq order); paths are made relative to `root`.
    pub fn from_observations(observations: &[Observation], root: Option<&str>) -> SessionDigest {
        let mut d = SessionDigest {
            tally: DigestTally {
                root: root.map(str::to_string),
                ..DigestTally::default()
            },
            ..SessionDigest::default()
        };
        d.extend(observations);
        d
    }

    /// Folds newer observations (in seq order) into the digest; observations at or below
    /// `tally.seq` are already in it and skipped. Pure: `from_observations(a ++ b)` equals
    /// `from_observations(a)` extended with `b`.
    pub fn extend(&mut self, observations: &[Observation]) {
        let root = self.tally.root.clone();
        for obs in observations {
            if self.tally.seq > 0 && obs.seq <= self.tally.seq {
                continue;
            }
            self.fold(obs, root.as_deref());
            self.tally.seq = self.tally.seq.max(obs.seq);
        }
        self.derive();
    }

    fn fold(&mut self, obs: &Observation, root: Option<&str>) {
        if self.first_ts.is_none() {
            self.first_ts = Some(obs.ts.clone());
        }
        self.last_ts = Some(obs.ts.clone());
        match obs.kind {
            ObservationKind::Prompt => {
                self.prompt_count += 1;
                self.prompts.push(prompt_text(obs));
            }
            ObservationKind::ToolUse => {
                self.tool_use_count += 1;
                let f = tool_facts(&obs.payload);
                if EDIT_TOOLS.contains(&f.tool.as_str()) {
                    for p in &f.paths {
                        bump(&mut self.tally.edits, &relative(p, root));
                    }
                } else if f.tool == "Read" {
                    if let Some(p) = f.paths.first() {
                        bump(&mut self.tally.reads, &relative(p, root));
                    }
                } else if f.tool == "Bash" {
                    if let Some(c) = f.command
                        && !self.tally.commands.contains(&c)
                    {
                        self.tally.commands.push(c);
                    }
                    if let Some(m) = f.git_commit
                        && !self.git_commits.contains(&m)
                    {
                        self.git_commits.push(m);
                    }
                }
                if f.is_error {
                    self.errors += 1;
                }
            }
            ObservationKind::Assistant => {
                let text = reply_text(obs);
                if !text.is_empty() {
                    self.reply_count += 1;
                    self.last_reply = Some(text);
                }
            }
            ObservationKind::Stop | ObservationKind::Compact | ObservationKind::Note => {}
        }
    }

    /// Recomputes the truncated lists and the duration from the tally.
    fn derive(&mut self) {
        let mut edits = self.tally.edits.clone();
        let mut reads = self.tally.reads.clone();
        // Stable sorts: equal counts keep their first-seen order.
        edits.sort_by_key(|f| std::cmp::Reverse(f.count));
        reads.sort_by_key(|f| std::cmp::Reverse(f.count));
        self.files = edits;
        self.reads = reads.into_iter().take(READS_MAX).map(|f| f.path).collect();
        let all = &self.tally.commands;
        self.commands = if all.len() <= COMMANDS_MAX {
            all.clone()
        } else {
            all[..COMMANDS_FIRST]
                .iter()
                .chain(&all[all.len() - COMMANDS_LAST..])
                .cloned()
                .collect()
        };
        self.duration_secs = match (
            self.first_ts.as_deref().and_then(parse_ts),
            self.last_ts.as_deref().and_then(parse_ts),
        ) {
            (Some(a), Some(b)) => (b - a).num_seconds().max(0),
            _ => 0,
        };
    }

    /// True when the session had at least one prompt or tool use (spec §7.1 step 2).
    pub fn is_substantive(&self) -> bool {
        self.prompt_count >= 1 || self.tool_use_count >= 1
    }

    /// For a digest of the observations since a rules handoff was issued: whether they
    /// warrant a new one (SPEC-M3.0 §4) — a prompt, a file edit, a commit, a reply, or at
    /// least [`MEANINGFUL_TOOL_USES`] tool uses.
    pub fn is_meaningful_change(&self) -> bool {
        self.prompt_count > 0
            || !self.files.is_empty()
            || !self.git_commits.is_empty()
            || self.reply_count > 0
            || self.tool_use_count >= MEANINGFUL_TOOL_USES
    }

    /// The auto-generated "Handoff" section (used when the agent wrote none).
    pub fn handoff_section(&self, lang: Lang) -> String {
        self.section_with_heading(lang, strings(lang).auto_handoff_heading)
    }

    /// The auto-generated addendum (「引き継ぎ（自動生成・追記）」) for work done after the
    /// agent's handoff; `self` is the digest of the observations after that handoff.
    pub fn handoff_delta_section(&self, lang: Lang) -> String {
        self.section_with_heading(lang, strings(lang).auto_handoff_delta_heading)
    }

    fn section_with_heading(&self, lang: Lang, heading: &str) -> String {
        let s = strings(lang);
        let mut lines = vec![heading.to_string()];
        let last_prompt = self
            .prompts
            .last()
            .map(|p| truncate_chars(&one_line(p), PROMPT_MAX))
            .unwrap_or_else(|| s.none.to_string());
        lines.push(fill(s.auto_last_prompt, &[("prompt", &last_prompt)]));
        if !self.files.is_empty() {
            let files = self
                .files
                .iter()
                .take(10)
                .map(|f| format!("{} ({})", f.path, f.count))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(fill(s.auto_files, &[("files", &files)]));
        }
        if !self.commands.is_empty() {
            let skip = self.commands.len().saturating_sub(8);
            let cmds = self
                .commands
                .iter()
                .skip(skip)
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(fill(s.auto_commands, &[("commands", &cmds)]));
        }
        if !self.git_commits.is_empty() {
            let commits = self
                .git_commits
                .iter()
                .map(|c| format!("「{c}」"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(fill(s.auto_commits, &[("commits", &commits)]));
        }
        if self.errors > 0 {
            lines.push(fill(s.auto_errors, &[("n", &self.errors.to_string())]));
        }
        // SPEC-M3.0 §3: the agent's last reply says where it got to; "next steps unknown"
        // only when there is not even that.
        match &self.last_reply {
            Some(reply) => {
                let reply = truncate_chars(&one_line(reply), HANDOFF_REPLY_MAX);
                lines.push(fill(s.auto_last_reply, &[("reply", &reply)]));
            }
            None => lines.push(s.auto_next_unknown.to_string()),
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
}

/// A prompt without the blocks agents wrap around it (SPEC-M2.8 §6): `<command-name>`,
/// `<command-message>`, `<system-reminder>` and `<pasted_content …>` blocks are removed
/// (an unclosed one to the end), as are `<command-args>` blocks, and the result is
/// trimmed. A prompt that was nothing but a slash command becomes the command and its
/// arguments (`/review 12`).
pub fn clean_prompt(text: &str) -> String {
    static BLOCKS: OnceLock<Vec<Regex>> = OnceLock::new();
    static NAME: OnceLock<Regex> = OnceLock::new();
    static ARGS_INNER: OnceLock<Regex> = OnceLock::new();
    let blocks = BLOCKS.get_or_init(|| {
        [
            "command-name",
            "command-message",
            "command-args",
            "system-reminder",
            "pasted_content",
        ]
        .iter()
        .map(|tag| {
            // Closed block, else an unclosed one (to the end), else a self-closing tag.
            Regex::new(&format!(
                r"(?s)<{tag}(?:\s[^>]*)?>.*?</{tag}>|<{tag}(?:\s[^>]*)?/>|<{tag}(?:\s[^>]*)?>.*\z"
            ))
            .expect("valid block regex")
        })
        .collect()
    });
    let name = NAME.get_or_init(|| {
        Regex::new(r"(?s)<command-name>(.*?)</command-name>").expect("valid name regex")
    });
    let mut out = text.to_string();
    for re in blocks {
        out = re.replace_all(&out, "").into_owned();
    }
    let cleaned = out.trim().to_string();
    if !cleaned.is_empty() {
        return cleaned;
    }
    let inner = |re: &Regex| {
        re.captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
    };
    let args_inner = ARGS_INNER.get_or_init(|| {
        Regex::new(r"(?s)<command-args>(.*?)</command-args>").expect("valid args regex")
    });
    match inner(name) {
        Some(cmd) => one_line(&format!("{cmd} {}", inner(args_inner).unwrap_or_default())),
        None => String::new(),
    }
}

/// The first non-empty line of a (cleaned) prompt, for titles (SPEC-M2.8 §6).
pub fn prompt_title_line(prompt: &str) -> String {
    prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn input_path(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Paths an edit-type tool call touched: `file_path` / `notebook_path`, plus every string in
/// `file_paths` (M2 §9.3: Codex `apply_patch` edits several files in one call). Each distinct
/// path counts once per call.
fn edit_paths(input: &Value) -> Vec<String> {
    let mut out: Vec<String> = input_path(input).into_iter().collect();
    if let Some(list) = input.get("file_paths").and_then(Value::as_array) {
        for p in list.iter().filter_map(Value::as_str) {
            let p = p.trim();
            if !p.is_empty() && !out.iter().any(|q| q == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

fn relative(path: &str, root: Option<&str>) -> String {
    if let Some(root) = root
        && let Ok(rel) = Path::new(path).strip_prefix(root)
    {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if !rel.is_empty() {
            return rel;
        }
    }
    path.to_string()
}

fn bump(list: &mut Vec<FileCount>, path: &str) {
    match list.iter_mut().find(|f| f.path == path) {
        Some(f) => f.count += 1,
        None => list.push(FileCount {
            path: path.to_string(),
            count: 1,
        }),
    }
}

/// Prefixes that make an error text (SPEC-M2.8 §6).
const ERROR_PREFIXES: [&str; 6] = [
    "error",
    "Error",
    "fatal:",
    "panicked",
    "Traceback",
    "FAILED",
];

fn starts_with_error(s: &str) -> bool {
    let s = s.trim_start();
    ERROR_PREFIXES.iter().any(|p| s.starts_with(p))
}

/// Whether a tool response failed (SPEC-M2.8 §6): `is_error` / `isError` is true, an
/// `exit_code` / `code` is a non-zero integer, `stderr` or a string `error` (or a plain
/// string response) *starts* with `error`, `Error`, `fatal:`, `panicked`, `Traceback` or
/// `FAILED`, or `error` is a structured (non-string, non-empty) value. Text inside a body
/// (a file that mentions errors, a passing test log) is never matched.
pub fn is_error_response(resp: &Value) -> bool {
    match resp {
        Value::String(s) => starts_with_error(s),
        Value::Object(map) => {
            let flag = |k: &str| map.get(k).and_then(Value::as_bool).unwrap_or(false);
            if flag("is_error") || flag("isError") {
                return true;
            }
            let nonzero = |k: &str| map.get(k).and_then(Value::as_i64).is_some_and(|c| c != 0);
            if nonzero("exit_code") || nonzero("exitCode") || nonzero("code") {
                return true;
            }
            let error = match map.get("error") {
                Some(Value::String(s)) => starts_with_error(s),
                Some(Value::Null) | Some(Value::Bool(false)) | None => false,
                Some(Value::Bool(true)) => true,
                Some(Value::Object(o)) => !o.is_empty(),
                Some(Value::Array(a)) => !a.is_empty(),
                Some(Value::Number(_)) => false,
            };
            error
                || map
                    .get("stderr")
                    .and_then(Value::as_str)
                    .is_some_and(starts_with_error)
        }
        _ => false,
    }
}

/// Extracts the message of a `git commit` in a shell command (first `-m`, heredoc aware).
///
/// Returns `None` when the command does not run `git commit`; when it does but no message
/// can be found, returns the command's first line.
pub fn git_commit_message(cmd: &str) -> Option<String> {
    static COMMIT: OnceLock<Regex> = OnceLock::new();
    static MSG: OnceLock<Regex> = OnceLock::new();
    let commit = COMMIT.get_or_init(|| {
        Regex::new(r"(?m)(?:^|&&|;|\|\||\n)\s*git\s+(?:-[Cc]\s+\S+\s+)*commit\b(?P<rest>[\s\S]*)")
            .expect("valid commit regex")
    });
    let msg = MSG.get_or_init(|| {
        Regex::new(
            r#"(?:\s-[a-zA-Z]*m\s*|\s--message[= ])(?:"(?P<dq>(?:[^"\\]|\\.)*)"|'(?P<sq>[^']*)'|(?P<bare>[^\s"';&|]+))"#,
        )
        .expect("valid message regex")
    });
    let rest = commit.captures(cmd)?.name("rest")?.as_str();
    let first_line = one_line(cmd.lines().next().unwrap_or(cmd));
    let Some(c) = msg.captures(rest) else {
        return Some(truncate_chars(&first_line, COMMAND_MAX));
    };
    let raw = c
        .name("dq")
        .or_else(|| c.name("sq"))
        .or_else(|| c.name("bare"))
        .map(|m| m.as_str())
        .unwrap_or_default();
    let text = if raw.trim_start().starts_with("$(cat <<") {
        // heredoc: the message is the first non-empty line after the `<<EOF` line
        raw.lines()
            .skip(1)
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string()
    } else {
        raw.replace("\\\"", "\"")
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if text.is_empty() {
        Some(truncate_chars(&first_line, COMMAND_MAX))
    } else {
        Some(truncate_chars(&text, COMMAND_MAX))
    }
}

/// Aggregates edited-file counts over several digests, most edits first.
pub fn aggregate_files(digests: &[SessionDigest], limit: usize) -> Vec<FileCount> {
    let mut totals: HashMap<String, (u32, usize)> = HashMap::new();
    let mut order = 0usize;
    for d in digests {
        for f in &d.files {
            let e = totals.entry(f.path.clone()).or_insert_with(|| {
                order += 1;
                (0, order)
            });
            e.0 += f.count;
        }
    }
    let mut out: Vec<(String, u32, usize)> =
        totals.into_iter().map(|(p, (c, o))| (p, c, o)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    out.into_iter()
        .take(limit)
        .map(|(path, count, _)| FileCount { path, count })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obs(seq: i64, kind: ObservationKind, payload: Value) -> Observation {
        Observation {
            id: seq,
            session_id: "s1".into(),
            project_id: "p".into(),
            seq,
            kind,
            ts: format!("2026-09-25T02:{:02}:00.000Z", seq),
            payload,
            text: String::new(),
        }
    }

    fn tool(seq: i64, name: &str, input: Value, response: Value) -> Observation {
        obs(
            seq,
            ObservationKind::ToolUse,
            json!({"tool_name": name, "tool_input": input, "tool_response": response}),
        )
    }

    fn fixture() -> Vec<Observation> {
        let root = "/home/u/kioku";
        vec![
            obs(
                1,
                ObservationKind::Prompt,
                json!({"prompt": "検索のテストを追加して"}),
            ),
            tool(
                2,
                "Read",
                json!({"file_path": format!("{root}/src/index.rs")}),
                json!({"type": "text"}),
            ),
            tool(
                3,
                "Edit",
                json!({"file_path": format!("{root}/src/index.rs"), "old_string": "a", "new_string": "b"}),
                json!({"filePath": "x"}),
            ),
            tool(
                4,
                "Bash",
                json!({"command": "cargo test -p kioku-core\n# second line"}),
                json!({"stdout": "ok", "stderr": ""}),
            ),
            tool(
                5,
                "Write",
                json!({"file_path": format!("{root}/src/new.rs"), "content": "fn main() {}"}),
                json!({}),
            ),
            tool(
                6,
                "Edit",
                json!({"file_path": format!("{root}/src/index.rs")}),
                json!({"is_error": true}),
            ),
            tool(
                7,
                "Bash",
                json!({"command": "cargo test -p kioku-core"}),
                json!({"stdout": "", "stderr": "error[E0425]: cannot find value"}),
            ),
            obs(
                8,
                ObservationKind::Prompt,
                json!({"prompt": format!("続けて。{}", "あ".repeat(400))}),
            ),
            tool(
                9,
                "Bash",
                json!({"command": "git add -A && git commit -m \"feat(core): add search tests\""}),
                json!({"stdout": "[main abc] feat"}),
            ),
            tool(
                10,
                "Read",
                json!({"file_path": "/etc/hosts"}),
                json!("127.0.0.1 localhost"),
            ),
            tool(
                11,
                "Read",
                json!({"file_path": format!("{root}/src/index.rs")}),
                json!({}),
            ),
            obs(
                12,
                ObservationKind::Stop,
                json!({"stop_hook_active": false}),
            ),
        ]
    }

    #[test]
    fn digest_from_fixture() {
        let d = SessionDigest::from_observations(&fixture(), Some("/home/u/kioku"));
        assert_eq!(d.prompt_count, 2);
        assert_eq!(d.tool_use_count, 9);
        assert_eq!(d.prompts[0], "検索のテストを追加して");
        assert_eq!(d.prompts[1].chars().count(), PROMPT_MAX);
        assert_eq!(
            d.files,
            vec![
                FileCount {
                    path: "src/index.rs".into(),
                    count: 2
                },
                FileCount {
                    path: "src/new.rs".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(d.reads, vec!["src/index.rs", "/etc/hosts"]);
        assert_eq!(
            d.commands,
            vec![
                "cargo test -p kioku-core",
                "git add -A && git commit -m \"feat(core): add search tests\""
            ]
        );
        assert_eq!(d.git_commits, vec!["feat(core): add search tests"]);
        assert_eq!(d.errors, 2);
        assert_eq!(d.duration_secs, 11 * 60);
        assert!(d.is_substantive());

        let h = d.handoff_section(Lang::Ja);
        assert!(h.starts_with("## 引き継ぎ（自動生成）\n最後の指示: 続けて。"));
        assert!(h.contains("触ったファイル: src/index.rs (2), src/new.rs (1)"));
        assert!(h.contains("実行したコマンド: `cargo test -p kioku-core`"));
        assert!(h.contains("コミット: 「feat(core): add search tests」"));
        assert!(h.contains("エラーの兆候: 2 件のツール実行がエラーを返した"));
        assert!(h.ends_with("次にやること: （エージェントが明示的に書かなかったため不明。上記を手がかりに再開すること）\n"));
        let en = d.handoff_section(Lang::En);
        assert!(en.starts_with("## Handoff (auto-generated)"));
    }

    #[test]
    fn edit_entries_accept_file_paths() {
        let root = "/home/u/kioku";
        let observations = vec![
            obs(
                1,
                ObservationKind::Prompt,
                json!({"prompt": "パッチを当てて"}),
            ),
            // Codex apply_patch, normalized client-side (M2 §3.5)
            tool(
                2,
                "Edit",
                json!({"file_paths": [format!("{root}/src/a.rs"), "src/b.rs", format!("{root}/src/a.rs"), 7, ""],
                       "patch": "*** Begin Patch"}),
                json!("Success. Updated the following files"),
            ),
            tool(
                3,
                "Edit",
                json!({"file_path": format!("{root}/src/a.rs"), "file_paths": [format!("{root}/src/c.rs")]}),
                json!({}),
            ),
            // Cursor postToolUseFailure / Gemini error, normalized client-side
            tool(
                4,
                "Bash",
                json!({"command": "npm test"}),
                json!({"is_error": true, "error": "Command timed out after 30s", "failure_type": "timeout"}),
            ),
            tool(
                5,
                "Edit",
                json!({"file_path": format!("{root}/src/c.rs")}),
                json!({"llmContent": "failed", "returnDisplay": "…", "error": {"message": "no match"}}),
            ),
        ];
        let d = SessionDigest::from_observations(&observations, Some(root));
        assert_eq!(
            d.files,
            vec![
                FileCount {
                    path: "src/a.rs".into(),
                    count: 2
                },
                FileCount {
                    path: "src/c.rs".into(),
                    count: 2
                },
                FileCount {
                    path: "src/b.rs".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(d.errors, 2);
        assert!(
            d.handoff_section(Lang::Ja)
                .contains("触ったファイル: src/a.rs (2), src/c.rs (2), src/b.rs (1)")
        );
    }

    #[test]
    fn empty_session_is_not_substantive() {
        let d = SessionDigest::from_observations(&[obs(1, ObservationKind::Stop, json!({}))], None);
        assert!(!d.is_substantive());
    }

    #[test]
    fn commit_message_variants() {
        assert_eq!(git_commit_message("git status"), None);
        assert_eq!(
            git_commit_message("git commit -m 'fix: x'").as_deref(),
            Some("fix: x")
        );
        assert_eq!(
            git_commit_message("git commit -am \"wip \\\"q\\\"\"").as_deref(),
            Some("wip \"q\"")
        );
        assert_eq!(
            git_commit_message("cd a && git -C b commit -m msg").as_deref(),
            Some("msg")
        );
        assert_eq!(
            git_commit_message("git commit --message=\"long form\"").as_deref(),
            Some("long form")
        );
        let heredoc = "git commit -m \"$(cat <<'EOF'\nfeat: 引き継ぎを自動化\n\nbody\nEOF\n)\"";
        assert_eq!(
            git_commit_message(heredoc).as_deref(),
            Some("feat: 引き継ぎを自動化")
        );
        assert_eq!(
            git_commit_message("git commit --amend --no-edit").as_deref(),
            Some("git commit --amend --no-edit")
        );
        assert_eq!(git_commit_message("echo git commit is fun"), None);
    }

    #[test]
    fn error_heuristic() {
        assert!(is_error_response(&json!({"is_error": true})));
        assert!(is_error_response(&json!("Error: file not found")));
        assert!(is_error_response(
            &json!({"stderr": "fatal: not a git repository"})
        ));
        assert!(is_error_response(&json!({"error": "error: denied"})));
        assert!(is_error_response(
            &json!({"error": {"message": "no match"}})
        ));
        assert!(!is_error_response(&json!({"stdout": "ok", "stderr": ""})));
        assert!(!is_error_response(&json!({"content": "fn error() {}"})));
        // SPEC-M2.8 §6: body substrings no longer count.
        assert!(!is_error_response(&json!({"stderr": "fatal error"})));
        assert!(!is_error_response(&json!({"error": "denied"})));
        assert!(!is_error_response(
            &json!({"exit_code": 0, "stderr": "warning: x"})
        ));
        assert!(is_error_response(
            &json!({"stderr": "  Traceback (most recent call last):"})
        ));
        assert!(!is_error_response(
            &json!({"stderr": "thread 'main' panicked at"})
        ));
        assert!(is_error_response(
            &json!({"stderr": "panicked at src/main.rs"})
        ));
        assert!(is_error_response(&json!({"code": 2})));
    }

    /// SPEC-M2.8 §6 required cases.
    #[test]
    fn reading_error_rs_and_passing_tests_are_not_errors() {
        let root = "/home/u/kioku";
        let observations = vec![
            tool(
                1,
                "Read",
                json!({"file_path": format!("{root}/crates/kioku-core/src/error.rs")}),
                json!({"type": "text", "file": {"filePath": "error.rs",
                    "content": "//! エラー型\npub enum Error { NotFound(String) }\n// error handling"}}),
            ),
            tool(
                2,
                "Read",
                json!({"file_path": format!("{root}/src/error.rs")}),
                json!("pub struct Error;\nimpl std::error::Error for Error {}"),
            ),
            tool(
                3,
                "Bash",
                json!({"command": "cargo test -p kioku-core"}),
                json!({"stdout": "running 3 tests\ntest error_heuristic ... ok\ntest result: ok. 3 passed; 0 failed",
                       "stderr": "   Compiling kioku-core v0.8.1\n    Finished `test` profile; 0 errors",
                       "exit_code": 0}),
            ),
            tool(
                4,
                "Bash",
                json!({"command": "cargo build"}),
                json!({"stdout": "", "stderr": "   Compiling x\nerror[E0308]: mismatched types", "exit_code": 1}),
            ),
        ];
        let d = SessionDigest::from_observations(&observations, Some(root));
        assert_eq!(d.errors, 1, "only the exit_code 1 build failed");
        assert!(!is_error_response(
            &observations[0].payload["tool_response"]
        ));
        assert!(!is_error_response(
            &observations[2].payload["tool_response"]
        ));
        assert!(is_error_response(&json!({"stdout": "", "exit_code": 1})));
    }

    /// SPEC-M2.8 §6: wrapper blocks are stripped before truncation; a slash command alone
    /// becomes the command; titles use the first non-empty line.
    #[test]
    fn prompt_blocks_are_stripped_for_titles() {
        let command = "<command-name>/review</command-name>\n<command-message>review is running…</command-message>\n<command-args>PR 12 の検索</command-args>";
        assert_eq!(clean_prompt(command), "/review PR 12 の検索");
        let reminder = format!(
            "<system-reminder>\n{}\n</system-reminder>\n\n  検索の日本語テストを直して\n詳細は後で",
            "x".repeat(500)
        );
        assert_eq!(
            clean_prompt(&reminder),
            "検索の日本語テストを直して\n詳細は後で"
        );
        assert_eq!(
            clean_prompt(
                "<command-name>/clear</command-name><command-message>clear</command-message>\n引き継ぎを書いて"
            ),
            "引き継ぎを書いて"
        );
        assert_eq!(
            clean_prompt("<pasted_content id=\"1\" lines=\"40\">log…</pasted_content> これを見て"),
            "これを見て"
        );
        assert_eq!(clean_prompt("<system-reminder>unclosed"), "");
        let observations = vec![
            obs(1, ObservationKind::Prompt, json!({"prompt": command})),
            obs(2, ObservationKind::Prompt, json!({"prompt": reminder})),
        ];
        let d = SessionDigest::from_observations(&observations, None);
        assert_eq!(d.prompts[0], "/review PR 12 の検索");
        assert_eq!(
            prompt_title_line(&d.prompts[1]),
            "検索の日本語テストを直して"
        );
    }

    #[test]
    fn commits_are_deduplicated_and_commands_keep_first_and_last() {
        let mut observations = Vec::new();
        for i in 0..40 {
            observations.push(tool(
                i + 1,
                "Bash",
                json!({"command": format!("echo 手順{i}")}),
                json!({}),
            ));
        }
        for i in 0..2 {
            observations.push(tool(
                41 + i,
                "Bash",
                json!({"command": format!("git commit -m \"feat: 検索\" # {i}")}),
                json!({}),
            ));
        }
        let d = SessionDigest::from_observations(&observations, None);
        assert_eq!(d.git_commits, vec!["feat: 検索"]);
        assert_eq!(d.commands.len(), COMMANDS_MAX);
        assert_eq!(d.commands[0], "echo 手順0");
        assert_eq!(d.commands[9], "echo 手順9");
        assert_eq!(d.commands[10], "echo 手順22");
        assert_eq!(d.commands[29], "git commit -m \"feat: 検索\" # 1");
    }

    /// SPEC-M2.8 §1/§3: extending equals building from scratch, and a digest of stubs
    /// equals the digest of the originals.
    #[test]
    fn extend_and_stubs_equal_from_scratch() {
        let all = fixture();
        let root = Some("/home/u/kioku");
        let full = SessionDigest::from_observations(&all, root);
        for split in 0..=all.len() {
            let mut d = SessionDigest::from_observations(&all[..split], root);
            d.extend(&all[split..]);
            assert_eq!(d, full, "split at {split}");
            // already folded observations are not counted twice
            d.extend(&all);
            assert_eq!(d, full);
        }
        let stubs: Vec<Observation> = all
            .iter()
            .map(|o| Observation {
                payload: stub_payload(o.kind, &o.payload, &o.text),
                text: String::new(),
                ..o.clone()
            })
            .collect();
        assert_eq!(SessionDigest::from_observations(&stubs, root), full);
        // stubbing twice changes nothing
        assert_eq!(
            stub_payload(stubs[3].kind, &stubs[3].payload, ""),
            stubs[3].payload
        );
    }

    #[test]
    fn aggregate_sorts_by_total() {
        let a = SessionDigest {
            files: vec![
                FileCount {
                    path: "a".into(),
                    count: 1,
                },
                FileCount {
                    path: "b".into(),
                    count: 2,
                },
            ],
            ..Default::default()
        };
        let b = SessionDigest {
            files: vec![FileCount {
                path: "a".into(),
                count: 3,
            }],
            ..Default::default()
        };
        let agg = aggregate_files(&[a, b], 10);
        assert_eq!(
            agg[0],
            FileCount {
                path: "a".into(),
                count: 4
            }
        );
        assert_eq!(
            agg[1],
            FileCount {
                path: "b".into(),
                count: 2
            }
        );
    }
}
