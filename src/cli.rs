use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agman")]
#[command(about = "Agent Manager - Orchestrate stateless AI agents across isolated git worktrees")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the entire flow for a task (used internally, runs in tmux)
    #[command(hide = true)]
    FlowRun {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Continue a task with follow-up instructions
    #[command(hide = true)]
    Continue {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Follow-up instructions or feedback (reads from FEEDBACK.md if not provided)
        #[arg(allow_hyphen_values = true)]
        feedback: Option<String>,
    },

    /// Run a stored command on a task
    #[command(hide = true)]
    RunCommand {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Command identifier (e.g., "create-pr", "address-review")
        command_id: String,
        /// Branch name argument (used by commands like rebase)
        #[arg(long)]
        branch: Option<String>,
    },

    /// Run a stored command's flow for a task (used internally, runs in tmux)
    #[command(hide = true)]
    CommandFlowRun {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Command identifier (e.g., "create-pr", "address-review")
        command_id: String,
        /// Branch name argument (used by commands like rebase)
        #[arg(long)]
        branch: Option<String>,
    },

    /// Initialize agman configuration (creates default flows, prompts, and commands)
    #[command(hide = true)]
    Init {
        /// Overwrite existing files with defaults
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}
