//! `kioku` binary entry point; everything lives in the `kioku_cli` library.

use clap::Parser;

fn main() {
    // Before parsing: `kioku --version` is handled by clap and never reaches `run`.
    kioku_cli::update::remove_stale_old_binary();
    let cli = kioku_cli::cli::Cli::parse();
    std::process::exit(kioku_cli::commands::run(cli));
}
