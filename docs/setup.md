# nflow — Setup & Installation

## Prerequisites

| Dependency | Minimum Version | Purpose |
|-----------|----------------|---------|
| Rust | 1.75+ | Building nflow |
| Claude Code CLI | latest | AI agent runtime (`claude` command) |
| Git | 2.20+ | Version control, worktrees |
| `gh` (GitHub CLI) | 2.0+ | Creating PRs (if using GitHub) |
| `glab` (GitLab CLI) | 1.30+ | Creating MRs (if using GitLab) |
| SQLite | 3.35+ | Bundled via `rusqlite`, no separate install needed |

### Verify prerequisites

```bash
# Rust
rustc --version    # must be >= 1.75

# Claude Code
claude --version   # must be installed and authenticated

# Git
git --version      # must be >= 2.20 (for worktree support)

# GitHub CLI (if using GitHub)
gh --version
gh auth status     # must be authenticated

# GitLab CLI (if using GitLab)
glab --version
glab auth status   # must be authenticated
```

## Installation

### From source

```bash
git clone https://github.com/nolood/nflow.git
cd nflow
cargo build --release

# Install to ~/.cargo/bin/
cargo install --path crates/nflow-cli
cargo install --path crates/nflow-tui
cargo install --path crates/nflow-daemon
```

This produces three binaries:
- `nflow` — CLI client
- `nflow-tui` — TUI client (or invoked via `nflow tui`)
- `nflow-daemon` — daemon process (or invoked via `nflow daemon start`)

Note: the `nflow` binary can launch both TUI and daemon as subcommands, so separate binaries are optional. The single `nflow` binary is sufficient.

### From cargo (when published)

```bash
cargo install nflow
```

## Initial Setup

### 1. Verify Claude Code authentication

```bash
claude --version
# If not authenticated, run:
claude
# Follow the OAuth flow
```

### 2. Initialize a project

```bash
cd /path/to/your/project
nflow init --name "my-project"
```

This creates:
- Entry in `~/.nflow/nflow.db`
- Directory `~/.nflow/projects/my-project/`
- Subdirectory `~/.nflow/projects/my-project/specs/`
- Subdirectory `~/.nflow/projects/my-project/agent-logs/`

### 3. (Optional) Configure

```bash
# Set max parallel agents
nflow config set max_parallel 4

# Set git provider (if using GitLab)
nflow config set --project my-project git_provider gitlab

# Set base branch (if not main)
nflow config set --project my-project base_branch develop
```

### 4. Start using

```bash
# Start a spec session
nflow spec new "feature-name"

# Or open TUI
nflow tui
```

## Directory Structure After Setup

```
~/.nflow/
├── config.toml              # Global configuration
├── nflow.db                 # SQLite database
├── logs/
│   └── daemon.log
└── projects/
    └── my-project/
        ├── specs/
        └── agent-logs/
```

## Uninstallation

```bash
# Remove binaries
cargo uninstall nflow

# Remove all data (specs, database, logs, worktrees)
rm -rf ~/.nflow

# Clean up any remaining worktrees
# (worktrees are git-managed, so removing them is safe)
```

## Troubleshooting

### "daemon not running"

```bash
nflow daemon start
# Or with debug output:
nflow daemon start --foreground
```

### "claude: command not found"

Install Claude Code CLI:
```bash
npm install -g @anthropic-ai/claude-code
# Or follow: https://docs.anthropic.com/en/docs/claude-code
```

### "gh: command not found" / "glab: command not found"

```bash
# GitHub CLI
brew install gh    # macOS
# or: https://cli.github.com/

# GitLab CLI
brew install glab  # macOS
# or: https://gitlab.com/gitlab-org/cli
```

### Socket permission errors

```bash
# Remove stale socket
rm ~/.nflow/nflow.sock

# Restart daemon
nflow daemon start
```

### Database locked

This can happen if the daemon crashed without cleanup.

```bash
# Stop daemon if running
nflow daemon stop

# Remove stale lock (SQLite WAL files)
rm -f ~/.nflow/nflow.db-wal ~/.nflow/nflow.db-shm

# Restart
nflow daemon start
```
