use clap::{Parser, Subcommand};

/// nflow — CLI/TUI orchestrator for Claude Code agents
#[derive(Debug, Parser)]
#[command(name = "nflow", version, about, long_about = None)]
pub struct Cli {
    /// Override project (default: detected from current directory)
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// Verbose output
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Output in JSON format (for scripting/agent use)
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage the background daemon process
    #[command(subcommand)]
    Daemon(DaemonCommand),

    /// Initialize a new nflow project in the current directory
    Init {
        /// Project name
        #[arg(long)]
        name: String,

        /// Base branch (default: main)
        #[arg(long, default_value = "main")]
        base_branch: String,

        /// Git provider: github or gitlab (default: auto-detect from remote)
        #[arg(long)]
        git_provider: Option<String>,
    },

    /// Manage registered projects
    #[command(subcommand)]
    Projects(ProjectsCommand),

    /// Manage a specific project
    #[command(subcommand)]
    Project(ProjectCommand),

    /// Manage specs (requirements documents)
    #[command(subcommand)]
    Spec(SpecCommand),

    /// Manage decomposition plans (waves)
    #[command(subcommand)]
    Plan(PlanCommand),

    /// Enable execution — start running agents for ready stories
    Run {
        /// Max parallel agents (overrides config)
        #[arg(long)]
        parallel: Option<u32>,

        /// Run only a specific story
        #[arg(long)]
        story: Option<String>,

        /// Show what would run without starting agents
        #[arg(long)]
        dry_run: bool,
    },

    /// Pause execution — stop picking up new stories (running agents continue)
    Pause,

    /// Show execution status of all work items
    Status {
        /// Show only a specific wave
        #[arg(long)]
        wave: Option<u32>,
    },

    /// Stream or print agent output log for a task
    Log {
        /// Task ID (e.g., W1-T3)
        task_id: String,

        /// Follow the log in real-time (like tail -f)
        #[arg(long, short)]
        follow: bool,
    },

    /// Re-run a failed task
    Retry {
        /// Task ID (e.g., W1-T3)
        task_id: String,
    },

    /// Mark a failed task as done and continue with the next task
    Skip {
        /// Task ID (e.g., W1-T3)
        task_id: String,
    },

    /// Continue a failed or cancelled story from where it left off
    Continue {
        /// Story ID (e.g., W1-S2)
        story_id: String,

        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },

    /// Stop running agents
    Stop {
        /// Story ID to stop (omit to stop all in current project)
        story_id: Option<String>,

        /// Stop all running agents in a specific wave
        #[arg(long)]
        wave: Option<u32>,

        /// Stop all running agents across all projects
        #[arg(long)]
        all: bool,
    },

    /// Cancel a pending or ready story
    Cancel {
        /// Story ID (e.g., W1-S2)
        story_id: String,

        /// Cancel all pending/ready stories in a wave
        #[arg(long)]
        wave: Option<u32>,
    },

    /// Manage git worktrees
    #[command(subcommand)]
    Worktree(WorktreeCommand),

    /// Remove old agent logs and accumulated data
    Cleanup {
        /// Clean agent logs
        #[arg(long)]
        logs: bool,

        /// Include logs for non-done stories (default: only done stories)
        #[arg(long)]
        all: bool,

        /// Only delete logs older than the given duration (e.g., 7d, 30d)
        #[arg(long)]
        older_than: Option<String>,

        /// Show what would be deleted without deleting
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage configuration
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Launch the terminal user interface
    Tui,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the background daemon process
    Start {
        /// Run in foreground (for debugging)
        #[arg(long)]
        foreground: bool,
    },

    /// Stop the running daemon
    Stop,

    /// Show daemon status (running/stopped, PID, uptime)
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ProjectsCommand {
    /// List all registered projects
    List,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Remove a project from nflow
    Delete {
        /// Project name
        name: String,

        /// Force delete (skip confirmation, bypass in-progress check)
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SpecCommand {
    /// Start a new spec session
    New {
        /// Spec name
        name: String,

        /// Give Claude access to the project's source code
        #[arg(long)]
        with_codebase: bool,
    },

    /// List specs for the current project
    List,

    /// Print spec contents to stdout
    View {
        /// Spec name
        name: String,
    },

    /// Resume a paused spec session
    Resume {
        /// Spec name (default: most recently updated draft)
        name: Option<String>,
    },

    /// Mark a spec as approved
    Approve {
        /// Spec name
        name: String,
    },

    /// Move an approved spec back to draft for further editing
    Reopen {
        /// Spec name
        name: String,
    },

    /// Soft-delete a spec
    Delete {
        /// Spec name
        name: String,

        /// Skip confirmation for approved specs
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// Create a new wave from unassigned approved specs
    Generate {
        /// Only use specific specs (comma-separated)
        #[arg(long)]
        specs: Option<String>,

        /// Give Claude read access to the project's source code
        #[arg(long)]
        with_codebase: bool,
    },

    /// Display the plan as an ASCII tree with statuses
    Show {
        /// Show only a specific wave
        #[arg(long)]
        wave: Option<u32>,

        /// Show story dependency graph as ASCII adjacency list
        #[arg(long)]
        dag: bool,
    },

    /// Provide feedback on the current draft wave
    Feedback {
        /// Feedback message
        message: String,

        /// Target a specific wave (default: latest draft)
        #[arg(long)]
        wave: Option<u32>,
    },

    /// Approve a wave's plan and lock work items for execution
    Approve {
        /// Target a specific wave (default: latest draft)
        #[arg(long)]
        wave: Option<u32>,
    },

    /// Discard a wave's plan and free specs for reuse
    Discard {
        /// Target a specific wave (default: latest draft)
        #[arg(long)]
        wave: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// List all active worktrees for the current project
    List,

    /// Remove worktrees for completed and cancelled stories
    Clean {
        /// Clean worktrees across all projects
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print current configuration
    Show,

    /// Set a config value
    Set {
        /// Config key (e.g., max_parallel, git_provider)
        key: String,

        /// Config value
        value: String,
    },
}
