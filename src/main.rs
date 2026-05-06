mod cli;
mod logging;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;

use agman::command;
use agman::config::Config;
use agman::supervisor;
use agman::task::{Task, TaskStatus};
use agman::tmux::Tmux;
use agman::use_cases;
use cli::{AssistantKindArg, Cli, Commands};
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

    match cli.command {
        Some(Commands::RunCommand {
            task_id,
            command_id,
            branch,
        }) => cmd_run_command(&config, &task_id, &command_id, branch.as_deref()),

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

        Some(Commands::StopTask { task_id }) => cmd_stop_task(&config, &task_id),

        Some(Commands::ArchiveTask { task_id, save }) => cmd_archive_task(&config, &task_id, save),

        Some(Commands::CreatePmTask {
            project,
            repo,
            task_name,
            description,
        }) => cmd_create_pm_task(&config, &project, &repo, &task_name, description),

        Some(Commands::ListPmTasks { project, status }) => {
            cmd_list_pm_tasks(&config, &project, status)
        }

        Some(Commands::Status) => cmd_status(&config),

        Some(Commands::TaskStatus { task_id }) => cmd_task_status(&config, &task_id),

        Some(Commands::TaskLog { task_id, tail }) => cmd_task_log(&config, &task_id, tail),

        Some(Commands::TaskCurrentPlan { task_id }) => cmd_task_current_plan(&config, &task_id),

        Some(Commands::Feedback {
            task_id,
            feedback,
            file,
        }) => cmd_queue_feedback(&config, &task_id, feedback.as_deref(), file.as_deref()),

        Some(Commands::CreateAssistant {
            kind,
            name,
            project,
            description,
            repo,
            branch_for_researcher,
            task,
            branch_pair,
        }) => {
            let project = project.as_deref().unwrap_or("chief-of-staff");
            match kind {
                AssistantKindArg::Researcher => {
                    if !branch_pair.is_empty() {
                        anyhow::bail!(
                            "--branch <repo>:<branch> is reviewer-only; use --branch-for-researcher \
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
                AssistantKindArg::Reviewer => {
                    if repo.is_some() || branch_for_researcher.is_some() || task.is_some() {
                        anyhow::bail!(
                            "--repo / --branch-for-researcher / --task are researcher-only; \
                             reviewers use --branch <repo>:<branch> (repeatable)"
                        );
                    }
                    cmd_create_reviewer(&config, project, &name, branch_pair, description)
                }
            }
        }

        Some(Commands::ListAssistants { project, cos, kind }) => {
            let filter = if cos {
                Some("chief-of-staff")
            } else {
                project.as_deref()
            };
            cmd_list_assistants(&config, filter, kind)
        }

        Some(Commands::ArchiveAssistant { name, project }) => {
            let project = project.as_deref().unwrap_or("chief-of-staff");
            cmd_archive_assistant(&config, project, &name)
        }

        Some(Commands::CreateResearcher {
            name,
            project,
            repo,
            branch,
            task,
            description,
        }) => {
            let project = project.as_deref().unwrap_or("chief-of-staff");
            cmd_create_researcher(&config, project, &name, repo, branch, task, description)
        }

        Some(Commands::CreateReviewer {
            name,
            project,
            branch_pair,
            description,
        }) => {
            let project = project.as_deref().unwrap_or("chief-of-staff");
            cmd_create_reviewer(&config, project, &name, branch_pair, description)
        }

        Some(Commands::ListResearchers { project, cos }) => {
            let filter = if cos {
                Some("chief-of-staff")
            } else {
                project.as_deref()
            };
            cmd_list_assistants(&config, filter, Some(AssistantKindArg::Researcher))
        }

        Some(Commands::ArchiveResearcher { name, project }) => {
            let project = project.as_deref().unwrap_or("chief-of-staff");
            cmd_archive_assistant(&config, project, &name)
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

fn cmd_run_command(
    config: &Config,
    task_id: &str,
    command_id: &str,
    branch: Option<&str>,
) -> Result<()> {
    config.init_default_files(false)?;

    let mut task = Task::load_by_id(config, task_id)?;

    // Guard: refuse create-pr if a PR is already linked
    if command_id == "create-pr" {
        if let Some(ref pr) = task.meta.linked_pr {
            println!("PR #{} already linked — use monitor-pr instead.", pr.number);
            return Ok(());
        }
    }

    if !task.meta.has_repos() {
        anyhow::bail!("Task '{}' has no repos configured yet", task.meta.task_id());
    }

    // Load the command (validate it exists + get metadata)
    let cmd = command::StoredCommand::get_by_id(&config.commands_dir, command_id)?
        .ok_or_else(|| anyhow::anyhow!("Command '{}' not found", command_id))?;

    // Validate the branch arg loudly at the CLI boundary — drain_queue silently
    // drops malformed command queue items, which would be a confusing UX here.
    if cmd.requires_arg.as_deref() == Some("branch") && branch.is_none() {
        anyhow::bail!("Command '{}' requires --branch argument", command_id);
    }

    println!("Running command: {}", cmd.name);
    println!("  Description: {}", cmd.description);
    println!("  Task: {}", task.meta.task_id());
    if let Some(b) = branch {
        println!("  Target branch: {}", b);
    }
    println!();

    if task.meta.status == TaskStatus::Running {
        let count = use_cases::queue_command(&mut task, config, command_id, branch)?;
        tracing::info!(task_id = %task_id, command_id, count, "queued command via CLI");
        println!(
            "Command queued for '{}' ({} item(s) in queue)",
            task_id, count
        );
    } else {
        supervisor::ensure_task_tmux(&task)
            .with_context(|| format!("failed to prepare tmux for command on '{}'", task_id))?;

        let count = use_cases::queue_command(&mut task, config, command_id, branch)?;
        tracing::info!(
            task_id = %task_id,
            command_id,
            count,
            "queued command via CLI; supervisor waking"
        );
        println!(
            "Command queued for '{}' ({} item(s) in queue); supervisor waking",
            task_id, count
        );
    }

    Ok(())
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
        println!("Initial message queued to PM inbox.");
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
    println!(
        "{:<20} {:<8} {:<8} {:<8}",
        "NAME", "TASKS", "ACTIVE", "ARCHIVED"
    );
    println!("{}", "-".repeat(48));
    for p in &projects {
        let tasks = use_cases::list_project_tasks(config, &p.meta.name).unwrap_or_default();
        let active = tasks
            .iter()
            .filter(|t| t.meta.status == TaskStatus::Running)
            .count();
        let archived_count = archived
            .iter()
            .filter(|t| t.meta.project.as_deref() == Some(&p.meta.name))
            .count();
        println!(
            "{:<20} {:<8} {:<8} {:<8}",
            p.meta.name,
            tasks.len(),
            active,
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
        "Tasks: {} active, {} archived",
        status.active_tasks, status.archived_tasks
    );

    let tasks = use_cases::list_project_tasks(config, name)?;
    if !tasks.is_empty() {
        println!("\n{:<30} {:<15}", "TASK", "STATUS");
        println!("{}", "-".repeat(45));
        for t in &tasks {
            println!("{:<30} {:<15}", t.meta.task_id(), t.meta.status);
        }
    }
    Ok(())
}

fn cmd_delete_project(config: &Config, name: &str) -> Result<()> {
    use_cases::delete_project(config, name)?;
    println!("Project '{}' deleted. All tasks have been archived.", name);
    Ok(())
}

fn cmd_stop_task(config: &Config, task_id: &str) -> Result<()> {
    let mut task = Task::load_by_id(config, task_id)?;

    // Send Ctrl+C to tmux sessions (best-effort)
    for repo in &task.meta.repos {
        if Tmux::session_exists(&repo.tmux_session) {
            if let Err(e) = Tmux::send_ctrl_c_to_window(&repo.tmux_session, "agman") {
                tracing::warn!(task_id = %task.meta.task_id(), error = %e, "failed to interrupt tmux session");
            }
        }
    }
    if task.meta.is_multi_repo() && task.meta.repos.is_empty() {
        let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
        if Tmux::session_exists(&parent_session) {
            if let Err(e) = Tmux::send_ctrl_c_to_window(&parent_session, "agman") {
                tracing::warn!(task_id = %task.meta.task_id(), error = %e, "failed to interrupt parent tmux session");
            }
        }
    }

    let display_id = task.meta.task_id();
    use_cases::stop_task(config, &mut task)?;

    tracing::info!(task_id = %display_id, "stopped task via CLI");
    println!("Task '{}' stopped.", display_id);

    Ok(())
}

fn cmd_archive_task(config: &Config, task_id: &str, save: bool) -> Result<()> {
    let mut task = Task::load_by_id(config, task_id)?;

    if task.meta.archived_at.is_some() {
        anyhow::bail!("Task '{}' is already archived", task.meta.task_id());
    }

    // Kill tmux sessions (best-effort)
    for repo in &task.meta.repos {
        let _ = Tmux::kill_session(&repo.tmux_session);
    }
    if task.meta.is_multi_repo() {
        let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
        let _ = Tmux::kill_session(&parent_session);
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
    description: Option<String>,
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

    let desc = match description {
        Some(d) => resolve_text_arg(Some(&d), None, "description")?,
        None => String::new(),
    };

    let mut task = use_cases::create_pm_task(config, project, repo, task_name, &desc)?;
    let task_id = task.meta.task_id();

    supervisor::ensure_task_tmux(&task)
        .with_context(|| format!("failed to prepare tmux for PM task '{}'", task_id))?;
    supervisor::launch_next_step(config, &mut task)
        .with_context(|| format!("failed to launch flow for PM task '{}'", task_id))?;

    println!("Task '{}' created in project '{}'", task_id, project);
    Ok(())
}

fn cmd_list_pm_tasks(
    config: &Config,
    project: &str,
    status: Option<cli::StatusFilter>,
) -> Result<()> {
    let mut tasks = use_cases::list_project_tasks(config, project)?;

    if let Some(filter) = status {
        let target = filter.to_task_status();
        tracing::debug!(project, %target, "filtering tasks by status");
        tasks.retain(|t| t.meta.status == target);
    }

    if tasks.is_empty() {
        println!("No tasks in project '{}'.", project);
        return Ok(());
    }

    println!("{:<30} {:<15} {:<20}", "TASK", "STATUS", "UPDATED");
    println!("{}", "-".repeat(65));
    for t in &tasks {
        println!(
            "{:<30} {:<15} {:<20}",
            t.meta.task_id(),
            t.meta.status,
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

fn cmd_task_status(config: &Config, task_id: &str) -> Result<()> {
    let text = use_cases::get_task_status_text(config, task_id)?;
    println!("{}", text);
    Ok(())
}

fn cmd_task_current_plan(config: &Config, task_id: &str) -> Result<()> {
    let text = use_cases::get_task_current_plan(config, task_id)?;
    if text.is_empty() {
        println!("(no plan)");
    } else {
        println!("{}", text);
    }
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

fn format_status_breakdown(tasks: &[use_cases::TaskSummary]) -> String {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for t in tasks {
        *counts.entry(format!("{}", t.status)).or_insert(0) += 1;
    }
    counts
        .iter()
        .map(|(k, v)| format!("{} {}", v, k))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_task_line(t: &use_cases::TaskSummary) {
    let step_str = match t.total_steps {
        Some(total) => format!("step {}/{}", t.flow_step, total),
        None => format!("step {}", t.flow_step),
    };
    let agent_str = match &t.current_agent {
        Some(agent) => format!(" ({})", agent),
        None => String::new(),
    };
    let time_str = format_relative_time(t.updated_at);
    let status_str = if t.queued_count > 0 {
        format!("{} (+{})", t.status, t.queued_count)
    } else {
        format!("{}", t.status)
    };
    println!(
        "  {:<40} {:<14} {}{:<20} {}",
        t.task_id, status_str, step_str, agent_str, time_str
    );
}

fn format_assistants_line(assistants: &[use_cases::AssistantSummary]) -> String {
    assistants
        .iter()
        .map(|a| {
            let kind_label = match a.kind {
                use_cases::AssistantKindLabel::Researcher => "researcher",
                use_cases::AssistantKindLabel::Reviewer => "reviewer",
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
        let breakdown = format_status_breakdown(&group.tasks);
        let archived_suffix = if group.archived_count > 0 {
            format!(", +{} archived", group.archived_count)
        } else {
            String::new()
        };
        println!(
            "{} ({} {}: {}{})",
            group.name,
            group.tasks.len(),
            task_word,
            breakdown,
            archived_suffix
        );
        for t in &group.tasks {
            format_task_line(t);
        }
        if !group.assistants.is_empty() {
            println!(
                "  Assistants: {}",
                format_assistants_line(&group.assistants)
            );
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

    if !status.chief_of_staff_assistants.is_empty() {
        println!();
        println!(
            "Chief of Staff assistants: {}",
            format_assistants_line(&status.chief_of_staff_assistants)
        );
    }

    Ok(())
}

fn cmd_queue_feedback(
    config: &Config,
    task_id: &str,
    feedback: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<()> {
    let resolved = resolve_text_arg(feedback, file, "feedback")?;
    let mut task = Task::load_by_id(config, task_id)?;
    let feedback = resolved.trim_end();

    if task.meta.status == TaskStatus::Running {
        let count = use_cases::queue_feedback(&mut task, config, feedback)?;
        tracing::info!(task_id = %task_id, count, "queued feedback via CLI");
        println!(
            "Feedback queued for '{}' ({} item(s) in queue)",
            task_id, count
        );
    } else {
        supervisor::ensure_task_tmux(&task)
            .with_context(|| format!("failed to prepare tmux for feedback on '{}'", task_id))?;

        let count = use_cases::queue_feedback(&mut task, config, feedback)?;
        tracing::info!(
            task_id = %task_id,
            count,
            "queued feedback via CLI; supervisor waking"
        );
        println!(
            "Feedback queued for '{}' ({} item(s) in queue); supervisor waking",
            task_id, count
        );
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
    let assistant = use_cases::create_researcher(config, project, name, &desc, repo, branch, task)?;
    use_cases::start_assistant_session(config, project, name, false)?;
    println!(
        "Researcher '{}' created for project '{}' (tmux: {})",
        assistant.meta.name,
        assistant.meta.project,
        Config::researcher_tmux_session(project, name),
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
    let mut branches = Vec::with_capacity(branch_pairs.len());
    for pair in &branch_pairs {
        let (repo, branch) = pair.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("--branch must be `<repo>:<branch>` (got `{}`)", pair)
        })?;
        if repo.is_empty() || branch.is_empty() {
            anyhow::bail!("--branch must be `<repo>:<branch>` (got `{}`)", pair);
        }
        branches.push((repo.to_string(), branch.to_string()));
    }
    let spec = use_cases::ReviewerSpec {
        branches,
        parent_dir: None,
    };
    let assistant = use_cases::create_reviewer(config, project, name, &desc, spec)?;
    use_cases::start_assistant_session(config, project, name, false)?;
    println!(
        "Reviewer '{}' created for project '{}' (tmux: {})",
        assistant.meta.name,
        assistant.meta.project,
        Config::reviewer_tmux_session(project, name),
    );
    Ok(())
}

fn cmd_list_assistants(
    config: &Config,
    project: Option<&str>,
    kind: Option<AssistantKindArg>,
) -> Result<()> {
    let kind_label = kind.map(|k| match k {
        AssistantKindArg::Researcher => use_cases::AssistantKindLabel::Researcher,
        AssistantKindArg::Reviewer => use_cases::AssistantKindLabel::Reviewer,
    });
    let assistants = use_cases::list_assistants(config, project, kind_label)?;
    if assistants.is_empty() {
        println!("No assistants.");
        return Ok(());
    }

    println!(
        "{:<20} {:<10} {:<20} {:<10} {:<24} DESCRIPTION",
        "NAME", "KIND", "PROJECT", "STATUS", "CREATED"
    );
    println!("{}", "-".repeat(110));
    for a in &assistants {
        let (session_name, kind_str) = match a.meta.kind {
            agman::assistant::AssistantKind::Researcher { .. } => (
                Config::researcher_tmux_session(&a.meta.project, &a.meta.name),
                "researcher",
            ),
            agman::assistant::AssistantKind::Reviewer { .. } => (
                Config::reviewer_tmux_session(&a.meta.project, &a.meta.name),
                "reviewer",
            ),
        };
        let status = if a.meta.status == agman::assistant::AssistantStatus::Archived {
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

fn cmd_archive_assistant(config: &Config, project: &str, name: &str) -> Result<()> {
    use_cases::archive_assistant(config, project, name)?;
    println!("Assistant '{name}' in project '{project}' archived.");
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
