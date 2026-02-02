use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agman")]
#[command(about = "Agent Manager - Orchestrate stateless AI agents across isolated git worktrees")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start a new task (creates worktree, tmux session, starts flow)
    New {
        /// Branch name for the task
        branch_name: String,
        /// Description of what to build
        description: String,
        /// Flow to use (default: "default")
        #[arg(long, default_value = "default")]
        flow: String,
    },

    /// List all tasks
    List,

    /// Delete a task (removes worktree, tmux session, and task directory)
    Delete {
        /// Branch name of the task to delete
        branch_name: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Run a specific agent on a task
    Run {
        /// Branch name of the task
        branch_name: String,
        /// Agent to run
        #[arg(long)]
        agent: String,
        /// Run agent in a loop until stop condition
        #[arg(long)]
        r#loop: bool,
    },

    /// Pause a running task
    Pause {
        /// Branch name of the task to pause
        branch_name: String,
    },

    /// Resume a paused task
    Resume {
        /// Branch name of the task to resume
        branch_name: String,
    },

    /// Attach to a task's tmux session
    Attach {
        /// Branch name of the task to attach to
        branch_name: String,
    },

    /// Initialize agman (creates directories and default files)
    Init,
}
