use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "agman")]
#[command(about = "Agent Manager - Orchestrate long-lived AI agents across projects and tasks")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize agman configuration
    Init {
        /// Overwrite existing files with defaults
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Send a message to an agent's inbox
    #[command(after_help = "\
EXAMPLES:
  agman send-message chief-of-staff \"Check the deploy status\"
  cat <<'EOF' | agman send-message chief-of-staff -
  Multi-line message via stdin using the - sentinel.
  EOF
  agman send-message chief-of-staff @./message.md")]
    SendMessage {
        /// Target: "chief-of-staff", "telegram", a project name (for the PM),
        /// or "<kind>:<project>--<name>" for engineer/researcher/operator/reviewer/tester
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
    #[command(after_help = "\
EXAMPLES:
  agman create-project myproj --description \"UI rewrite\"
  agman create-project myproj --description \"UI rewrite\" --initial-message \"Kick off with the design doc\"
  agman create-project myproj --description \"UI rewrite\" --initial-message @./goal.txt
  cat <<'EOF' | agman create-project myproj --description \"UI rewrite\" --initial-message -
  Multi-line message via stdin using the - sentinel.
  EOF")]
    CreateProject {
        /// Project name (alphanumeric + hyphens)
        name: String,
        /// Human label for the project (shown in lists). Not sent to the PM.
        #[arg(long)]
        description: Option<String>,
        /// Initial message sent to the PM's inbox on spawn
        /// (accepts inline text, @path, or - for stdin)
        #[arg(long, allow_hyphen_values = true)]
        initial_message: Option<String>,
        /// Read the initial message from a file
        #[arg(short = 'F', long)]
        file: Option<std::path::PathBuf>,
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

    /// List available project templates
    ListTemplates,

    /// Print a template's body to stdout
    GetTemplate {
        /// Template name (filename without .md)
        name: String,
    },

    /// Save a new project template
    #[command(after_help = "\
EXAMPLES:
  agman create-template release-cleanup --file ./templates/release-cleanup.md
  cat <<'EOF' | agman create-template release-cleanup -
  Multi-line template via stdin using the - sentinel.
  EOF
  agman create-template release-cleanup @./templates/release-cleanup.md")]
    CreateTemplate {
        /// Template name (alphanumeric + hyphens; becomes the filename)
        name: String,
        /// Template body (inline text, @path, or - for stdin)
        #[arg(allow_hyphen_values = true)]
        body: Option<String>,
        /// Read template body from a file
        #[arg(short = 'F', long)]
        file: Option<std::path::PathBuf>,
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
  agman create-pm-task myproj myrepo fix-bug --first-prompt \"Fix the login bug\"
  cat <<'EOF' | agman create-pm-task myproj myrepo fix-bug --first-prompt -
  Multi-line first prompt via stdin using the - sentinel.
  EOF
  agman create-pm-task myproj myrepo fix-bug --first-prompt @./task-prompt.md")]
    CreatePmTask {
        /// Project name
        project: String,
        /// Repository name
        repo: String,
        /// Task name (becomes the branch name, e.g. 'fix-login-bug')
        task_name: String,
        /// Optional first prompt sent to the attached engineer
        #[arg(
            long = "first-prompt",
            short = 'd',
            alias = "description",
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
    },

    /// List tasks belonging to a project
    ListPmTasks {
        /// Project name
        project: String,
    },

    /// Show task metadata and recent log
    TaskInfo {
        /// Task identifier (repo--branch format)
        task_id: String,
    },

    /// Link a GitHub PR to a task so the TUI can display and open it
    #[command(after_help = "\
EXAMPLES:
  agman link-pr backend--fix-login https://github.com/acme/backend/pull/42
  agman link-pr backend--fix-login 42 --author alice
  agman link-pr backend--fix-login --from-sidecar")]
    LinkPr {
        /// Task identifier (repo--branch format, or just branch if unambiguous)
        task_id: String,
        /// PR number or URL. A number is resolved through the task repo's origin remote.
        pr: Option<String>,
        /// Mark this PR as owned by the task engineer (default)
        #[arg(long, default_value_t = true, conflicts_with = "not_owned")]
        owned: bool,
        /// Mark this PR as external/not owned by the task engineer
        #[arg(long, default_value_t = false)]
        not_owned: bool,
        /// GitHub author/login for the PR
        #[arg(long)]
        author: Option<String>,
        /// Overwrite a different existing linked PR
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Read the PR reference from a legacy .pr-link sidecar
        #[arg(long, default_value_t = false)]
        from_sidecar: bool,
    },

    /// Read the agent log for a task
    TaskLog {
        /// Task identifier (repo--branch format)
        task_id: String,
        /// Number of lines from the end to show
        #[arg(long, default_value = "50")]
        tail: usize,
    },

    /// Show aggregated status across all projects and tasks
    Status,

    /// Create a project-scoped agent (researcher, operator, reviewer, or tester).
    #[command(after_help = "\
EXAMPLES:
  agman create-agent --kind researcher --name api-investigator --project backend --first-prompt \"Investigate the API latency\"
  agman create-agent --kind operator --name docs-updater --project docs --first-prompt \"Update the launch notes\"
  agman create-agent --kind reviewer --name pr-1247 --project reviews \\
    --branch galoy:fix-deposit-path \\
    --branch lana-dashboard:fix-deposit-path \\
    --first-prompt \"Review the cross-repo deposit fix\"
  agman create-agent --kind tester --name browser-pass --project reviews \\
    --branch galoy:fix-deposit-path --browser \\
    --first-prompt \"Exercise the deposit path in browser\"")]
    CreateAgent {
        /// Agent kind: researcher, operator, reviewer, or tester
        #[arg(long, value_enum)]
        kind: AgentKindArg,
        /// Agent name (alphanumeric + hyphens)
        #[arg(long, short)]
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
        /// Optional first prompt sent to the agent inbox
        #[arg(
            long = "first-prompt",
            short = 'd',
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
        // --- Repo-hint flags (researcher/operator only; rejected for reviewer/tester) ---
        /// Repository name (researcher/operator only — for working directory context)
        #[arg(long)]
        repo: Option<String>,
        /// Branch name (researcher/operator only — used with --repo for worktree resolution)
        #[arg(long, conflicts_with = "branch_pair")]
        branch_for_researcher: Option<String>,
        /// Task ID to inherit working directory from (researcher/operator only)
        #[arg(long)]
        task: Option<String>,
        // --- Worktree-backed flags (repeatable; rejected for researcher) ---
        /// `<repo>:<branch>` pair to scope the reviewer/tester to; repeat
        /// to include multiple. Required for reviewers and testers.
        #[arg(long = "branch", value_name = "REPO:BRANCH")]
        branch_pair: Vec<String>,
        /// Request browser automation tools (tester only)
        #[arg(long, default_value_t = false)]
        browser: bool,
    },

    /// List project agents
    ListAgents {
        /// Project name
        #[arg(long)]
        project: String,
        /// Filter by kind
        #[arg(long, value_enum)]
        kind: Option<AgentKindArg>,
    },

    /// Archive a project-scoped agent
    ArchiveAgent {
        /// Agent name
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
    },

    /// Attach a non-engineer agent to a task
    #[command(after_help = "\
EXAMPLES:
  agman attach-agent --project backend --name api-investigator --task backend--fix-login
  agman attach-agent --project backend --name pr-review --task backend--fix-login --role-label \"PR review\"")]
    AttachAgent {
        /// Project name
        #[arg(long)]
        project: String,
        /// Agent name
        #[arg(long, short)]
        name: String,
        /// Task identifier (repo--branch format)
        #[arg(long)]
        task: String,
        /// Optional role label shown with the task attachment
        #[arg(long)]
        role_label: Option<String>,
    },

    /// Move a non-engineer agent to another task
    #[command(after_help = "\
EXAMPLES:
  agman move-agent --project backend --name api-investigator --task backend--new-task
  agman move-agent --project backend --name pr-review --task backend--new-task --role-label \"Second pass\"")]
    MoveAgent {
        /// Project name
        #[arg(long)]
        project: String,
        /// Agent name
        #[arg(long, short)]
        name: String,
        /// Destination task identifier (repo--branch format)
        #[arg(long)]
        task: String,
        /// Optional role label shown with the task attachment
        #[arg(long)]
        role_label: Option<String>,
    },

    /// Detach a non-engineer agent from its task
    #[command(after_help = "\
EXAMPLES:
  agman detach-agent --project backend --name api-investigator")]
    DetachAgent {
        /// Project name
        #[arg(long)]
        project: String,
        /// Agent name
        #[arg(long, short)]
        name: String,
    },

    /// Create a researcher agent.
    #[command(after_help = "\
EXAMPLES:
  agman create-researcher my-research --project backend --first-prompt \"Investigate the API latency\"
  cat <<'EOF' | agman create-researcher my-research --project backend --first-prompt -
  Multi-line first prompt via stdin using the - sentinel.
  EOF
  agman create-researcher my-research --project backend --first-prompt @./research-prompt.md")]
    CreateResearcher {
        /// Researcher name (alphanumeric + hyphens)
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
        /// Repository name (for working directory context)
        #[arg(long)]
        repo: Option<String>,
        /// Branch name (used with --repo for worktree resolution)
        #[arg(long)]
        branch: Option<String>,
        /// Task ID to inherit working directory from
        #[arg(long)]
        task: Option<String>,
        /// Optional first prompt sent to the researcher inbox
        #[arg(
            long = "first-prompt",
            short = 'd',
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
    },

    /// Create an operator agent.
    #[command(after_help = "\
EXAMPLES:
  agman create-operator docs-updater --project docs --first-prompt \"Update the launch notes\"
  cat <<'EOF' | agman create-operator docs-updater --project docs --first-prompt -
  Multi-line first prompt via stdin using the - sentinel.
  EOF
  agman create-operator docs-updater --project docs --first-prompt @./operator-prompt.md")]
    CreateOperator {
        /// Operator name (alphanumeric + hyphens)
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
        /// Repository name (for working directory context)
        #[arg(long)]
        repo: Option<String>,
        /// Branch name (used with --repo for worktree resolution)
        #[arg(long)]
        branch: Option<String>,
        /// Task ID to inherit working directory from
        #[arg(long)]
        task: Option<String>,
        /// Optional first prompt sent to the operator inbox
        #[arg(
            long = "first-prompt",
            short = 'd',
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
    },

    /// Create a reviewer agent.
    #[command(after_help = "\
EXAMPLES:
  agman create-reviewer --name pr-1247 --project reviews \\
    --branch galoy:fix-deposit-path \\
    --branch lana-dashboard:fix-deposit-path \\
    --first-prompt \"Review the cross-repo deposit fix\"")]
    CreateReviewer {
        /// Reviewer name (alphanumeric + hyphens)
        #[arg(long, short)]
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
        /// `<repo>:<branch>` pair (repeatable, required at least once)
        #[arg(long = "branch", value_name = "REPO:BRANCH", required = true)]
        branch_pair: Vec<String>,
        /// Optional first prompt sent to the reviewer inbox
        #[arg(
            long = "first-prompt",
            short = 'd',
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
    },

    /// Create a tester agent.
    #[command(after_help = "\
EXAMPLES:
  agman create-tester --name browser-pass --project reviews \\
    --branch galoy:fix-deposit-path --browser \\
    --first-prompt \"Exercise the deposit path\"")]
    CreateTester {
        /// Tester name (alphanumeric + hyphens)
        #[arg(long, short)]
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
        /// `<repo>:<branch>` pair (repeatable, required at least once)
        #[arg(long = "branch", value_name = "REPO:BRANCH", required = true)]
        branch_pair: Vec<String>,
        /// Request browser automation tools
        #[arg(long, default_value_t = false)]
        browser: bool,
        /// Optional first prompt sent to the tester inbox
        #[arg(
            long = "first-prompt",
            short = 'd',
            allow_hyphen_values = true,
            value_name = "FIRST_PROMPT"
        )]
        first_prompt: Option<String>,
    },

    /// List researcher agents.
    ListResearchers {
        /// Project name
        #[arg(long)]
        project: String,
    },

    /// Archive a researcher agent.
    ArchiveResearcher {
        /// Researcher name
        name: String,
        /// Project name
        #[arg(long)]
        project: String,
    },

    /// Respawn an agent with a fresh session (Chief of Staff or PM)
    RespawnAgent {
        /// Target: "chief-of-staff" or a project name (for the PM)
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

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum AgentKindArg {
    Researcher,
    Operator,
    Reviewer,
    Tester,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use clap::{error::ErrorKind, Parser};

    #[test]
    fn create_pm_task_parses_first_prompt_short_and_description_alias() {
        let parsed = Cli::try_parse_from([
            "agman",
            "create-pm-task",
            "project",
            "repo",
            "branch",
            "--first-prompt",
            "Do the work",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreatePmTask {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Do the work"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-pm-task",
            "project",
            "repo",
            "branch",
            "-d",
            "Short prompt",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreatePmTask {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Short prompt"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-pm-task",
            "project",
            "repo",
            "branch",
            "--description",
            "Alias prompt",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreatePmTask {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Alias prompt"
        ));
    }

    #[test]
    fn agent_commands_parse_first_prompt_short_and_reject_description_alias() {
        let parsed = Cli::try_parse_from([
            "agman",
            "create-agent",
            "--kind",
            "researcher",
            "--name",
            "investigate",
            "--project",
            "project",
            "--first-prompt",
            "Research this",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreateAgent {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Research this"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-researcher",
            "investigate",
            "--project",
            "project",
            "-d",
            "Short prompt",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreateResearcher {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Short prompt"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-operator",
            "operate",
            "--project",
            "project",
            "--first-prompt",
            "Do the thing",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreateOperator {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Do the thing"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-reviewer",
            "--name",
            "review",
            "--project",
            "project",
            "--branch",
            "repo:branch",
            "--first-prompt",
            "Review this",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreateReviewer {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Review this"
        ));

        let parsed = Cli::try_parse_from([
            "agman",
            "create-tester",
            "--name",
            "test",
            "--project",
            "project",
            "--branch",
            "repo:branch",
            "-d",
            "Test this",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::CreateTester {
                first_prompt: Some(ref prompt),
                ..
            }) if prompt == "Test this"
        ));
    }

    #[test]
    fn agent_commands_reject_description_alias() {
        let cases: &[&[&str]] = &[
            &[
                "agman",
                "create-agent",
                "--kind",
                "reviewer",
                "--name",
                "review",
                "--project",
                "project",
                "--branch",
                "repo:branch",
                "--description",
                "Alias prompt",
            ],
            &[
                "agman",
                "create-researcher",
                "investigate",
                "--project",
                "project",
                "--description",
                "Research this",
            ],
            &[
                "agman",
                "create-operator",
                "operate",
                "--project",
                "project",
                "--description",
                "Do the thing",
            ],
            &[
                "agman",
                "create-reviewer",
                "--name",
                "review",
                "--project",
                "project",
                "--branch",
                "repo:branch",
                "--description",
                "Review this",
            ],
            &[
                "agman",
                "create-tester",
                "--name",
                "test",
                "--project",
                "project",
                "--branch",
                "repo:branch",
                "--description",
                "Test this",
            ],
        ];

        for args in cases {
            let err = match Cli::try_parse_from(*args) {
                Ok(_) => panic!("{args:?} unexpectedly parsed"),
                Err(err) => err,
            };
            assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{args:?}");
        }
    }
}
