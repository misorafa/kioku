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
    /// Register kioku hooks and the MCP server with an agent.
    Install {
        /// Target agent.
        #[command(subcommand)]
        target: InstallTarget,
    },
    /// Remove kioku hooks and the MCP server from an agent.
    Uninstall {
        /// Target agent.
        #[command(subcommand)]
        target: InstallTarget,
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
#[derive(Debug, Subcommand)]
pub enum InstallTarget {
    /// Claude Code: hooks in settings.json + `mcpServers.kioku` in ~/.claude.json.
    ClaudeCode {
        /// Use ./.claude/settings.json instead of ~/.claude/settings.json.
        #[arg(long)]
        project: bool,
    },
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
                target: InstallTarget::ClaudeCode { project: true }
            }
        ));
        assert!(p(&["uninstall", "claude-code"]).is_ok());
        assert!(p(&["serve", "--bind", "0.0.0.0", "--port", "9000"]).is_ok());
        assert!(p(&["project", "id", "/tmp"]).is_ok());
        assert!(p(&["reindex"]).is_ok());
        assert!(p(&["status"]).is_ok());
    }
}
