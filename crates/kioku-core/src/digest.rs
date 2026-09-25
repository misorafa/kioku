//! Rule-based (zero-LLM) session digest (spec §7.2) and its auto-generated handoff section.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handoff::Handoff;
use crate::session::{Observation, ObservationKind};
use crate::strings::{Lang, fill, strings};
use crate::util::{one_line, parse_ts, truncate_chars};

/// Max chars kept per prompt.
pub const PROMPT_MAX: usize = 300;
/// Max chars kept per command line.
pub const COMMAND_MAX: usize = 160;
/// Max distinct commands kept.
pub const COMMANDS_MAX: usize = 30;
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

/// What happened in a session, derived purely from its observations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionDigest {
    /// Prompts in order, each truncated to 300 chars.
    pub prompts: Vec<String>,
    /// Edited files, most edits first.
    pub files: Vec<FileCount>,
    /// Read files, most reads first (top 20).
    pub reads: Vec<String>,
    /// First lines of Bash commands, deduplicated in first-seen order (last 30 kept).
    pub commands: Vec<String>,
    /// Messages of `git commit` commands.
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
}

impl SessionDigest {
    /// Builds a digest from observations (in seq order); paths are made relative to `root`.
    pub fn from_observations(observations: &[Observation], root: Option<&str>) -> SessionDigest {
        let mut d = SessionDigest::default();
        let mut edits: Vec<FileCount> = Vec::new();
        let mut reads: Vec<FileCount> = Vec::new();
        let mut commands: Vec<String> = Vec::new();

        for obs in observations {
            if d.first_ts.is_none() {
                d.first_ts = Some(obs.ts.clone());
            }
            d.last_ts = Some(obs.ts.clone());
            match obs.kind {
                ObservationKind::Prompt => {
                    d.prompt_count += 1;
                    let text = obs
                        .payload
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or(&obs.text);
                    d.prompts.push(truncate_chars(text.trim(), PROMPT_MAX));
                }
                ObservationKind::ToolUse => {
                    d.tool_use_count += 1;
                    let tool = obs
                        .payload
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let input = obs.payload.get("tool_input").unwrap_or(&Value::Null);
                    if EDIT_TOOLS.contains(&tool) {
                        for p in edit_paths(input) {
                            bump(&mut edits, &relative(&p, root));
                        }
                    } else if tool == "Read" {
                        if let Some(p) = input_path(input) {
                            bump(&mut reads, &relative(&p, root));
                        }
                    } else if tool == "Bash"
                        && let Some(cmd) = input.get("command").and_then(Value::as_str)
                    {
                        let first =
                            truncate_chars(cmd.lines().next().unwrap_or("").trim(), COMMAND_MAX);
                        if !first.is_empty() && !commands.contains(&first) {
                            commands.push(first);
                        }
                        if let Some(msg) = git_commit_message(cmd) {
                            d.git_commits.push(msg);
                        }
                    }
                    if obs
                        .payload
                        .get("tool_response")
                        .is_some_and(is_error_response)
                    {
                        d.errors += 1;
                    }
                }
                ObservationKind::Stop | ObservationKind::Compact | ObservationKind::Note => {}
            }
        }

        edits.sort_by_key(|f| std::cmp::Reverse(f.count));
        reads.sort_by_key(|f| std::cmp::Reverse(f.count));
        d.files = edits;
        d.reads = reads.into_iter().take(READS_MAX).map(|f| f.path).collect();
        let skip = commands.len().saturating_sub(COMMANDS_MAX);
        d.commands = commands.into_iter().skip(skip).collect();
        d.duration_secs = match (
            d.first_ts.as_deref().and_then(parse_ts),
            d.last_ts.as_deref().and_then(parse_ts),
        ) {
            (Some(a), Some(b)) => (b - a).num_seconds().max(0),
            _ => 0,
        };
        d
    }

    /// True when the session had at least one prompt or tool use (spec §7.1 step 2).
    pub fn is_substantive(&self) -> bool {
        self.prompt_count >= 1 || self.tool_use_count >= 1
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
        lines.push(s.auto_next_unknown.to_string());
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
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

/// Heuristic: `is_error == true`, or error text in a string response / `stderr` / `error`.
pub fn is_error_response(resp: &Value) -> bool {
    let has_marker = |s: &str| s.contains("error") || s.contains("Error:");
    match resp {
        Value::String(s) => has_marker(s),
        Value::Object(map) => {
            let flag = |k: &str| map.get(k).and_then(Value::as_bool).unwrap_or(false);
            if flag("is_error") || flag("isError") {
                return true;
            }
            let non_empty = |k: &str| match map.get(k) {
                Some(Value::String(s)) => !s.trim().is_empty(),
                Some(Value::Null) | None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => true,
            };
            if non_empty("error") {
                return true;
            }
            map.get("stderr")
                .and_then(Value::as_str)
                .is_some_and(has_marker)
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
        assert!(is_error_response(&json!({"stderr": "fatal error"})));
        assert!(is_error_response(&json!({"error": "denied"})));
        assert!(!is_error_response(&json!({"stdout": "ok", "stderr": ""})));
        assert!(!is_error_response(&json!({"content": "fn error() {}"})));
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
