//! Every piece of generated human-readable text, in Japanese (default) and English.
//!
//! Templates use `{name}` placeholders filled with [`fill`].

use serde::{Deserialize, Serialize};

/// Language of generated summaries, handoffs and STATE.md (`[server] summary_lang`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Japanese (default).
    #[default]
    Ja,
    /// English.
    En,
}

/// The full set of generated strings for one language.
#[derive(Debug)]
pub struct Strings {
    /// Heading of the rule-generated handoff section.
    pub auto_handoff_heading: &'static str,
    /// Heading of the rule-generated addendum appended to a stale agent handoff.
    pub auto_handoff_delta_heading: &'static str,
    /// `{prompt}`: last user prompt line.
    pub auto_last_prompt: &'static str,
    /// `{files}`: touched files line.
    pub auto_files: &'static str,
    /// `{commands}`: executed commands line.
    pub auto_commands: &'static str,
    /// `{commits}`: git commits line.
    pub auto_commits: &'static str,
    /// `{n}`: error-count line.
    pub auto_errors: &'static str,
    /// Fixed "next steps unknown" line.
    pub auto_next_unknown: &'static str,
    /// `{agent}`, `{date}`: heading of an agent-written handoff.
    pub agent_handoff_heading: &'static str,
    /// Sub-heading: summary.
    pub handoff_summary: &'static str,
    /// Sub-heading: next steps.
    pub handoff_next_steps: &'static str,
    /// Sub-heading: open questions.
    pub handoff_open_questions: &'static str,
    /// Sub-heading: decisions.
    pub handoff_decisions: &'static str,
    /// Placeholder for an empty list or missing value.
    pub none: &'static str,
    /// Placeholder title when the session had no prompt.
    pub no_prompt: &'static str,
    /// Session page: overview heading.
    pub session_overview: &'static str,
    /// `{agent}`, `{start}`, `{end}`, `{prompts}`, `{tools}`: overview line.
    pub session_overview_line: &'static str,
    /// `{n}`: overview line about tool errors.
    pub session_errors_line: &'static str,
    /// Session page: prompts heading.
    pub session_prompts: &'static str,
    /// Session page: changed files heading.
    pub session_files: &'static str,
    /// Session page: commands heading.
    pub session_commands: &'static str,
    /// Session page: commits heading.
    pub session_commits: &'static str,
    /// `{name}`: STATE.md title.
    pub state_title: &'static str,
    /// STATE.md: latest handoff heading.
    pub state_latest_handoff: &'static str,
    /// `{date}`, `{agent}`, `{source}`: metadata line for the latest handoff.
    pub state_handoff_meta: &'static str,
    /// STATE.md: no handoff yet.
    pub state_no_handoff: &'static str,
    /// STATE.md: recent sessions heading.
    pub state_recent_sessions: &'static str,
    /// STATE.md: frequently touched files heading.
    pub state_hot_files: &'static str,
    /// `{name}`, `{id}`: SessionStart context: first line naming the project id.
    pub start_project_line: &'static str,
    /// `{id}`: SessionStart context: line naming the session id.
    pub start_session_line: &'static str,
    /// SessionStart context: heading of the pending handoff.
    pub start_handoff_heading: &'static str,
    /// SessionStart context: heading of the STATE.md excerpt.
    pub start_state_heading: &'static str,
    /// SessionStart context: closing instructions.
    pub start_footer: &'static str,
    /// `{project}`, `{session}`: Stop hook nudge paragraph (Claude Code, M1 §8.4).
    pub stop_nudge: &'static str,
    /// `{project}`, `{session}`: Stop nudge for the other agents (M2 §3.6) — no
    /// `stop_hook_active` reference.
    pub stop_nudge_generic: &'static str,
    /// `{project_line}`: body of the instruction snippet written into AGENTS.md / GEMINI.md /
    /// CLAUDE.md / `.cursor/rules/kioku.mdc` (M2 §7), without the block markers.
    pub instructions_body: &'static str,
    /// `{project_line}` of a global (user-level) snippet, which has no single project id.
    pub instructions_global_project: &'static str,
}

/// Japanese strings.
pub const JA: Strings = Strings {
    auto_handoff_heading: "## 引き継ぎ（自動生成）",
    auto_handoff_delta_heading: "## 引き継ぎ（自動生成・追記）",
    auto_last_prompt: "最後の指示: {prompt}",
    auto_files: "触ったファイル: {files}",
    auto_commands: "実行したコマンド: {commands}",
    auto_commits: "コミット: {commits}",
    auto_errors: "エラーの兆候: {n} 件のツール実行がエラーを返した",
    auto_next_unknown: "次にやること: （エージェントが明示的に書かなかったため不明。上記を手がかりに再開すること）",
    agent_handoff_heading: "## 引き継ぎ（{agent}, {date}）",
    handoff_summary: "### 要約",
    handoff_next_steps: "### 次にやること",
    handoff_open_questions: "### 未解決の質問",
    handoff_decisions: "### 決定事項",
    none: "（なし）",
    no_prompt: "（指示なし）",
    session_overview: "## 概要",
    session_overview_line: "- エージェント: {agent} / 開始: {start} / 終了: {end} / プロンプト {prompts} / ツール実行 {tools}",
    session_errors_line: "- エラーの兆候: {n} 件",
    session_prompts: "## 指示 (prompts)",
    session_files: "## 変更したファイル",
    session_commands: "## 実行したコマンド",
    session_commits: "## コミット",
    state_title: "{name} — 現在の状態",
    state_latest_handoff: "## 最新の引き継ぎ",
    state_handoff_meta: "_{date} / {agent} / source: {source}_",
    state_no_handoff: "（まだ引き継ぎはありません）",
    state_recent_sessions: "## 最近のセッション",
    state_hot_files: "## よく触るファイル（直近10セッション）",
    start_project_line: "project: {name} (id: {id})  ← kioku_* ツールの project 引数にはこの id を渡すこと",
    start_session_line: "session: {id}  ← kioku_handoff_write の session 引数にはこの id を渡すこと",
    start_handoff_heading: "## 前回からの引き継ぎ",
    start_state_heading: "## 現在の状態（STATE.md 抜粋）",
    start_footer: "セッション終了前に kioku_handoff_write（上の project と session を渡す）で要約・次の一手・未解決点を書くこと。\n関連する過去の記録は kioku_query で検索できる。",
    stop_nudge: "kioku: このセッションの引き継ぎがまだ書かれていないか、最後の引き継ぎ以降に作業が進んでいます。kioku_handoff_write（project={project}, session={session}）で 要約 / 次にやること / 未解決の質問 / 決定事項 を記録してから終了してください。記録済みなら stop_hook_active により再度この確認は出ません。",
    stop_nudge_generic: "kioku: このセッションの引き継ぎがまだ書かれていないか、最後の引き継ぎ以降に作業が進んでいます。kioku_handoff_write（project={project}, session={session}）で 要約 / 次にやること / 未解決の質問 / 決定事項 を記録してから終了してください。記録済みなら、そのまま終了してください。",
    instructions_body: "## kioku（共有メモリ）\n- kioku の MCP ツール（kioku_*）はこのマシンと他のエージェントで共有される記憶。\n- セッション開始時に `<kioku>` ブロックがあれば、その project / session を使う。無ければ作業前に\n  `kioku_read` で `<project>/STATE.md` を読む（project id: {project_line}）。\n- 調べる前に `kioku_query` で過去の記録を検索する。\n- タスクを終える前に必ず `kioku_handoff_write`（project, session, summary, next_steps,\n  open_questions, decisions）で引き継ぎを書く。session が分からなければ省略してよい。\n",
    instructions_global_project: "`kioku project id` の出力",
};

/// English strings.
pub const EN: Strings = Strings {
    auto_handoff_heading: "## Handoff (auto-generated)",
    auto_handoff_delta_heading: "## Handoff (auto-generated addendum)",
    auto_last_prompt: "Last instruction: {prompt}",
    auto_files: "Files touched: {files}",
    auto_commands: "Commands run: {commands}",
    auto_commits: "Commits: {commits}",
    auto_errors: "Error signals: {n} tool calls returned an error",
    auto_next_unknown: "Next steps: (unknown — the agent did not write them; resume from the clues above)",
    agent_handoff_heading: "## Handoff ({agent}, {date})",
    handoff_summary: "### Summary",
    handoff_next_steps: "### Next steps",
    handoff_open_questions: "### Open questions",
    handoff_decisions: "### Decisions",
    none: "(none)",
    no_prompt: "(no instruction)",
    session_overview: "## Overview",
    session_overview_line: "- Agent: {agent} / Started: {start} / Ended: {end} / Prompts {prompts} / Tool calls {tools}",
    session_errors_line: "- Error signals: {n}",
    session_prompts: "## Instructions (prompts)",
    session_files: "## Changed files",
    session_commands: "## Commands run",
    session_commits: "## Commits",
    state_title: "{name} — current state",
    state_latest_handoff: "## Latest handoff",
    state_handoff_meta: "_{date} / {agent} / source: {source}_",
    state_no_handoff: "(no handoff yet)",
    state_recent_sessions: "## Recent sessions",
    state_hot_files: "## Frequently touched files (last 10 sessions)",
    start_project_line: "project: {name} (id: {id})  ← pass this id as `project` to kioku_* tools",
    start_session_line: "session: {id}  ← pass this id as `session` to kioku_handoff_write",
    start_handoff_heading: "## Handoff from the previous session",
    start_state_heading: "## Current state (STATE.md excerpt)",
    start_footer: "Before ending the session, record a summary, next steps and open questions with kioku_handoff_write (pass the project and session above).\nSearch past records with kioku_query.",
    stop_nudge: "kioku: no handoff has been written for this session yet, or work continued after the last one. Record summary / next steps / open questions / decisions with kioku_handoff_write (project={project}, session={session}) before stopping. If you already did, stop_hook_active prevents this check from repeating.",
    stop_nudge_generic: "kioku: no handoff has been written for this session yet, or work continued after the last one. Record summary / next steps / open questions / decisions with kioku_handoff_write (project={project}, session={session}) before stopping. If you already did, just stop.",
    instructions_body: "## kioku (shared memory)\n- The kioku MCP tools (kioku_*) are memory shared by this machine and other agents.\n- If a `<kioku>` block was given at session start, use its project / session. Otherwise, before\n  working, read `<project>/STATE.md` with `kioku_read` (project id: {project_line}).\n- Search past records with `kioku_query` before investigating.\n- Before finishing a task, always write a handoff with `kioku_handoff_write` (project, session,\n  summary, next_steps, open_questions, decisions). Omit session if you do not know it.\n",
    instructions_global_project: "the output of `kioku project id`",
};

/// Returns the string table for a language.
pub fn strings(lang: Lang) -> &'static Strings {
    match lang {
        Lang::Ja => &JA,
        Lang::En => &EN,
    }
}

/// Replaces each `{key}` in `template` with its value.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_replaces_all_placeholders() {
        let s = fill(JA.auto_errors, &[("n", "3")]);
        assert_eq!(s, "エラーの兆候: 3 件のツール実行がエラーを返した");
        let s = fill(
            EN.agent_handoff_heading,
            &[("agent", "codex"), ("date", "2026-09-25")],
        );
        assert_eq!(s, "## Handoff (codex, 2026-09-25)");
    }

    #[test]
    fn generic_nudge_differs_only_in_the_last_sentence() {
        for (t, tail_m1, tail) in [
            (
                &JA,
                "記録済みなら stop_hook_active により再度この確認は出ません。",
                "記録済みなら、そのまま終了してください。",
            ),
            (
                &EN,
                "If you already did, stop_hook_active prevents this check from repeating.",
                "If you already did, just stop.",
            ),
        ] {
            assert!(t.stop_nudge_generic.ends_with(tail));
            assert!(!t.stop_nudge_generic.contains("stop_hook_active"));
            assert_eq!(
                t.stop_nudge.strip_suffix(tail_m1),
                t.stop_nudge_generic.strip_suffix(tail)
            );
        }
    }
}
