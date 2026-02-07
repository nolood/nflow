mod daemon;
mod db;
pub mod error;

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
        "foreground" => {
            // Foreground mode: write PID, bind socket, log to stderr, handle Ctrl+C
            let _shutdown = match daemon::foreground_mode() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to start foreground mode: {}", e);
                    std::process::exit(1);
                }
            };
            eprintln!(
                "nflow-daemon started in foreground mode (pid: {})",
                std::process::id()
            );

            // Main loop — wait for shutdown signal
            while !daemon::is_shutting_down() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            eprintln!("nflow-daemon shutting down gracefully...");

            // Cleanup: remove PID file
            if let Err(e) = daemon::remove_pid_file() {
                eprintln!("Warning: failed to remove PID file: {}", e);
            }
        }
        _ => {
            // Default: foreground mode (direct invocation without env var)
            let _shutdown = match daemon::foreground_mode() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to start daemon: {}", e);
                    std::process::exit(1);
                }
            };
            eprintln!("nflow-daemon started (pid: {})", std::process::id());

            while !daemon::is_shutting_down() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            eprintln!("nflow-daemon shutting down gracefully...");

            if let Err(e) = daemon::remove_pid_file() {
                eprintln!("Warning: failed to remove PID file: {}", e);
            }
        }
    }
}
