//! clap command-line definition (spec §11).

use clap::{Parser, Subcommand, ValueEnum};

use crate::event::{Agent, HookEventKind};

/// kioku — shared memory for AI coding agents.
#[derive(Debug, Parser)]
#[command(
    name = "kioku",
    version,
    about = "kioku (記憶): shared, Japanese-first memory for AI coding agents"
)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the data dir, config.toml (with a new auth token) and the wiki repository.
    Init {
        /// Only write a [client] section pointing at an existing server.
        #[arg(long, num_args = 2, value_names = ["URL", "TOKEN"])]
        client_only: Option<Vec<String>>,
    },
    /// Run the HTTP API + MCP server.
    Serve {
        /// Address to bind (overrides [server] bind / KIOKU_BIND).
        #[arg(long)]
        bind: Option<String>,
        /// Port to listen on (overrides [server] port / KIOKU_PORT).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Search the wiki through the server.
    Search {
        /// Query words (joined with spaces).
        #[arg(required = true)]
        query: Vec<String>,
        /// Restrict to a project (plus global pages).
        #[arg(long)]
        project: Option<String>,
        /// Search scope.
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Maximum number of hits.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Lifecycle hook handler (reads the agent's JSON payload on stdin).
    Hook {
        /// Which lifecycle event this invocation handles.
        #[arg(value_enum)]
        event: HookEventKind,
        /// Agent whose payload format is on stdin.
        #[arg(long, value_enum, default_value_t = Agent::ClaudeCode)]
        agent: Agent,
    },
    /// Captured hook payloads (`KIOKU_HOOK_DUMP=1` / `[client] hook_dump = true`).
    HookDump {
        /// Subcommand.
        #[command(subcommand)]
        command: HookDumpCommand,
    },
    /// Register kioku hooks, the MCP server and the instruction snippet with an agent.
    Install {
        /// Target agent, or `all` (every detected agent).
        #[arg(value_enum)]
        target: InstallTarget,
        /// Hooks and instructions in the current project instead of the user config (MCP
        /// stays user-level: the token never goes into a repository).
        #[arg(long)]
        project: bool,
        /// Do not write the instruction snippet.
        #[arg(long, conflicts_with = "instructions")]
        no_instructions: bool,
        /// Write the instruction snippet even where it is off by default (Claude Code).
        #[arg(long)]
        instructions: bool,
        /// Print what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// `all` only: restrict to these agents (comma-separated).
        #[arg(long, value_enum, value_delimiter = ',')]
        agents: Vec<Agent>,
        /// Codex: also set `[features] hooks = true` (older Codex builds).
        #[arg(long)]
        enable_hooks_feature: bool,
        /// Gemini CLI: register the MCP server with `"trust": true`.
        #[arg(long)]
        trust_mcp: bool,
    },
    /// Remove kioku hooks, the MCP server and the instruction snippet from an agent.
    Uninstall {
        /// Target agent, or `all` (every agent that has kioku entries).
        #[arg(value_enum)]
        target: InstallTarget,
        /// Remove the project's hooks and instructions instead of the user config's.
        #[arg(long)]
        project: bool,
        /// Print what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Project identity helpers.
    Project {
        /// Subcommand.
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Rebuild the search index from the wiki (via the server).
    Reindex,
    /// Show server status and counts.
    Status,
}

/// Agents `install` / `uninstall` know about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum InstallTarget {
    /// Claude Code: settings.json hooks + `mcpServers.kioku` in ~/.claude.json.
    ClaudeCode,
    /// Codex CLI: hooks.json + managed block in config.toml + AGENTS.md.
    Codex,
    /// Cursor: hooks.json + ~/.cursor/mcp.json (+ `.cursor/rules/kioku.mdc` with --project).
    Cursor,
    /// Gemini CLI: settings.json hooks + mcpServers + GEMINI.md.
    GeminiCli,
    /// Every detected agent.
    All,
}

impl InstallTarget {
    /// The single agent, or `None` for `all`.
    pub fn agent(self) -> Option<Agent> {
        match self {
            InstallTarget::ClaudeCode => Some(Agent::ClaudeCode),
            InstallTarget::Codex => Some(Agent::Codex),
            InstallTarget::Cursor => Some(Agent::Cursor),
            InstallTarget::GeminiCli => Some(Agent::GeminiCli),
            InstallTarget::All => None,
        }
    }
}

/// `kioku hook-dump …`.
#[derive(Debug, Subcommand)]
pub enum HookDumpCommand {
    /// Write the newest captured payload as `<out>/<agent>/<event>.captured.json`.
    Extract {
        /// Agent whose payload to extract.
        #[arg(value_enum)]
        agent: Agent,
        /// Neutral event (`post-tool-use`) or native name (`afterFileEdit`, `BeforeAgent`).
        event: String,
        /// Output directory (default: current directory).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

/// `kioku project …`.
#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Print the project id for a directory (default: current directory).
    Id {
        /// Directory to identify.
        path: Option<std::path::PathBuf>,
    },
}

/// `--scope` values of `kioku search`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    /// Everything.
    All,
    /// The given project plus global pages.
    Project,
    /// Only global pages.
    Global,
}

impl ScopeArg {
    /// Query-string value.
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeArg::All => "all",
            ScopeArg::Project => "project",
            ScopeArg::Global => "global",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent_and_parses_spec_forms() {
        Cli::command().debug_assert();
        let p = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("kioku").chain(args.iter().copied()))
        };
        assert!(matches!(
            p(&["init", "--client-only", "http://h:7391", "tok"]).unwrap().command,
            Command::Init { client_only: Some(v) } if v == ["http://h:7391", "tok"]
        ));
        assert!(p(&["init", "--client-only", "http://h:7391"]).is_err());
        assert!(matches!(
            p(&["hook", "post-tool-use"]).unwrap().command,
            Command::Hook {
                event: HookEventKind::PostToolUse,
                agent: Agent::ClaudeCode
            }
        ));
        assert!(p(&["hook", "stop", "--agent", "claude-code"]).is_ok());
        for (name, agent) in [
            ("codex", Agent::Codex),
            ("cursor", Agent::Cursor),
            ("gemini-cli", Agent::GeminiCli),
        ] {
            assert!(matches!(
                p(&["hook", "session-start", "--agent", name]).unwrap().command,
                Command::Hook { event: HookEventKind::SessionStart, agent: a } if a == agent
            ));
        }
        assert!(p(&["hook", "stop", "--agent", "gemini"]).is_err());
        assert!(matches!(
            p(&["hook-dump", "extract", "cursor", "afterFileEdit", "--out", "/tmp/f"]).unwrap().command,
            Command::HookDump { command: HookDumpCommand::Extract { agent: Agent::Cursor, event, out: Some(_) } }
                if event == "afterFileEdit"
        ));
        assert!(p(&["hook-dump", "extract", "codex"]).is_err());
        assert!(p(&["hook", "bogus"]).is_err());
        assert!(matches!(
            p(&["search", "引き継ぎ", "自動化", "--scope", "global", "--limit", "3"]).unwrap().command,
            Command::Search { query, scope: Some(ScopeArg::Global), limit: Some(3), .. } if query.len() == 2
        ));
        assert!(matches!(
            p(&["install", "claude-code", "--project"]).unwrap().command,
            Command::Install {
                target: InstallTarget::ClaudeCode,
                project: true,
                ..
            }
        ));
        assert!(matches!(
            p(&["install", "all", "--agents", "codex,gemini-cli", "--dry-run", "--no-instructions"]).unwrap().command,
            Command::Install { target: InstallTarget::All, agents, dry_run: true, no_instructions: true, .. }
                if agents == [Agent::Codex, Agent::GeminiCli]
        ));
        assert!(p(&["install", "codex", "--enable-hooks-feature"]).is_ok());
        assert!(p(&["install", "gemini-cli", "--trust-mcp", "--instructions"]).is_ok());
        assert!(p(&["install", "cursor", "--instructions", "--no-instructions"]).is_err());
        assert!(p(&["install", "all", "--agents", "gemini"]).is_err());
        assert!(p(&["install", "copilot"]).is_err());
        assert!(p(&["uninstall", "claude-code"]).is_ok());
        assert!(matches!(
            p(&["uninstall", "all", "--project"]).unwrap().command,
            Command::Uninstall {
                target: InstallTarget::All,
                project: true,
                dry_run: false
            }
        ));
        assert!(p(&["serve", "--bind", "0.0.0.0", "--port", "9000"]).is_ok());
        assert!(p(&["project", "id", "/tmp"]).is_ok());
        assert!(p(&["reindex"]).is_ok());
        assert!(p(&["status"]).is_ok());
    }
}
