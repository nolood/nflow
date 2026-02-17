pub mod cli;
pub mod daemon_client;
pub mod error;
pub mod format;
pub mod socket_client;
pub mod streaming;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use cli::{Cli, Commands, DaemonCommand, PipelineCommand, SpecCommand};
use daemon_client::{daemon_status, daemon_stop, ensure_daemon};
use error::CliError;
use socket_client::{ResponseStatus, SocketClient};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    format::init(cli.no_color);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", format::format_error(&e.to_string()));
            e.exit_code()
        }
    }
}

async fn run(cli: Cli) -> error::Result<()> {
    // Set up Ctrl+C handler
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_flag = cancelled.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_flag.store(true, Ordering::Relaxed);
        }
    });

    match cli.command {
        // --- Daemon commands (local, no socket needed) ---
        Commands::Daemon(DaemonCommand::Start { foreground }) => {
            if foreground {
                // Foreground mode: exec into nflow-daemon, replacing this process
                use std::os::unix::process::CommandExt;
                let daemon_binary = daemon_client::find_daemon_binary()?;
                let err = std::process::Command::new(&daemon_binary)
                    .env("NFLOW_DAEMON_MODE", "foreground")
                    .exec();
                // exec() only returns on error
                Err(CliError::Io(std::io::Error::other(format!(
                    "failed to exec {}: {}",
                    daemon_binary.display(),
                    err
                ))))
            } else {
                ensure_daemon()?;
                println!("daemon started");
                Ok(())
            }
        }
        Commands::Daemon(DaemonCommand::Stop) => {
            let pid = daemon_stop()?;
            println!("daemon stopped (pid: {})", pid);
            Ok(())
        }
        Commands::Daemon(DaemonCommand::Status) => {
            let status = daemon_status()?;
            println!("{}", status);
            Ok(())
        }

        // --- Streaming: spec new ---
        Commands::Spec(SpecCommand::New {
            name,
            with_codebase,
            non_interactive,
        }) => {
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;

            let project = resolve_project(&cli.project)?;
            let params = serde_json::json!({
                "project_name": project,
                "spec_name": name,
                "with_codebase": with_codebase,
            });

            streaming::handle_spec_dialogue(
                &mut client,
                "spec.new",
                params,
                cancelled,
                non_interactive,
            )
            .await
        }

        // --- Streaming: spec resume ---
        Commands::Spec(SpecCommand::Resume { name }) => {
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;

            let project = resolve_project(&cli.project)?;
            let mut params = serde_json::json!({
                "project_name": project,
            });
            if let Some(n) = name {
                params["spec_name"] = serde_json::Value::String(n);
            }

            streaming::handle_spec_dialogue(&mut client, "spec.resume", params, cancelled, false)
                .await
        }

        // --- Streaming: pipeline start ---
        Commands::Pipeline(PipelineCommand::Start {
            description,
            mode,
            max_iterations,
        }) => {
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;
            let project = resolve_project(&cli.project)?;
            let params = serde_json::json!({
                "project_name": project,
                "name": description,
                "goal": description,
                "mode": mode,
                "max_iterations": max_iterations,
            });
            streaming::handle_pipeline_stream(&mut client, params, cancelled).await
        }

        // --- Streaming: log follow ---
        Commands::Log {
            task_id,
            follow: true,
        } => {
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;
            streaming::handle_log_follow(&mut client, &task_id, cancelled).await
        }

        // --- Non-streaming: log (no follow) ---
        Commands::Log {
            task_id,
            follow: false,
        } => {
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;
            let params = serde_json::json!({
                "task_id": task_id,
                "follow": false,
            });
            let resp = client.send_command("exec.log", params).await?;
            handle_response(resp, cli.json, "exec.log")
        }

        // --- All other commands: send to daemon and print response ---
        other => {
            let (command, params) = build_command(&cli.project, other)?;
            ensure_daemon()?;
            let mut client = SocketClient::connect().await?;
            let resp = client.send_command(&command, params).await?;
            handle_response(resp, cli.json, &command)
        }
    }
}

/// Handle a single response from the daemon.
fn handle_response(
    resp: socket_client::Response,
    json_mode: bool,
    command: &str,
) -> error::Result<()> {
    match resp.status {
        ResponseStatus::Ok => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp.data).unwrap_or_default()
                );
            } else {
                format::print_response(&resp.data, command);
            }
            Ok(())
        }
        ResponseStatus::Error => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error from daemon");
            Err(CliError::Socket(msg.to_string()))
        }
    }
}

/// Resolve the project name from CLI flag or auto-detection.
fn resolve_project(project_flag: &Option<String>) -> error::Result<String> {
    if let Some(name) = project_flag {
        return Ok(name.clone());
    }
    // Auto-detect from current directory name
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            "failed to determine current directory",
        ))
    })?;
    let dir_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    Ok(dir_name.to_string())
}

/// Build the daemon command string and params from the parsed CLI command.
///
/// Returns (command_name, params) for commands that use the standard
/// request/response flow (non-streaming).
fn build_command(
    project_flag: &Option<String>,
    command: Commands,
) -> error::Result<(String, serde_json::Value)> {
    let project = resolve_project(project_flag)?;

    match command {
        Commands::Init {
            name,
            base_branch,
            git_provider,
        } => {
            let cwd = std::env::current_dir()
                .map_err(CliError::Io)?
                .to_string_lossy()
                .to_string();
            let mut params = serde_json::json!({
                "name": name,
                "path": cwd,
                "base_branch": base_branch,
            });
            if let Some(gp) = git_provider {
                params["git_provider"] = serde_json::Value::String(gp);
            }
            Ok(("project.init".to_string(), params))
        }

        Commands::Projects(cli::ProjectsCommand::List) => {
            Ok(("project.list".to_string(), serde_json::json!({})))
        }

        Commands::Project(cli::ProjectCommand::Delete { name, force }) => Ok((
            "project.delete".to_string(),
            serde_json::json!({
                "name": name,
                "force": force,
            }),
        )),

        Commands::Spec(SpecCommand::List) => Ok((
            "spec.list".to_string(),
            serde_json::json!({ "project_name": project }),
        )),

        Commands::Spec(SpecCommand::View { name }) => Ok((
            "spec.view".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
            }),
        )),

        Commands::Spec(SpecCommand::Approve { name }) => Ok((
            "spec.approve".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
            }),
        )),

        Commands::Spec(SpecCommand::Reopen { name }) => Ok((
            "spec.reopen".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
            }),
        )),

        Commands::Spec(SpecCommand::Delete { name, force }) => Ok((
            "spec.delete".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
                "force": force,
            }),
        )),

        Commands::Spec(SpecCommand::Questions { name }) => Ok((
            "spec.questions".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
            }),
        )),

        Commands::Spec(SpecCommand::AnswerQuestion {
            name,
            question,
            answer,
        }) => Ok((
            "spec.answer_question".to_string(),
            serde_json::json!({
                "project_name": project,
                "spec_name": name,
                "question_id": question,
                "answer": answer,
            }),
        )),

        Commands::Plan(cli::PlanCommand::Generate {
            specs,
            with_codebase,
        }) => {
            let mut params = serde_json::json!({
                "project_name": project,
                "with_codebase": with_codebase,
            });
            if let Some(s) = specs {
                params["specs"] = serde_json::Value::String(s);
            }
            Ok(("plan.generate".to_string(), params))
        }

        Commands::Plan(cli::PlanCommand::Show { wave, dag }) => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            if dag {
                params["dag"] = serde_json::json!(true);
            }
            Ok(("plan.show".to_string(), params))
        }

        Commands::Plan(cli::PlanCommand::Feedback { message, wave }) => {
            let mut params = serde_json::json!({
                "project_name": project,
                "message": message,
            });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            Ok(("plan.feedback".to_string(), params))
        }

        Commands::Plan(cli::PlanCommand::Approve { wave }) => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            Ok(("plan.approve".to_string(), params))
        }

        Commands::Plan(cli::PlanCommand::Discard { wave }) => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            Ok(("plan.discard".to_string(), params))
        }

        Commands::Run {
            parallel,
            story,
            dry_run,
        } => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(p) = parallel {
                params["max_parallel"] = serde_json::json!(p);
            }
            if let Some(s) = story {
                params["story_id"] = serde_json::Value::String(s);
            }
            if dry_run {
                params["dry_run"] = serde_json::json!(true);
            }
            Ok(("exec.run".to_string(), params))
        }

        Commands::Pause => Ok((
            "exec.pause".to_string(),
            serde_json::json!({ "project_name": project }),
        )),

        Commands::Status { wave } => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            Ok(("exec.status".to_string(), params))
        }

        Commands::Retry { task_id } => Ok((
            "exec.retry".to_string(),
            serde_json::json!({
                "project_name": project,
                "task_id": task_id,
            }),
        )),

        Commands::Skip { task_id } => Ok((
            "exec.skip".to_string(),
            serde_json::json!({
                "project_name": project,
                "task_id": task_id,
            }),
        )),

        Commands::Continue { story_id, force } => Ok((
            "exec.continue".to_string(),
            serde_json::json!({
                "project_name": project,
                "story_id": story_id,
                "force": force,
            }),
        )),

        Commands::Stop {
            story_id,
            wave,
            all,
        } => {
            let mut params = serde_json::json!({ "project_name": project });
            if let Some(s) = story_id {
                params["story_id"] = serde_json::Value::String(s);
            }
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            if all {
                params["all"] = serde_json::json!(true);
            }
            Ok(("exec.stop".to_string(), params))
        }

        Commands::Cancel { story_id, wave } => {
            let mut params = serde_json::json!({
                "project_name": project,
                "story_id": story_id,
            });
            if let Some(w) = wave {
                params["wave"] = serde_json::json!(w);
            }
            Ok(("exec.cancel".to_string(), params))
        }

        Commands::Worktree(cli::WorktreeCommand::List) => Ok((
            "worktree.list".to_string(),
            serde_json::json!({ "project": project }),
        )),

        Commands::Worktree(cli::WorktreeCommand::Clean { all }) => Ok((
            "worktree.clean".to_string(),
            serde_json::json!({
                "project": project,
                "all": all,
            }),
        )),

        Commands::Cleanup {
            logs,
            all,
            older_than,
            dry_run,
        } => {
            let mut params = serde_json::json!({
                "project": project,
                "logs": logs,
                "all": all,
                "dry_run": dry_run,
            });
            if let Some(age) = older_than {
                params["older_than"] = serde_json::Value::String(age);
            }
            Ok(("cleanup.logs".to_string(), params))
        }

        Commands::Config(cli::ConfigCommand::Show) => {
            let env_overrides: serde_json::Map<String, serde_json::Value> = std::env::vars()
                .filter(|(k, _)| k.starts_with("NFLOW_"))
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            let mut params = serde_json::json!({ "project_name": project });
            if !env_overrides.is_empty() {
                params["env_overrides"] = serde_json::Value::Object(env_overrides);
            }
            Ok(("config.show".to_string(), params))
        }

        Commands::Config(cli::ConfigCommand::Set { key, value }) => {
            let json_value: serde_json::Value = if let Ok(n) = value.parse::<i64>() {
                serde_json::json!(n)
            } else if value == "true" {
                serde_json::json!(true)
            } else if value == "false" {
                serde_json::json!(false)
            } else {
                serde_json::json!(value)
            };
            Ok((
                "config.set".to_string(),
                serde_json::json!({
                    "project_name": project,
                    "key": key,
                    "value": json_value,
                }),
            ))
        }

        Commands::Pipeline(PipelineCommand::List) => Ok((
            "pipeline.list".to_string(),
            serde_json::json!({ "project_name": project }),
        )),

        Commands::Pipeline(PipelineCommand::Status { pipeline_id }) => Ok((
            "pipeline.status".to_string(),
            serde_json::json!({ "pipeline_run_id": pipeline_id }),
        )),

        Commands::Pipeline(PipelineCommand::Cancel { pipeline_id }) => Ok((
            "pipeline.cancel".to_string(),
            serde_json::json!({ "pipeline_run_id": pipeline_id }),
        )),

        Commands::Pipeline(PipelineCommand::Log {
            pipeline_id,
            stage,
            iteration,
        }) => {
            let mut params = serde_json::json!({ "pipeline_run_id": pipeline_id });
            if let Some(s) = stage {
                params["stage_type"] = serde_json::Value::String(s);
            }
            if let Some(i) = iteration {
                params["iteration"] = serde_json::json!(i);
            }
            Ok(("pipeline.log".to_string(), params))
        }

        Commands::Pipeline(PipelineCommand::Approve { pipeline_id }) => Ok((
            "pipeline.approve".to_string(),
            serde_json::json!({ "pipeline_run_id": pipeline_id }),
        )),

        Commands::Pipeline(PipelineCommand::Reject {
            pipeline_id,
            feedback,
        }) => Ok((
            "pipeline.reject".to_string(),
            serde_json::json!({
                "pipeline_run_id": pipeline_id,
                "feedback": feedback,
            }),
        )),

        Commands::Pipeline(PipelineCommand::Answer {
            pipeline_id,
            question_id,
            answer,
        }) => Ok((
            "pipeline.answer".to_string(),
            serde_json::json!({
                "pipeline_run_id": pipeline_id,
                "question_id": question_id,
                "answer": answer,
            }),
        )),

        Commands::Pipeline(PipelineCommand::Questions { pipeline_id }) => Ok((
            "pipeline.questions".to_string(),
            serde_json::json!({ "pipeline_run_id": pipeline_id }),
        )),

        Commands::Tui => {
            // TUI launch is handled separately — not a daemon command
            Err(CliError::Socket("TUI is not yet implemented".to_string()))
        }

        // Streaming commands are handled in run() before reaching build_command
        Commands::Daemon(_)
        | Commands::Spec(SpecCommand::New { .. })
        | Commands::Spec(SpecCommand::Resume { .. })
        | Commands::Pipeline(PipelineCommand::Start { .. })
        | Commands::Log { .. } => {
            unreachable!("streaming commands handled before build_command")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_project_explicit() {
        let result = resolve_project(&Some("my-project".to_string())).unwrap();
        assert_eq!(result, "my-project");
    }

    #[test]
    fn test_resolve_project_auto() {
        let result = resolve_project(&None).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_build_command_init() {
        let (cmd, params) = build_command(
            &None,
            Commands::Init {
                name: "test".to_string(),
                base_branch: "main".to_string(),
                git_provider: None,
            },
        )
        .unwrap();
        assert_eq!(cmd, "project.init");
        assert_eq!(params["name"], "test");
        assert_eq!(params["base_branch"], "main");
    }

    #[test]
    fn test_build_command_project_list() {
        let (cmd, _params) =
            build_command(&None, Commands::Projects(cli::ProjectsCommand::List)).unwrap();
        assert_eq!(cmd, "project.list");
    }

    #[test]
    fn test_build_command_spec_list() {
        let (cmd, params) =
            build_command(&Some("proj".to_string()), Commands::Spec(SpecCommand::List)).unwrap();
        assert_eq!(cmd, "spec.list");
        assert_eq!(params["project_name"], "proj");
    }

    #[test]
    fn test_build_command_run() {
        let (cmd, params) = build_command(
            &Some("proj".to_string()),
            Commands::Run {
                parallel: Some(4),
                story: None,
                dry_run: true,
            },
        )
        .unwrap();
        assert_eq!(cmd, "exec.run");
        assert_eq!(params["max_parallel"], 4);
        assert_eq!(params["dry_run"], true);
    }

    #[test]
    fn test_build_command_plan_show_with_wave() {
        let (cmd, params) = build_command(
            &None,
            Commands::Plan(cli::PlanCommand::Show {
                wave: Some(2),
                dag: true,
            }),
        )
        .unwrap();
        assert_eq!(cmd, "plan.show");
        assert_eq!(params["wave"], 2);
        assert_eq!(params["dag"], true);
    }

    #[test]
    fn test_handle_response_ok_json() {
        let resp = socket_client::Response {
            id: "test".to_string(),
            status: ResponseStatus::Ok,
            data: serde_json::json!({"key": "value"}),
        };
        let result = handle_response(resp, true, "test");
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_response_ok_human() {
        format::init(true); // no color for tests
        let resp = socket_client::Response {
            id: "test".to_string(),
            status: ResponseStatus::Ok,
            data: serde_json::json!({"key": "value"}),
        };
        let result = handle_response(resp, false, "test");
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_response_error() {
        let resp = socket_client::Response {
            id: "test".to_string(),
            status: ResponseStatus::Error,
            data: serde_json::json!({"message": "NOT_FOUND: spec does not exist"}),
        };
        let result = handle_response(resp, false, "spec.view");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("NOT_FOUND"));
    }

    #[test]
    fn test_build_command_config_set() {
        let (cmd, params) = build_command(
            &Some("proj".to_string()),
            Commands::Config(cli::ConfigCommand::Set {
                key: "max_parallel".to_string(),
                value: "8".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "config.set");
        assert_eq!(params["key"], "max_parallel");
        assert_eq!(params["value"], 8);
    }

    #[test]
    fn test_build_command_cleanup() {
        let (cmd, params) = build_command(
            &None,
            Commands::Cleanup {
                logs: true,
                all: false,
                older_than: Some("7d".to_string()),
                dry_run: true,
            },
        )
        .unwrap();
        assert_eq!(cmd, "cleanup.logs");
        assert_eq!(params["logs"], true);
        assert_eq!(params["older_than"], "7d");
        assert_eq!(params["dry_run"], true);
    }

    #[test]
    fn test_build_command_pipeline_list() {
        let (cmd, params) = build_command(
            &Some("proj".to_string()),
            Commands::Pipeline(cli::PipelineCommand::List),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.list");
        assert_eq!(params["project_name"], "proj");
    }

    #[test]
    fn test_build_command_pipeline_status() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Status {
                pipeline_id: "abc-123".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.status");
        assert_eq!(params["pipeline_run_id"], "abc-123");
    }

    #[test]
    fn test_build_command_pipeline_cancel() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Cancel {
                pipeline_id: "abc-123".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.cancel");
        assert_eq!(params["pipeline_run_id"], "abc-123");
    }

    #[test]
    fn test_build_command_pipeline_log() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Log {
                pipeline_id: "abc-123".to_string(),
                stage: Some("plan".to_string()),
                iteration: Some(2),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.log");
        assert_eq!(params["pipeline_run_id"], "abc-123");
        assert_eq!(params["stage_type"], "plan");
        assert_eq!(params["iteration"], 2);
    }

    #[test]
    fn test_build_command_pipeline_log_no_filters() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Log {
                pipeline_id: "abc-123".to_string(),
                stage: None,
                iteration: None,
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.log");
        assert_eq!(params["pipeline_run_id"], "abc-123");
        assert!(params.get("stage_type").is_none());
        assert!(params.get("iteration").is_none());
    }

    #[test]
    fn test_build_command_pipeline_approve() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Approve {
                pipeline_id: "abc-123".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.approve");
        assert_eq!(params["pipeline_run_id"], "abc-123");
    }

    #[test]
    fn test_build_command_pipeline_reject() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Reject {
                pipeline_id: "abc-123".to_string(),
                feedback: "needs more detail".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.reject");
        assert_eq!(params["pipeline_run_id"], "abc-123");
        assert_eq!(params["feedback"], "needs more detail");
    }

    #[test]
    fn test_build_command_pipeline_answer() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Answer {
                pipeline_id: "abc-123".to_string(),
                question_id: "q-456".to_string(),
                answer: "use redis".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.answer");
        assert_eq!(params["pipeline_run_id"], "abc-123");
        assert_eq!(params["question_id"], "q-456");
        assert_eq!(params["answer"], "use redis");
    }

    #[test]
    fn test_build_command_pipeline_questions() {
        let (cmd, params) = build_command(
            &None,
            Commands::Pipeline(cli::PipelineCommand::Questions {
                pipeline_id: "abc-123".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(cmd, "pipeline.questions");
        assert_eq!(params["pipeline_run_id"], "abc-123");
    }
}
