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

    /// Send a message to an agent's inbox
    #[command(hide = true)]
    SendMessage {
        /// Target: "ceo" or a project name (for the PM)
        target: String,
        /// Message text
        message: String,
        /// Sender name
        #[arg(long)]
        from: Option<String>,
    },

    /// Create a new project with a PM
    #[command(hide = true)]
    CreateProject {
        /// Project name (alphanumeric + hyphens)
        name: String,
        /// Project description
        #[arg(long)]
        description: Option<String>,
    },

    /// List all projects
    #[command(hide = true)]
    ListProjects,

    /// Get detailed status of a project
    #[command(hide = true)]
    ProjectStatus {
        /// Project name
        name: String,
    },

    /// Create a task within a project
    #[command(hide = true)]
    CreatePmTask {
        /// Project name
        project: String,
        /// Repository name
        repo: String,
        /// Branch name
        branch: String,
        /// Task description
        description: String,
    },

    /// List tasks belonging to a project
    #[command(hide = true)]
    ListPmTasks {
        /// Project name
        project: String,
    },

    /// Get status and recent log for a task
    #[command(hide = true)]
    TaskStatus {
        /// Task identifier (repo--branch format)
        task_id: String,
    },

    /// Read the agent log for a task
    #[command(hide = true)]
    TaskLog {
        /// Task identifier (repo--branch format)
        task_id: String,
        /// Number of lines from the end to show
        #[arg(long, default_value = "50")]
        tail: usize,
    },
}
