//! Per-agent output rendering (M2 §3.6): handlers return a neutral [`HookResult`];
//! [`render`] turns it into what the agent expects on stdout / stderr / exit code.
//!
//! Invariants (tested): Gemini and Cursor stdout is always exactly one JSON object; Codex
//! never prints plain text on a Stop that exits 0; Cursor never exits 2.

use serde_json::{Value, json};

use crate::event::{Agent, HookEventKind};
use crate::hook::{HookOutcome, NUDGE_EXIT_CODE};

/// What a handler wants the agent to see, independent of the agent's wire format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookResult {
    /// Nothing to show.
    Silent,
    /// Context text for the model (the `<kioku>` block).
    Context(String),
    /// Stop nudge: make the agent continue with this message.
    Nudge(String),
}

/// Renders a handler result for an agent and event.
pub fn render(agent: Agent, event: HookEventKind, result: HookResult) -> HookOutcome {
    use HookEventKind::*;
    // A nudge only exists for Stop; anything else degrades to Silent.
    let result = match result {
        HookResult::Nudge(_) if event != Stop => HookResult::Silent,
        other => other,
    };
    match agent {
        Agent::ClaudeCode | Agent::Codex => match result {
            HookResult::Silent => HookOutcome::ok(),
            // Codex: plain text on a Stop that exits 0 is invalid (C1) — never print it.
            HookResult::Context(_) if agent == Agent::Codex && event == Stop => HookOutcome::ok(),
            HookResult::Context(t) => HookOutcome {
                stdout: t,
                ..HookOutcome::default()
            },
            HookResult::Nudge(m) => HookOutcome {
                stdout: String::new(),
                stderr: format!("{m}\n"),
                exit_code: NUDGE_EXIT_CODE,
            },
        },
        Agent::Cursor => {
            let body = match (event, result) {
                (Stop, HookResult::Nudge(m)) => json!({ "followup_message": m }),
                (SessionStart | PostToolUse, HookResult::Context(t)) => {
                    json!({ "additional_context": t })
                }
                // beforeSubmitPrompt cannot add context (U1); the block is delivered on the
                // next postToolUse instead (§5.6).
                (UserPromptSubmit, _) => json!({ "continue": true }),
                _ => json!({}),
            };
            json_outcome(body)
        }
        Agent::GeminiCli => {
            let body = match (event, result) {
                (Stop, HookResult::Nudge(m)) => json!({ "decision": "deny", "reason": m }),
                (SessionStart, HookResult::Context(t)) => json!({
                    "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": t }
                }),
                (UserPromptSubmit, HookResult::Context(t)) => json!({
                    "hookSpecificOutput": { "hookEventName": "BeforeAgent", "additionalContext": t }
                }),
                _ => json!({}),
            };
            json_outcome(body)
        }
    }
}

fn json_outcome(body: Value) -> HookOutcome {
    HookOutcome {
        stdout: format!("{body}\n"),
        stderr: String::new(),
        exit_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ALL_AGENTS, ALL_EVENTS};
    use HookEventKind::*;

    fn out(stdout: &str, stderr: &str, exit_code: i32) -> HookOutcome {
        HookOutcome {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    fn ctx() -> HookResult {
        HookResult::Context("<kioku>\n引き継ぎ\n</kioku>\n".into())
    }

    fn nudge() -> HookResult {
        HookResult::Nudge("kioku: 引き継ぎを書いて".into())
    }

    #[test]
    fn golden_claude_code() {
        let a = Agent::ClaudeCode;
        let block = "<kioku>\n引き継ぎ\n</kioku>\n";
        assert_eq!(render(a, SessionStart, ctx()), out(block, "", 0));
        assert_eq!(render(a, UserPromptSubmit, ctx()), out(block, "", 0));
        assert_eq!(
            render(a, Stop, nudge()),
            out("", "kioku: 引き継ぎを書いて\n", 2)
        );
        for e in ALL_EVENTS {
            assert_eq!(render(a, e, HookResult::Silent), HookOutcome::ok());
        }
    }

    #[test]
    fn golden_codex() {
        let a = Agent::Codex;
        let block = "<kioku>\n引き継ぎ\n</kioku>\n";
        assert_eq!(render(a, SessionStart, ctx()), out(block, "", 0));
        assert_eq!(render(a, UserPromptSubmit, ctx()), out(block, "", 0));
        assert_eq!(
            render(a, Stop, nudge()),
            out("", "kioku: 引き継ぎを書いて\n", 2)
        );
        for e in ALL_EVENTS {
            assert_eq!(render(a, e, HookResult::Silent), HookOutcome::ok());
        }
        // Stop never prints to stdout on exit 0
        assert_eq!(render(a, Stop, ctx()), HookOutcome::ok());
    }

    #[test]
    fn golden_cursor() {
        let a = Agent::Cursor;
        assert_eq!(
            render(a, SessionStart, ctx()),
            out(
                "{\"additional_context\":\"<kioku>\\n引き継ぎ\\n</kioku>\\n\"}\n",
                "",
                0
            )
        );
        assert_eq!(
            render(a, PostToolUse, ctx()),
            render(a, SessionStart, ctx())
        );
        assert_eq!(
            render(a, UserPromptSubmit, ctx()),
            out("{\"continue\":true}\n", "", 0)
        );
        assert_eq!(
            render(a, Stop, nudge()),
            out(
                "{\"followup_message\":\"kioku: 引き継ぎを書いて\"}\n",
                "",
                0
            )
        );
        assert_eq!(
            render(a, UserPromptSubmit, HookResult::Silent),
            out("{\"continue\":true}\n", "", 0)
        );
        for e in [SessionStart, PostToolUse, Stop, PreCompact, SessionEnd] {
            assert_eq!(render(a, e, HookResult::Silent), out("{}\n", "", 0));
        }
    }

    #[test]
    fn golden_gemini() {
        let a = Agent::GeminiCli;
        assert_eq!(
            render(a, SessionStart, ctx()),
            out(
                "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"<kioku>\\n引き継ぎ\\n</kioku>\\n\"}}\n",
                "",
                0
            )
        );
        assert_eq!(
            render(a, UserPromptSubmit, ctx()),
            out(
                "{\"hookSpecificOutput\":{\"hookEventName\":\"BeforeAgent\",\"additionalContext\":\"<kioku>\\n引き継ぎ\\n</kioku>\\n\"}}\n",
                "",
                0
            )
        );
        assert_eq!(
            render(a, Stop, nudge()),
            out(
                "{\"decision\":\"deny\",\"reason\":\"kioku: 引き継ぎを書いて\"}\n",
                "",
                0
            )
        );
        for e in ALL_EVENTS {
            assert_eq!(render(a, e, HookResult::Silent), out("{}\n", "", 0));
        }
    }

    #[test]
    fn invariants_over_every_cell() {
        for a in ALL_AGENTS {
            for e in ALL_EVENTS {
                for r in [HookResult::Silent, ctx(), nudge()] {
                    let o = render(a, e, r.clone());
                    match a {
                        Agent::Cursor | Agent::GeminiCli => {
                            let v: Value = serde_json::from_str(&o.stdout)
                                .unwrap_or_else(|_| panic!("{a:?} {e:?} {r:?}: {}", o.stdout));
                            assert!(v.is_object());
                            assert_eq!(o.exit_code, 0);
                            assert!(o.stderr.is_empty());
                        }
                        Agent::Codex if e == Stop && o.exit_code == 0 => {
                            assert!(o.stdout.is_empty());
                        }
                        _ => {}
                    }
                    if e != Stop {
                        assert_ne!(o.exit_code, 2, "{a:?} {e:?}: only Stop may exit 2");
                    }
                }
            }
        }
    }
}
