mod daemon;
mod db;
pub mod error;

use std::env;

fn main() {
    // Check if we should daemonize (set by spawn_daemon via env var)
    let is_background = env::var("NFLOW_DAEMON_MODE").as_deref() == Ok("background");

    if is_background {
        // Daemonize: setsid, redirect output, write PID file
        if let Err(e) = daemon::daemonize() {
            eprintln!("Failed to daemonize: {}", e);
            std::process::exit(1);
        }
    } else {
        // Foreground mode or direct invocation — just ensure dirs and write PID
        if let Err(e) = daemon::ensure_nflow_home() {
            eprintln!("Failed to create nflow home: {}", e);
            std::process::exit(1);
        }
    }

    println!("nflow-daemon started (pid: {})", std::process::id());
}
