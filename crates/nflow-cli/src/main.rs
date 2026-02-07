pub mod cli;
pub mod daemon_client;
pub mod error;

use clap::Parser;
use cli::Cli;

fn main() {
    let _cli = Cli::parse();

    // Command dispatch will be implemented in US-070 (CLI socket client).
    // For now, parsing validates all commands and flags.
    println!("nflow: command parsed successfully");
}
