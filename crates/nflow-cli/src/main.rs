pub mod cli;
pub mod daemon_client;
pub mod error;
pub mod socket_client;

use clap::Parser;
use cli::Cli;

fn main() {
    let _cli = Cli::parse();

    // Command dispatch will be implemented in US-071/US-072.
    // For now, parsing validates all commands and flags.
    println!("nflow: command parsed successfully");
}
