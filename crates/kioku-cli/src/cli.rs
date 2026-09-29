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
        /// Write the log to this file (rotated at 10 MiB, keeps .1-.3) instead of stderr.
        #[arg(long)]
        log_file: Option<std::path::PathBuf>,
    },
    /// Set up this machine in one idempotent step: config, background service, hooks + MCP
    /// for every detected agent, summary.
    Setup {
        /// Client-only machine: talk to the server at URL with TOKEN (checked before anything
        /// is written).
        #[arg(long, num_args = 2, value_names = ["URL", "TOKEN"])]
        client_only: Option<Vec<String>>,
        /// Do not install the background service.
        #[arg(long)]
        no_service: bool,
        /// Do not install hooks / MCP for any agent.
        #[arg(long)]
        no_agents: bool,
        /// Only these agents (comma-separated).
        #[arg(long, value_enum, value_delimiter = ',')]
        agents: Vec<Agent>,
        /// `[server] bind` for a new config (e.g. 0.0.0.0 on a home server).
        #[arg(long, conflicts_with = "client_only")]
        bind: Option<String>,
        /// Do not write the instruction snippets (AGENTS.md, GEMINI.md).
        #[arg(long)]
        no_instructions: bool,
        /// Print the plan; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Register the server URL + token as the agents' MCP server (v0.3 form) instead of
        /// the `kioku mcp` stdio bridge.
        #[arg(long)]
        mcp_http: bool,
        /// Also print the manual command for other machines (`curl -fsSL
        /// https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s --
        /// --client-only <url> <token>`); it contains the token. `kioku invite` is easier.
        #[arg(long)]
        print_client_command: bool,
    },
    /// On the server machine: print one line to paste on a new machine (installs kioku there
    /// and joins this server; valid 10 minutes, once).
    Invite {
        /// Minutes the line stays valid (at most 60).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
        ttl: u32,
        /// How many machines may use it (at most 20).
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        uses: u32,
    },
    /// Join the server with a code from `kioku invite`: fetch its token, write a client-only
    /// config and set up every detected agent (what the pasted invite line runs).
    Join {
        /// Server URL (`http://<host>:<port>`).
        url: String,
        /// Invite code.
        code: String,
        /// Only these agents (comma-separated).
        #[arg(long, value_enum, value_delimiter = ',')]
        agents: Vec<Agent>,
        /// Do not install hooks / MCP for any agent.
        #[arg(long)]
        no_agents: bool,
        /// Do not write the instruction snippets (AGENTS.md, GEMINI.md).
        #[arg(long)]
        no_instructions: bool,
        /// Register the server URL + token as the agents' MCP server instead of `kioku mcp`.
        #[arg(long)]
        mcp_http: bool,
    },
    /// Manage the user-level background service (launchd / systemd --user) running `kioku serve`.
    Service {
        /// Subcommand.
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Check this machine's kioku setup (config, server, service, every agent's hooks and MCP).
    Doctor {
        /// Machine-readable output: {"checks":[{id, status, message, fix?}]}.
        #[arg(long)]
        json: bool,
        /// Only check this agent (other agents are skipped).
        #[arg(long, value_enum)]
        agent: Option<Agent>,
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
        /// Register the server URL + token as the MCP server (v0.3 form) instead of the
        /// `kioku mcp` stdio bridge.
        #[arg(long)]
        mcp_http: bool,
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
    /// Replace this binary with a release (SHA-256 verified) and restart the service.
    Update {
        /// Release tag to install (default: the latest release).
        #[arg(long)]
        version: Option<String>,
        /// Only compare with the latest release; exit 10 when an update is available.
        #[arg(long)]
        check: bool,
    },
    /// Rebuild the search index from the wiki (via the server).
    Reindex,
    /// Show server status and counts.
    Status,
    /// Replace the server's auth token (run on the server machine), restart the service and
    /// print the command for the other machines.
    RotateToken {
        /// Show what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Serve the kioku MCP tools on stdin/stdout for an agent, relaying to the server in
    /// [client] (what `kioku install` registers as the agents' MCP server).
    Mcp,
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
    /// Antigravity CLI (`agy`): ~/.gemini/config/hooks.json + mcp_config.json + GEMINI.md.
    Antigravity,
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
            InstallTarget::Antigravity => Some(Agent::Antigravity),
            InstallTarget::All => None,
        }
    }
}

/// `kioku service …`.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Write the definition, enable and start it (idempotent).
    Install,
    /// Stop, disable and remove the definition.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Installed? active? pid; server health.
    Status,
    /// Print the end of serve.log.
    Logs {
        /// Keep printing new lines.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of lines.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
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
    /// Fold project <FROM> into <INTO> on the server: sessions, handoffs and pages move,
    /// <FROM> becomes an alias of <INTO> (M2.4 §2.3).
    Merge {
        /// Project id to fold in (it stops existing; its id keeps working as an alias).
        from: String,
        /// Project id that receives everything.
        into: String,
        /// Only list what would move.
        #[arg(long)]
        dry_run: bool,
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
        assert!(matches!(
            p(&["serve", "--log-file", "/tmp/s.log"]).unwrap().command,
            Command::Serve {
                log_file: Some(_),
                ..
            }
        ));
        assert!(matches!(
            p(&["setup", "--client-only", "http://h:7391", "tok", "--agents", "codex,cursor", "--no-instructions", "--dry-run", "--print-client-command"]).unwrap().command,
            Command::Setup { client_only: Some(v), agents, no_instructions: true, dry_run: true, print_client_command: true, .. }
                if v == ["http://h:7391", "tok"] && agents == [Agent::Codex, Agent::Cursor]
        ));
        assert!(matches!(
            p(&["setup", "--no-service", "--no-agents", "--bind", "0.0.0.0"]).unwrap().command,
            Command::Setup { no_service: true, no_agents: true, bind: Some(b), .. } if b == "0.0.0.0"
        ));
        assert!(
            p(&[
                "setup",
                "--client-only",
                "http://h:7391",
                "tok",
                "--bind",
                "0.0.0.0"
            ])
            .is_err()
        );
        for sub in ["install", "uninstall", "start", "stop", "status", "logs"] {
            assert!(p(&["service", sub]).is_ok(), "{sub}");
        }
        assert!(matches!(
            p(&["service", "logs", "-f", "-n", "50"]).unwrap().command,
            Command::Service {
                command: ServiceCommand::Logs {
                    follow: true,
                    lines: 50
                }
            }
        ));
        assert!(matches!(
            p(&["service", "logs"]).unwrap().command,
            Command::Service {
                command: ServiceCommand::Logs {
                    follow: false,
                    lines: 200
                }
            }
        ));
        assert!(matches!(
            p(&["doctor", "--json", "--agent", "gemini-cli"])
                .unwrap()
                .command,
            Command::Doctor {
                json: true,
                agent: Some(Agent::GeminiCli)
            }
        ));
        assert!(p(&["doctor", "--agent", "all"]).is_err());
        assert!(p(&["project", "id", "/tmp"]).is_ok());
        assert!(matches!(
            p(&[
                "project",
                "merge",
                "kioku-71002b89",
                "ai-agents-shared-memory-02036d30",
                "--dry-run"
            ])
            .unwrap()
            .command,
            Command::Project {
                command: ProjectCommand::Merge { dry_run: true, .. }
            }
        ));
        assert!(p(&["project", "merge", "only-one"]).is_err());
        assert!(matches!(
            p(&["update", "--version", "v0.2.0"]).unwrap().command,
            Command::Update { version: Some(v), check: false } if v == "v0.2.0"
        ));
        assert!(matches!(
            p(&["update", "--check"]).unwrap().command,
            Command::Update {
                version: None,
                check: true
            }
        ));
        assert!(p(&["reindex"]).is_ok());
        assert!(matches!(
            p(&["invite"]).unwrap().command,
            Command::Invite { ttl: 10, uses: 1 }
        ));
        assert!(matches!(
            p(&["invite", "--ttl", "30", "--uses", "3"])
                .unwrap()
                .command,
            Command::Invite { ttl: 30, uses: 3 }
        ));
        assert!(p(&["invite", "--uses", "0"]).is_err());
        assert!(matches!(
            p(&["join", "http://192.168.1.240:7391", "K7Q2M9XD", "--agents", "codex"]).unwrap().command,
            Command::Join { url, code, agents, .. }
                if url == "http://192.168.1.240:7391" && code == "K7Q2M9XD" && agents == [Agent::Codex]
        ));
        assert!(p(&["join", "http://h:7391"]).is_err());
        assert!(p(&["status"]).is_ok());
    }
}
