mod cli;
mod logging;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;

use agman::agent_model::TesterCapabilities;
use agman::config::Config;
use agman::supervisor;
use agman::task::Task;
use agman::tmux::Tmux;
use agman::use_cases;
use cli::{AgentKindArg, Cli, Commands};
use tui::run_tui;

fn resolve_text_arg(
    value: Option<&str>,
    file: Option<&std::path::Path>,
    field_name: &str,
) -> Result<String> {
    use std::io::{IsTerminal, Read as _};

    if let Some(path) = file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {field_name} file: {}", path.display()));
    }

    if let Some(val) = value {
        if val == "-" {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .with_context(|| format!("Failed to read {field_name} from stdin"))?;
            return Ok(buf);
        }
        if let Some(path) = val.strip_prefix('@') {
            return std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read {field_name} from file: {path}"));
        }
        return Ok(val.to_string());
    }

    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .with_context(|| format!("Failed to read {field_name} from stdin"))?;
        return Ok(buf);
    }

    anyhow::bail!("No {field_name} provided. Pass as argument, --file, or pipe via stdin.");
}

fn main() -> Result<()> {
    // Setup better panic handling
    better_panic::install();

    // Load config
    let config = Config::load()?;

    // Rotate log file before setting up logging (keeps it under 1000 lines)
    logging::rotate_log(&config);

    // Initialize file-based logging to ~/.agman/agman.log
    logging::setup_logging(&config)?;

    // Parse CLI
    let cli = Cli::parse();

    tracing::debug!(command = ?cli.command, "dispatching command");
    use_cases::purge_chief_of_staff_agents(&config);

    match cli.command {
        Some(Commands::Init { force }) => {
            config.init_default_files(force)?;
            println!("agman initialized at {}", config.base_dir.display());
            Ok(())
        }

        Some(Commands::SendMessage {
            target,
            message,
            file,
            from,
        }) => cmd_send_message(
            &config,
            &target,
            message.as_deref(),
            file.as_deref(),
            from.as_deref(),
        ),

        Some(Commands::CreateProject {
            name,
            description,
            initial_message,
            file,
        }) => cmd_create_project(
            &config,
            &name,
            description.as_deref(),
            initial_message.as_deref(),
            file.as_deref(),
        ),

        Some(Commands::ListProjects) => cmd_list_projects(&config),

        Some(Commands::ProjectStatus { name }) => cmd_project_status(&config, &name),

        Some(Commands::DeleteProject { name }) => cmd_delete_project(&config, &name),

        Some(Commands::ListTemplates) => cmd_list_templates(&config),

        Some(Commands::GetTemplate { name }) => cmd_get_template(&config, &name),

        Some(Commands::CreateTemplate { name, body, file }) => {
            cmd_create_template(&config, &name, body.as_deref(), file.as_deref())
        }

        Some(Commands::ArchiveTask { task_id, save }) => cmd_archive_task(&config, &task_id, save),

        Some(Commands::CreatePmTask {
            project,
            repo,
            task_name,
            first_prompt,
        }) => cmd_create_pm_task(&config, &project, &repo, &task_name, first_prompt),

        Some(Commands::ListPmTasks { project }) => cmd_list_pm_tasks(&config, &project),

        Some(Commands::Status) => cmd_status(&config),

        Some(Commands::TaskInfo { task_id }) => cmd_task_info(&config, &task_id),

        Some(Commands::LinkPr {
            task_id,
            pr,
            owned,
            not_owned,
            author,
            force,
            from_sidecar,
        }) => cmd_link_pr(
            &config,
            &task_id,
            pr.as_deref(),
            owned && !not_owned,
            author,
            force,
            from_sidecar,
        ),

        Some(Commands::TaskLog { task_id, tail }) => cmd_task_log(&config, &task_id, tail),

        Some(Commands::CreateAgent {
            kind,
            name,
            project,
            description,
            repo,
            branch_for_researcher,
            task,
            branch_pair,
            browser,
        }) => {
            let project = project.as_str();
            match kind {
                AgentKindArg::Researcher => {
                    if browser {
                        anyhow::bail!("--browser is tester-only");
                    }
                    if !branch_pair.is_empty() {
                        anyhow::bail!(
                            "--branch <repo>:<branch> is reviewer/tester-only; use --branch-for-researcher \
                             with a plain branch name for researchers"
                        );
                    }
                    cmd_create_researcher(
                        &config,
                        project,
                        &name,
                        repo,
                        branch_for_researcher,
                        task,
                        description,
                    )
                }
                AgentKindArg::Operator => {
                    if browser {
                        anyhow::bail!("--browser is tester-only");
                    }
                    if !branch_pair.is_empty() {
                        anyhow::bail!(
                            "--branch <repo>:<branch> is reviewer/tester-only; use --branch-for-researcher \
                             with a plain branch name for operators"
                        );
                    }
                    cmd_create_operator(
                        &config,
                        project,
                        &name,
                        repo,
                        branch_for_researcher,
                        task,
                        description,
                    )
                }
                AgentKindArg::Reviewer => {
                    if browser {
                        anyhow::bail!("--browser is tester-only");
                    }
                    if repo.is_some() || branch_for_researcher.is_some() || task.is_some() {
                        anyhow::bail!(
                            "--repo / --branch-for-researcher / --task are researcher/operator-only; \
                             reviewers use --branch <repo>:<branch> (repeatable)"
                        );
                    }
                    cmd_create_reviewer(&config, project, &name, branch_pair, description)
                }
                AgentKindArg::Tester => {
                    if repo.is_some() || branch_for_researcher.is_some() || task.is_some() {
                        anyhow::bail!(
                            "--repo / --branch-for-researcher / --task are researcher/operator-only; \
                             testers use --branch <repo>:<branch> (repeatable)"
                        );
                    }
                    cmd_create_tester(&config, project, &name, branch_pair, browser, description)
                }
            }
        }

        Some(Commands::ListAgents { project, kind }) => {
            cmd_list_agents(&config, Some(project.as_str()), kind)
        }

        Some(Commands::ArchiveAgent { name, project }) => {
            cmd_archive_agent(&config, &project, &name)
        }

        Some(Commands::AttachAgent {
            project,
            name,
            task,
            role_label,
        }) => cmd_attach_agent(&config, &project, &name, &task, role_label),

        Some(Commands::MoveAgent {
            project,
            name,
            task,
            role_label,
        }) => cmd_move_agent(&config, &project, &name, &task, role_label),

        Some(Commands::DetachAgent { project, name }) => cmd_detach_agent(&config, &project, &name),

        Some(Commands::CreateResearcher {
            name,
            project,
            repo,
            branch,
            task,
            description,
        }) => cmd_create_researcher(&config, &project, &name, repo, branch, task, description),

        Some(Commands::CreateOperator {
            name,
            project,
            repo,
            branch,
            task,
            description,
        }) => cmd_create_operator(&config, &project, &name, repo, branch, task, description),

        Some(Commands::CreateReviewer {
            name,
            project,
            branch_pair,
            description,
        }) => cmd_create_reviewer(&config, &project, &name, branch_pair, description),

        Some(Commands::CreateTester {
            name,
            project,
            branch_pair,
            browser,
            description,
        }) => cmd_create_tester(&config, &project, &name, branch_pair, browser, description),

        Some(Commands::ListResearchers { project }) => cmd_list_agents(
            &config,
            Some(project.as_str()),
            Some(AgentKindArg::Researcher),
        ),

        Some(Commands::ArchiveResearcher { name, project }) => {
            cmd_archive_agent(&config, &project, &name)
        }

        Some(Commands::RespawnAgent {
            target,
            force,
            timeout,
        }) => cmd_respawn_agent(&config, &target, force, timeout),

        Some(Commands::Restart) => cmd_restart(),

        None => {
            // No subcommand - launch TUI
            config.ensure_dirs()?;

            // Check that all required tools are on $PATH
            let missing = agman::use_cases::check_dependencies(&config);
            if !missing.is_empty() {
                eprintln!("Error: the following required tools are not installed:\n");
                for tool in &missing {
                    eprintln!(
                        "  - {}  ({})",
                        tool,
                        agman::use_cases::install_hint(&config, tool)
                    );
                }
                eprintln!("\nPlease install the missing tools and try again.");
                std::process::exit(1);
            }

            run_tui(config)
        }
    }
}

// ---------------------------------------------------------------------------
// Chief of Staff & PM command handlers
// ---------------------------------------------------------------------------

fn cmd_send_message(
    config: &Config,
    target: &str,
    message: Option<&str>,
    file: Option<&std::path::Path>,
    from: Option<&str>,
) -> Result<()> {
    let resolved = resolve_text_arg(message, file, "message")?;
    let sender = from.unwrap_or("unknown");
    use_cases::send_message(config, target, sender, resolved.trim_end())?;
    println!("Message sent to '{}'", target);
    Ok(())
}

fn cmd_create_project(
    config: &Config,
    name: &str,
    description: Option<&str>,
    initial_message: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<()> {
    let desc = description.unwrap_or("");

    // Resolve --initial-message / --file. The user may pass neither (project starts
    // with empty inbox), or one of inline text / @path / - / --file.
    let initial_owned: Option<String> = if initial_message.is_some() || file.is_some() {
        let resolved = resolve_text_arg(initial_message, file, "initial-message")?;
        Some(resolved)
    } else {
        None
    };
    let initial_trimmed: Option<&str> = initial_owned
        .as_deref()
        .map(|s| s.trim_end())
        .filter(|s| !s.is_empty());

    let project = use_cases::create_project(config, name, desc, initial_trimmed)?;
    println!(
        "Project '{}' created at {}",
        project.meta.name,
        project.dir.display()
    );
    if initial_trimmed.is_some() {
        println!("Initial message sent to PM inbox.");
    }
    Ok(())
}

fn cmd_list_templates(config: &Config) -> Result<()> {
    let templates = agman::templates::list_templates(config)?;
    if templates.is_empty() {
        println!("No templates. Add one with `agman create-template <name> --file <path>`.");
        println!("Templates live in {}", config.templates_dir().display());
        return Ok(());
    }

    println!("{:<24} DESCRIPTION", "NAME");
    println!("{}", "-".repeat(60));
    for t in &templates {
        println!("{:<24} {}", t.name, t.description);
    }
    Ok(())
}

fn cmd_get_template(config: &Config, name: &str) -> Result<()> {
    let body = agman::templates::read_template(config, name)?;
    print!("{body}");
    Ok(())
}

fn cmd_create_template(
    config: &Config,
    name: &str,
    body: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<()> {
    let resolved = resolve_text_arg(body, file, "template body")?;
    agman::templates::write_template(config, name, &resolved)?;
    println!(
        "Template '{}' saved to {}",
        name,
        config.template_path(name).display()
    );
    Ok(())
}

fn cmd_list_projects(config: &Config) -> Result<()> {
    let projects = use_cases::list_projects(config)?;
    if projects.is_empty() {
        println!("No projects.");
        return Ok(());
    }

    let archived = Task::list_archived(config);
    println!("{:<20} {:<8} {:<8}", "NAME", "TASKS", "ARCHIVED");
    println!("{}", "-".repeat(38));
    for p in &projects {
        let tasks = use_cases::list_project_tasks(config, &p.meta.name).unwrap_or_default();
        let archived_count = archived
            .iter()
            .filter(|t| t.meta.project.as_deref() == Some(&p.meta.name))
            .count();
        println!(
            "{:<20} {:<8} {:<8}",
            p.meta.name,
            tasks.len(),
            archived_count
        );
    }
    Ok(())
}

fn cmd_project_status(config: &Config, name: &str) -> Result<()> {
    let status = use_cases::project_status(config, name)?;
    println!("Project: {}", status.project.meta.name);
    println!("Description: {}", status.project.meta.description);
    println!("Created: {}", status.project.meta.created_at);
    println!(
        "Tasks: {} total, {} archived",
        status.total_tasks, status.archived_tasks
    );

    let tasks = use_cases::list_project_tasks(config, name)?;
    if !tasks.is_empty() {
        println!("\n{:<30} {:<20}", "TASK", "UPDATED");
        println!("{}", "-".repeat(52));
        for t in &tasks {
            println!(
                "{:<30} {:<20}",
                t.meta.task_id(),
                t.meta.updated_at.format("%Y-%m-%d %H:%M")
            );
        }
    }
    Ok(())
}

fn cmd_delete_project(config: &Config, name: &str) -> Result<()> {
    use_cases::delete_project(config, name)?;
    println!("Project '{}' deleted. All tasks have been archived.", name);
    Ok(())
}

fn cmd_archive_task(config: &Config, task_id: &str, save: bool) -> Result<()> {
    let mut task = Task::load_by_id(config, task_id)?;

    if task.meta.archived_at.is_some() {
        anyhow::bail!("Task '{}' is already archived", task.meta.task_id());
    }

    let display_id = task.meta.task_id();
    use_cases::archive_task(config, &mut task, save)?;

    let suffix = if save { " (saved)" } else { "" };
    tracing::info!(task_id = %display_id, save, "archived task via CLI");
    println!("Task '{}' archived{}.", display_id, suffix);

    Ok(())
}

fn cmd_create_pm_task(
    config: &Config,
    project: &str,
    repo: &str,
    task_name: &str,
    first_prompt: Option<String>,
) -> Result<()> {
    // Reject protected branch names
    if matches!(task_name, "main" | "master" | "develop") {
        anyhow::bail!(
            "task-name should describe the task, e.g. 'fix-login-bug', not a base branch"
        );
    }

    // Check if branch already exists
    let output = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            &format!("refs/heads/{}", task_name),
        ])
        .output()?;
    if output.status.success() {
        anyhow::bail!(
            "Branch '{}' already exists — pick a unique task name",
            task_name
        );
    }

    let first_prompt = match first_prompt {
        Some(prompt) => Some(resolve_text_arg(Some(&prompt), None, "first-prompt")?),
        None => None,
    };

    let mut task =
        use_cases::create_pm_task(config, project, repo, task_name, first_prompt.as_deref())?;
    let task_id = task.meta.task_id();

    supervisor::ensure_task_tmux(config, &task)
        .with_context(|| format!("failed to prepare tmux for PM task '{}'", task_id))?;
    supervisor::launch_next_step(config, &mut task)
        .with_context(|| format!("failed to launch agent for PM task '{}'", task_id))?;

    println!("Task '{}' created in project '{}'", task_id, project);
    Ok(())
}

fn cmd_list_pm_tasks(config: &Config, project: &str) -> Result<()> {
    let tasks = use_cases::list_project_tasks(config, project)?;

    if tasks.is_empty() {
        println!("No tasks in project '{}'.", project);
        return Ok(());
    }

    println!("{:<30} {:<20}", "TASK", "UPDATED");
    println!("{}", "-".repeat(52));
    for t in &tasks {
        println!(
            "{:<30} {:<20}",
            t.meta.task_id(),
            t.meta.updated_at.format("%Y-%m-%d %H:%M")
        );
    }

    let archived_count = Task::list_archived(config)
        .iter()
        .filter(|t| t.meta.project.as_deref() == Some(project))
        .count();
    if archived_count > 0 {
        println!("(+{} archived)", archived_count);
    }

    Ok(())
}

fn cmd_task_info(config: &Config, task_id: &str) -> Result<()> {
    let text = use_cases::get_task_info_text(config, task_id)?;
    println!("{}", text);
    Ok(())
}

fn cmd_link_pr(
    config: &Config,
    task_id: &str,
    pr: Option<&str>,
    owned: bool,
    author: Option<String>,
    force: bool,
    from_sidecar: bool,
) -> Result<()> {
    let linked = if from_sidecar {
        if pr.is_some() {
            anyhow::bail!("pass either a PR reference or --from-sidecar, not both");
        }
        use_cases::link_task_pr_from_sidecar(config, task_id, owned, author, force)?
    } else {
        let pr = pr.ok_or_else(|| {
            anyhow::anyhow!("missing PR reference; pass a PR number, PR URL, or --from-sidecar")
        })?;
        use_cases::link_task_pr(config, task_id, pr, owned, author, force)?
    };

    println!(
        "Task '{}' linked to PR #{}: {}",
        task_id, linked.number, linked.url
    );
    Ok(())
}

fn cmd_task_log(config: &Config, task_id: &str, tail: usize) -> Result<()> {
    let text = use_cases::get_task_log_tail(config, task_id, tail)?;
    if text.is_empty() {
        println!("(no log output)");
    } else {
        println!("{}", text);
    }
    Ok(())
}

fn format_relative_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);
    let secs = duration.num_seconds().max(0);

    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn format_task_line(t: &use_cases::TaskSummary) {
    let agent_str = match &t.engineer {
        Some(agent) => format!(" engineer:{}", agent),
        None => String::new(),
    };
    let time_str = format_relative_time(t.updated_at);
    println!("  {:<40} {:<24} {}", t.task_id, agent_str, time_str);
}

fn format_agents_line(agents: &[use_cases::AgentSummary]) -> String {
    agents
        .iter()
        .map(|a| {
            let kind_label = match a.kind {
                use_cases::AgentKindLabel::Engineer => "engineer",
                use_cases::AgentKindLabel::Researcher => "researcher",
                use_cases::AgentKindLabel::Operator => "operator",
                use_cases::AgentKindLabel::Reviewer => "reviewer",
                use_cases::AgentKindLabel::Tester => "tester",
            };
            format!("{} [{}] ({})", a.name, kind_label, a.status)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn cmd_status(config: &Config) -> Result<()> {
    let status = use_cases::aggregated_status(config)?;

    println!("=== agman status ===");

    for group in &status.projects {
        println!();
        let task_word = if group.tasks.len() == 1 {
            "task"
        } else {
            "tasks"
        };
        let archived_suffix = if group.archived_count > 0 {
            format!(", +{} archived", group.archived_count)
        } else {
            String::new()
        };
        println!(
            "{} ({} {}{})",
            group.name,
            group.tasks.len(),
            task_word,
            archived_suffix
        );
        for t in &group.tasks {
            format_task_line(t);
        }
        if !group.agents.is_empty() {
            println!("  Agents: {}", format_agents_line(&group.agents));
        }
    }

    if !status.unassigned.is_empty() || status.archived_unassigned > 0 {
        println!();
        let task_word = if status.unassigned.len() == 1 {
            "task"
        } else {
            "tasks"
        };
        let archived_suffix = if status.archived_unassigned > 0 {
            format!(", +{} archived", status.archived_unassigned)
        } else {
            String::new()
        };
        println!(
            "Unassigned ({} {}{})",
            status.unassigned.len(),
            task_word,
            archived_suffix
        );
        for t in &status.unassigned {
            format_task_line(t);
        }
    }

    Ok(())
}

fn cmd_create_researcher(
    config: &Config,
    project: &str,
    name: &str,
    repo: Option<String>,
    branch: Option<String>,
    task: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let desc = match description {
        Some(d) => resolve_text_arg(Some(&d), None, "description")?,
        None => String::new(),
    };
    let agent = use_cases::create_researcher(config, project, name, &desc, repo, branch, task)?;
    use_cases::start_agent_session(config, project, name, false)?;
    println!(
        "Researcher '{}' created for project '{}' (tmux: {})",
        agent.meta.name,
        agent.meta.project,
        Config::researcher_tmux_session(project, name),
    );
    Ok(())
}

fn cmd_create_operator(
    config: &Config,
    project: &str,
    name: &str,
    repo: Option<String>,
    branch: Option<String>,
    task: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let desc = match description {
        Some(d) => resolve_text_arg(Some(&d), None, "description")?,
        None => String::new(),
    };
    let agent = use_cases::create_operator(config, project, name, &desc, repo, branch, task)?;
    use_cases::start_agent_session(config, project, name, false)?;
    println!(
        "Operator '{}' created for project '{}' (tmux: {})",
        agent.meta.name,
        agent.meta.project,
        Config::operator_tmux_session(project, name),
    );
    Ok(())
}

fn cmd_create_reviewer(
    config: &Config,
    project: &str,
    name: &str,
    branch_pairs: Vec<String>,
    description: Option<String>,
) -> Result<()> {
    let desc = match description {
        Some(d) => resolve_text_arg(Some(&d), None, "description")?,
        None => String::new(),
    };
    let branches = parse_branch_pairs(&branch_pairs)?;
    let spec = use_cases::WorktreeSpec {
        branches,
        parent_dir: None,
    };
    let agent = use_cases::create_reviewer(config, project, name, &desc, spec)?;
    use_cases::start_agent_session(config, project, name, false)?;
    println!(
        "Reviewer '{}' created for project '{}' (tmux: {})",
        agent.meta.name,
        agent.meta.project,
        Config::reviewer_tmux_session(project, name),
    );
    Ok(())
}

fn cmd_create_tester(
    config: &Config,
    project: &str,
    name: &str,
    branch_pairs: Vec<String>,
    browser: bool,
    description: Option<String>,
) -> Result<()> {
    let desc = match description {
        Some(d) => resolve_text_arg(Some(&d), None, "description")?,
        None => String::new(),
    };
    let branches = parse_branch_pairs(&branch_pairs)?;
    let spec = use_cases::WorktreeSpec {
        branches,
        parent_dir: None,
    };
    let capabilities = TesterCapabilities { browser };
    let agent = use_cases::create_tester(config, project, name, &desc, spec, capabilities)?;
    use_cases::start_agent_session(config, project, name, false)?;
    println!(
        "Tester '{}' created for project '{}' (tmux: {})",
        agent.meta.name,
        agent.meta.project,
        Config::tester_tmux_session(project, name),
    );
    Ok(())
}

fn parse_branch_pairs(branch_pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut branches = Vec::with_capacity(branch_pairs.len());
    for pair in branch_pairs {
        let (repo, branch) = pair.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("--branch must be `<repo>:<branch>` (got `{}`)", pair)
        })?;
        if repo.is_empty() || branch.is_empty() {
            anyhow::bail!("--branch must be `<repo>:<branch>` (got `{}`)", pair);
        }
        branches.push((repo.to_string(), branch.to_string()));
    }
    Ok(branches)
}

fn cmd_list_agents(
    config: &Config,
    project: Option<&str>,
    kind: Option<AgentKindArg>,
) -> Result<()> {
    let kind_label = kind.map(|k| match k {
        AgentKindArg::Researcher => use_cases::AgentKindLabel::Researcher,
        AgentKindArg::Operator => use_cases::AgentKindLabel::Operator,
        AgentKindArg::Reviewer => use_cases::AgentKindLabel::Reviewer,
        AgentKindArg::Tester => use_cases::AgentKindLabel::Tester,
    });
    let agents = use_cases::list_agents(config, project, kind_label)?;
    if agents.is_empty() {
        println!("No agents.");
        return Ok(());
    }

    println!(
        "{:<20} {:<10} {:<20} {:<10} {:<24} DESCRIPTION",
        "NAME", "KIND", "PROJECT", "STATUS", "CREATED"
    );
    println!("{}", "-".repeat(110));
    for a in &agents {
        let (session_name, kind_str) = match a.meta.kind {
            agman::agent_model::AgentKind::Engineer => (
                Config::engineer_tmux_session(&a.meta.project, &a.meta.name),
                "engineer",
            ),
            agman::agent_model::AgentKind::Researcher { .. } => (
                Config::researcher_tmux_session(&a.meta.project, &a.meta.name),
                "researcher",
            ),
            agman::agent_model::AgentKind::Operator { .. } => (
                Config::operator_tmux_session(&a.meta.project, &a.meta.name),
                "operator",
            ),
            agman::agent_model::AgentKind::Reviewer { .. } => (
                Config::reviewer_tmux_session(&a.meta.project, &a.meta.name),
                "reviewer",
            ),
            agman::agent_model::AgentKind::Tester { .. } => (
                Config::tester_tmux_session(&a.meta.project, &a.meta.name),
                "tester",
            ),
        };
        let status = if a.meta.status == agman::agent_model::AgentStatus::Archived {
            "archived"
        } else if Tmux::session_exists(&session_name) {
            "running"
        } else {
            "stopped"
        };
        let created = a.meta.created_at.format("%Y-%m-%d %H:%M");
        let desc = if a.meta.description.len() > 40 {
            format!("{}...", &a.meta.description[..37])
        } else {
            a.meta.description.clone()
        };
        println!(
            "{:<20} {:<10} {:<20} {:<10} {:<24} {}",
            a.meta.name, kind_str, a.meta.project, status, created, desc
        );
    }
    Ok(())
}

fn cmd_archive_agent(config: &Config, project: &str, name: &str) -> Result<()> {
    use_cases::archive_agent(config, project, name)?;
    println!("AgentRecord '{name}' in project '{project}' archived.");
    Ok(())
}

fn cmd_attach_agent(
    config: &Config,
    project: &str,
    name: &str,
    task_id: &str,
    role_label: Option<String>,
) -> Result<()> {
    let agent = use_cases::attach_agent_to_task(config, project, name, task_id, role_label)?;
    println!(
        "Agent '{}' attached to task '{}'.",
        agent.meta.name, task_id
    );
    Ok(())
}

fn cmd_move_agent(
    config: &Config,
    project: &str,
    name: &str,
    task_id: &str,
    role_label: Option<String>,
) -> Result<()> {
    let agent = use_cases::move_agent_to_task(config, project, name, task_id, role_label)?;
    println!("Agent '{}' moved to task '{}'.", agent.meta.name, task_id);
    Ok(())
}

fn cmd_detach_agent(config: &Config, project: &str, name: &str) -> Result<()> {
    let agent = use_cases::detach_agent_from_task(config, project, name)?;
    println!("Agent '{}' detached from its task.", agent.meta.name);
    Ok(())
}

fn cmd_respawn_agent(config: &Config, target: &str, force: bool, timeout: u64) -> Result<()> {
    println!(
        "Respawning agent '{}'{}...",
        target,
        if force { " (force)" } else { "" }
    );
    use_cases::respawn_agent(config, target, force, timeout)?;
    println!("Agent '{}' respawned successfully.", target);
    Ok(())
}

fn cmd_restart() -> Result<()> {
    let signal_file = dirs::home_dir()
        .context("could not determine home directory")?
        .join(".agman/.agman-restart");

    std::fs::write(&signal_file, "")
        .with_context(|| format!("failed to write signal file {}", signal_file.display()))?;

    tracing::info!("wrote .agman-restart signal file");
    println!("Restart signal sent. The TUI will restart momentarily.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_text_arg_literal() {
        let result = resolve_text_arg(Some("hello world"), None, "test").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn resolve_text_arg_at_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("input.md");
        std::fs::write(&file, "file contents\nline two\n").unwrap();

        let arg = format!("@{}", file.display());
        let result = resolve_text_arg(Some(&arg), None, "test").unwrap();
        assert_eq!(result, "file contents\nline two\n");
    }

    #[test]
    fn resolve_text_arg_file_param_takes_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("priority.txt");
        std::fs::write(&file, "from file param").unwrap();

        let result = resolve_text_arg(Some("ignored"), Some(file.as_path()), "test").unwrap();
        assert_eq!(result, "from file param");
    }

    #[test]
    fn resolve_text_arg_at_path_missing_file() {
        let result = resolve_text_arg(Some("@/nonexistent/path.md"), None, "test");
        assert!(result.is_err());
    }
}
