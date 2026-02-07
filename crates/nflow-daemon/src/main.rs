mod daemon;
mod db;
pub mod error;
pub mod events;
mod recovery;
mod shutdown;
pub mod socket;

use std::env;

fn main() {
    let mode = env::var("NFLOW_DAEMON_MODE")
        .unwrap_or_default()
        .to_lowercase();

    match mode.as_str() {
        "background" => {
            // Daemonize: setsid, redirect output, write PID file
            if let Err(e) = daemon::daemonize() {
                eprintln!("Failed to daemonize: {}", e);
                std::process::exit(1);
            }
            eprintln!(
                "nflow-daemon started in background mode (pid: {})",
                std::process::id()
            );
        }
        "foreground" | _ => {
            // Foreground mode: write PID, bind socket, log to stderr, handle Ctrl+C
            let label = if mode == "foreground" {
                "foreground mode"
            } else {
                "default mode"
            };
            let _shutdown = match daemon::foreground_mode() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to start {}: {}", label, e);
                    std::process::exit(1);
                }
            };
            eprintln!(
                "nflow-daemon started in {} (pid: {})",
                label,
                std::process::id()
            );

            // Main loop — wait for shutdown signal
            // When shutting_down is set, the scheduler stops and no new
            // connections are accepted.
            while !daemon::is_shutting_down() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            eprintln!("nflow-daemon shutting down gracefully...");

            // Run the full graceful shutdown sequence:
            // - Wait for agents (60s) → SIGTERM (10s) → SIGKILL
            // - Remove socket and PID files
            let socket_path = daemon::socket_path().unwrap_or_default();
            let pid_file = daemon::pid_file_path().unwrap_or_default();

            // Open a DB connection for reading running agents
            let db_path = daemon::db_path().unwrap_or_default();
            let conn = db::open_connection(&db_path);
            match conn {
                Ok(conn) => {
                    let report = shutdown::graceful_shutdown(&conn, &socket_path, &pid_file);
                    eprintln!(
                        "shutdown complete: {} agents finished naturally, {} sigtermed, {} sigkilled",
                        report.agents_finished_naturally,
                        report.agents_sigtermed,
                        report.agents_sigkilled
                    );
                    // Connection is dropped here, closing the database
                }
                Err(e) => {
                    eprintln!("Warning: could not open database for shutdown: {}", e);
                    // Still clean up files even without DB access
                    shutdown::remove_socket_file(&socket_path);
                    shutdown::remove_pid_file(&pid_file);
                }
            }
        }
    }
}
