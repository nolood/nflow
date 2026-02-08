#[cfg(test)]
mod commands_exec_tests;
#[cfg(test)]
mod commands_plan_tests;
#[cfg(test)]
mod commands_project_spec_tests;
mod daemon;
mod db;
#[cfg(test)]
mod e2e_tests;
pub mod error;
pub mod events;
pub mod handlers;
#[cfg(test)]
mod lifecycle_tests;
pub mod platform;
#[cfg(test)]
mod protocol_tests;
mod recovery;
mod scheduler_loop;
mod shutdown;
pub mod socket;

use std::env;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info};

use handlers::{create_handler, HandlerState};
use socket::SocketServerConfig;

#[tokio::main]
async fn main() {
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

            // --- Database migrations ---
            let db_path = match daemon::db_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to resolve db path: {}", e);
                    std::process::exit(1);
                }
            };

            {
                let conn = match db::open_connection(&db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to open database: {}", e);
                        std::process::exit(1);
                    }
                };
                match db::run_migrations(&conn, &db_path) {
                    Ok(version) => {
                        eprintln!("database schema at version {}", version);
                    }
                    Err(e) => {
                        eprintln!("Failed to run migrations: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            // --- Crash recovery ---
            let socket_path = match daemon::socket_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to resolve socket path: {}", e);
                    std::process::exit(1);
                }
            };
            let pid_file = match daemon::pid_file_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Failed to resolve pid file path: {}", e);
                    std::process::exit(1);
                }
            };

            {
                let conn = match db::open_connection(&db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to open database for recovery: {}", e);
                        std::process::exit(1);
                    }
                };
                match recovery::recover_session_state(&conn, &socket_path, &pid_file) {
                    Ok(report) => {
                        info!(
                            specs_reset = report.specs_reset,
                            tasks_failed = report.tasks_failed,
                            stories_needing_completion = report.stories_needing_completion.len(),
                            agents_adopted = report.agents_adopted.len(),
                            socket_removed = report.socket_removed,
                            pid_file_removed = report.pid_file_removed,
                            "crash recovery complete"
                        );
                    }
                    Err(e) => {
                        eprintln!("Warning: crash recovery failed: {}", e);
                        // Non-fatal — continue startup
                    }
                }
            }

            // --- Socket server startup ---

            let state = Arc::new(HandlerState {
                db_path: db_path.clone(),
            });
            let handler = create_handler(state);
            let config = SocketServerConfig {
                socket_path: socket_path.clone(),
                handler,
                event_bus: None,
            };
            let _server_handle = match socket::start_server(config) {
                Ok(handle) => handle,
                Err(e) => {
                    eprintln!("Failed to start socket server: {}", e);
                    std::process::exit(1);
                }
            };
            eprintln!("socket server listening on {}", socket_path.display());

            // --- Scheduler loop on the main task ---
            // Ticks every 2 seconds, serialized with command handling.
            // Skips tick when shutting_down flag is set.
            let mut interval = tokio::time::interval(Duration::from_secs(2));

            loop {
                interval.tick().await;

                if daemon::is_shutting_down() {
                    break;
                }

                // Open DB connection for this tick
                let db_path = match daemon::db_path() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                let conn = match db::open_connection(&db_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                // Run scheduler tick with reaping (synchronous — serialized on main task)
                let (actions, progress_actions) = scheduler_loop::scheduler_tick(&conn, None);

                // Execute story progress actions first (start next tasks from reaping)
                if !progress_actions.is_empty() {
                    debug!(
                        "scheduler: {} story progress actions to execute",
                        progress_actions.len()
                    );
                    scheduler_loop::execute_story_progress_actions(&conn, &progress_actions, None)
                        .await;
                }

                if !actions.is_empty() {
                    debug!("scheduler: {} actions produced this tick", actions.len());
                    // Execute actions: update DB, create worktrees, start tasks
                    scheduler_loop::execute_actions(&conn, &actions, None).await;
                }
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
                        "shutdown complete: {} agents finished naturally, {} sigtermed, {} sigkilled, socket_removed={}, pid_file_removed={}",
                        report.agents_finished_naturally,
                        report.agents_sigtermed,
                        report.agents_sigkilled,
                        report.socket_removed,
                        report.pid_file_removed,
                    );
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
