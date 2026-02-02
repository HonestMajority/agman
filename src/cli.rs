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
        /// Repository name (under ~/repos/)
        repo_name: String,
        /// Branch name for the task (also used as worktree name)
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
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Run a specific agent on a task
    Run {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Agent to run
        #[arg(long)]
        agent: String,
        /// Run agent in a loop until stop condition
        #[arg(long)]
        r#loop: bool,
    },

    /// Run the entire flow for a task (used internally, runs in tmux)
    #[command(hide = true)]
    FlowRun {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Pause a running task
    Pause {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Resume a paused task
    Resume {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Attach to a task's tmux session
    Attach {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Initialize agman (creates directories and default files)
    Init,

    /// Continue a task with follow-up instructions
    Continue {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Follow-up instructions or feedback
        feedback: String,
        /// Flow to use (default: "continue")
        #[arg(long, default_value = "continue")]
        flow: String,
    },
}
