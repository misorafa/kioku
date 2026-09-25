//! `kioku` binary entry point; everything lives in the `kioku_cli` library.

use clap::Parser;

fn main() {
    let cli = kioku_cli::cli::Cli::parse();
    std::process::exit(kioku_cli::commands::run(cli));
}
