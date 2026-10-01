//! `kioku` binary entry point; everything lives in the `kioku_cli` library.

use clap::Parser;

fn main() {
    // Before parsing: `kioku --version` is handled by clap and never reaches `run`.
    kioku_cli::update::remove_stale_old_binary();
    let cli = match kioku_cli::cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // SPEC-M2.7 §1: an agent reads exit 2 from a hook as "block"; a hook whose
            // arguments do not parse (an unknown --agent or event from a newer config) must
            // stay silent and succeed. Every other command keeps clap's behaviour.
            let argv: Vec<String> = std::env::args_os()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            if kioku_cli::commands::is_hook_invocation(&argv) && err.use_stderr() {
                kioku_cli::commands::log_hook_parse_failure(&err.to_string());
                std::process::exit(0);
            }
            err.exit();
        }
    };
    std::process::exit(kioku_cli::commands::run(cli));
}
