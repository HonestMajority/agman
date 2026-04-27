use clap::{Parser, Subcommand, ValueEnum};

use agman::task::TaskStatus;

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
    /// Run a stored command on a task
    RunCommand {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Command identifier (e.g., "create-pr", "address-review")
        command_id: String,
        /// Branch name argument (used by commands like rebase)
        #[arg(long)]
        branch: Option<String>,
    },

    /// Initialize agman configuration (creates default flows, prompts, and commands)
    Init {
        /// Overwrite existing files with defaults
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Send a message to an agent's inbox
    #[command(after_help = "\
EXAMPLES:
  agman send-message ceo \"Check the deploy status\"
  cat <<'EOF' | agman send-message ceo -
  Multi-line message via stdin using the - sentinel.
  EOF
  agman send-message ceo @./message.md")]
    SendMessage {
        /// Target: "ceo", "telegram", "researcher:<project>--<name>", or a project name (for the PM)
        target: String,
        /// Message text (can also be provided via stdin or --file)
        #[arg(allow_hyphen_values = true)]
        message: Option<String>,
        /// Read message from a file
        #[arg(short = 'F', long)]
        file: Option<std::path::PathBuf>,
        /// Sender name
        #[arg(long)]
        from: Option<String>,
    },

    /// Create a new project with a PM
    CreateProject {
        /// Project name (alphanumeric + hyphens)
        name: String,
        /// Project description
        #[arg(long)]
        description: Option<String>,
    },

    /// List all projects
    ListProjects,

    /// Get detailed status of a project
    ProjectStatus {
        /// Project name
        name: String,
    },

    /// Delete a project and archive all its tasks
    DeleteProject {
        /// Project name
        name: String,
    },

    /// Stop a running task
    StopTask {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
    },

    /// Archive a task (remove worktrees, keep directory and branches)
    ArchiveTask {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Save the task (exempt from automatic purging)
        #[arg(long, default_value_t = false)]
        save: bool,
    },

    /// Create a task within a project
    #[command(after_help = "\
EXAMPLES:
  agman create-pm-task myproj myrepo fix-bug --description \"Fix the login bug\"
  cat <<'EOF' | agman create-pm-task myproj myrepo fix-bug --description -
  Multi-line description via stdin using the - sentinel.
  EOF
  agman create-pm-task myproj myrepo fix-bug --description @./task-desc.md")]
    CreatePmTask {
        /// Project name
        project: String,
        /// Repository name
        repo: String,
        /// Task name (becomes the branch name, e.g. 'fix-login-bug')
        task_name: String,
        /// Task description for the TASK.md Goal section
        #[arg(long, short, allow_hyphen_values = true)]
        description: Option<String>,
    },

    /// List tasks belonging to a project
    ListPmTasks {
        /// Project name
        project: String,
        /// Filter by task status (running, stopped, input_needed, on_hold)
        #[arg(long)]
        status: Option<StatusFilter>,
    },

    /// Get status and recent log for a task
    TaskStatus {
        /// Task identifier (repo--branch format)
        task_id: String,
    },

    /// Read the agent log for a task
    TaskLog {
        /// Task identifier (repo--branch format)
        task_id: String,
        /// Number of lines from the end to show
        #[arg(long, default_value = "50")]
        tail: usize,
    },

    /// Read the current plan (TASK.md) for a task
    TaskCurrentPlan {
        /// Task identifier (repo--branch format)
        task_id: String,
    },

    /// Show aggregated status across all projects and tasks
    Status,

    /// Queue feedback on a running task
    #[command(after_help = "\
EXAMPLES:
  agman feedback myrepo--fix-bug \"Please also check edge cases\"
  cat <<'EOF' | agman feedback myrepo--fix-bug -
  Multi-line feedback via stdin using the - sentinel.
  EOF
  agman feedback myrepo--fix-bug @./feedback.md")]
    Feedback {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// Feedback text to queue
        #[arg(allow_hyphen_values = true)]
        feedback: Option<String>,
        /// Read feedback from a file
        #[arg(short = 'F', long)]
        file: Option<std::path::PathBuf>,
    },

    /// Create a researcher (defaults to CEO-level when --project is omitted)
    #[command(after_help = "\
EXAMPLES:
  agman create-researcher my-research --description \"Investigate the API latency\"
  cat <<'EOF' | agman create-researcher my-research --description -
  Multi-line description via stdin using the - sentinel.
  EOF
  agman create-researcher my-research --description @./research-desc.md")]
    CreateResearcher {
        /// Researcher name (alphanumeric + hyphens)
        name: String,
        /// Project name (defaults to "ceo" for CEO-level researchers)
        #[arg(long)]
        project: Option<String>,
        /// Repository name (for working directory context)
        #[arg(long)]
        repo: Option<String>,
        /// Branch name (used with --repo for worktree resolution)
        #[arg(long)]
        branch: Option<String>,
        /// Task ID to inherit working directory from
        #[arg(long)]
        task: Option<String>,
        /// Research description/question
        #[arg(long, short, allow_hyphen_values = true)]
        description: Option<String>,
    },

    /// List researchers
    ListResearchers {
        /// Filter by project name
        #[arg(long)]
        project: Option<String>,
        /// Show only CEO-level researchers
        #[arg(long)]
        ceo: bool,
    },

    /// Archive a researcher (defaults to CEO-level when --project is omitted)
    ArchiveResearcher {
        /// Researcher name
        name: String,
        /// Project name (defaults to "ceo" for CEO-level researchers)
        #[arg(long)]
        project: Option<String>,
    },

    /// Respawn an agent with a fresh session (CEO or PM)
    RespawnAgent {
        /// Target: "ceo" or a project name (for the PM)
        target: String,
        /// Skip graceful handoff — kill and restart immediately
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Handoff timeout in seconds (default 120)
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Restart the agman TUI binary itself to pick up a new version. Chat sessions are unaffected.
    Restart,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatusFilter {
    Running,
    Stopped,
    InputNeeded,
    OnHold,
}

impl StatusFilter {
    pub fn to_task_status(self) -> TaskStatus {
        match self {
            StatusFilter::Running => TaskStatus::Running,
            StatusFilter::Stopped => TaskStatus::Stopped,
            StatusFilter::InputNeeded => TaskStatus::InputNeeded,
            StatusFilter::OnHold => TaskStatus::OnHold,
        }
    }
}
