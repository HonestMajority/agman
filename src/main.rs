mod cli;
mod logging;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;

use agman::agent;
use agman::command;
use agman::config::Config;
use agman::flow::{self, Flow};
use agman::task::{Task, TaskStatus};
use agman::tmux::Tmux;
use agman::use_cases;
use cli::{Cli, Commands};
use tui::run_tui;

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
        Some(Commands::FlowRun { task_id }) => cmd_flow_run(&config, &task_id),

        Some(Commands::Continue {
            task_id,
            feedback,
        }) => cmd_continue(&config, &task_id, feedback.as_deref()),

        Some(Commands::RunCommand {
            task_id,
            command_id,
            branch,
        }) => cmd_run_command(&config, &task_id, &command_id, branch.as_deref()),

        Some(Commands::CommandFlowRun {
            task_id,
            command_id,
            branch,
        }) => cmd_command_flow_run(&config, &task_id, &command_id, branch.as_deref()),

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
        }) => cmd_send_message(&config, &target, message.as_deref(), file.as_deref(), from.as_deref()),

        Some(Commands::CreateProject { name, description }) => {
            cmd_create_project(&config, &name, description.as_deref())
        }

        Some(Commands::ListProjects) => cmd_list_projects(&config),

        Some(Commands::ProjectStatus { name }) => cmd_project_status(&config, &name),

        Some(Commands::DeleteProject { name }) => cmd_delete_project(&config, &name),

        Some(Commands::ArchiveTask { task_id, save }) => {
            cmd_archive_task(&config, &task_id, save)
        }

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

        Some(Commands::QueueFeedback { task_id, feedback }) => {
            cmd_queue_feedback(&config, &task_id, &feedback)
        }

        None => {
            // No subcommand - launch TUI
            config.ensure_dirs()?;

            // Check that all required tools are on $PATH
            let missing = agman::use_cases::check_dependencies();
            if !missing.is_empty() {
                eprintln!("Error: the following required tools are not installed:\n");
                for tool in &missing {
                    eprintln!(
                        "  - {}  ({})",
                        tool,
                        agman::use_cases::install_hint(tool)
                    );
                }
                eprintln!("\nPlease install the missing tools and try again.");
                std::process::exit(1);
            }

            run_tui(config)
        }
    }
}

fn cmd_flow_run(config: &Config, task_id: &str) -> Result<()> {
    config.init_default_files(false)?;

    let mut task = Task::load_by_id(config, task_id)?;

    println!(
        "Running flow '{}' for task '{}'",
        task.meta.flow_name,
        task.meta.task_id()
    );
    println!();

    let runner = agent::AgentRunner::new(config.clone());
    let result = runner.run_flow(&mut task)?;

    println!();
    println!("Flow finished with: {}", result);

    // If the flow paused for user input, just print a message and exit
    if result == flow::StopCondition::InputNeeded {
        println!("Task needs user input. Check the TUI to answer questions.");
        return Ok(());
    }

    // If review_after is enabled and the flow completed successfully, run review-pr
    // Re-read meta in case it was updated during the flow
    task.reload_meta()?;
    if task.meta.review_after {
        let is_success = result == flow::StopCondition::AgentDone
            || result == flow::StopCondition::TaskComplete;
        if is_success {
            println!();
            println!("Running post-flow review...");

            // Wipe REVIEW.md for a clean slate in all repos
            for repo in &task.meta.repos {
                let _ = Tmux::wipe_review_md(&repo.worktree_path);
            }

            // Reset review_after so it doesn't re-trigger
            task.meta.review_after = false;
            task.save_meta()?;

            // Load and run the review-pr command flow
            if let Ok(Some(cmd)) =
                command::StoredCommand::get_by_id(&config.commands_dir, "review-pr")
            {
                let review_flow = Flow::load(&cmd.flow_path)?;
                task.meta.flow_step = 0;
                task.save_meta()?;

                let review_result = runner.run_flow_with(&mut task, &review_flow)?;
                println!();
                println!("Review finished with: {}", review_result);
            } else {
                println!("Warning: review-pr command not found, skipping review");
            }
        } else {
            // Flow didn't complete successfully, reset flag
            task.meta.review_after = false;
            task.save_meta()?;
        }
    }

    Ok(())
}

fn cmd_continue(
    config: &Config,
    task_id: &str,
    feedback: Option<&str>,
) -> Result<()> {
    config.init_default_files(false)?;

    let flow_name = "continue";

    let mut task = Task::load_by_id(config, task_id)?;

    // Verify flow exists
    let flow_path = config.flow_path(flow_name);
    if !flow_path.exists() {
        anyhow::bail!("Flow '{}' does not exist", flow_name);
    }

    // Get feedback: from argument, or from existing FEEDBACK.md file
    let feedback_text = match feedback {
        Some(f) => {
            // Write feedback to file for the refiner
            task.write_feedback(f)?;
            f.to_string()
        }
        None => {
            // Read from existing FEEDBACK.md
            let existing = task.read_feedback()?;
            if existing.trim().is_empty() {
                anyhow::bail!("No feedback provided and FEEDBACK.md is empty");
            }
            existing
        }
    };

    if !task.meta.has_repos() {
        anyhow::bail!("Task '{}' has no repos configured yet", task.meta.task_id());
    }

    // Wipe REVIEW.md in all repo worktrees
    for repo in &task.meta.repos {
        let _ = Tmux::wipe_review_md(&repo.worktree_path);
    }

    // Log feedback to agent.log for history
    let _ = task.append_feedback_to_log(&feedback_text);

    println!("Continuing task: {}", task.meta.task_id());
    println!("Feedback: {}", feedback_text);
    println!();

    // Update task state
    task.meta.flow_name = flow_name.to_string();
    task.reset_flow_step()?;
    task.update_status(TaskStatus::Running)?;

    // Ensure tmux sessions exist for all repos
    for repo in &task.meta.repos {
        println!("Ensuring tmux session for {}...", repo.repo_name);
        Tmux::ensure_session(&repo.tmux_session, &repo.worktree_path)?;
    }

    // For multi-repo tasks, also ensure the parent-dir session exists (the flow runs there)
    let tmux_target = if task.meta.is_multi_repo() {
        let parent_dir = task.meta.parent_dir.as_ref().expect("multi-repo task must have parent_dir");
        let session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
        if !Tmux::session_exists(&session) {
            println!("Recreating parent-dir tmux session...");
            Tmux::create_session_with_windows(&session, parent_dir)?;
        }
        session
    } else {
        task.meta.primary_repo().tmux_session.clone()
    };
    let flow_cmd = format!("agman flow-run {}", task.meta.task_id());
    Tmux::send_keys_to_window(&tmux_target, "agman", &flow_cmd)?;

    println!("Flow started in tmux.");
    println!("To watch: agman attach {}", task.meta.task_id());

    Ok(())
}

fn cmd_run_command(
    config: &Config,
    task_id: &str,
    command_id: &str,
    branch: Option<&str>,
) -> Result<()> {
    config.init_default_files(false)?;

    let task = Task::load_by_id(config, task_id)?;

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

    let mut task = task;

    // Load the command (validate it exists)
    let cmd = command::StoredCommand::get_by_id(&config.commands_dir, command_id)?
        .ok_or_else(|| anyhow::anyhow!("Command '{}' not found", command_id))?;

    println!("Running command: {}", cmd.name);
    println!("  Description: {}", cmd.description);
    println!("  Task: {}", task.meta.task_id());

    // If the command requires a branch arg, write it to .branch-target in the task dir
    if cmd.requires_arg.as_deref() == Some("branch") {
        let branch = branch.ok_or_else(|| {
            anyhow::anyhow!("Command '{}' requires --branch argument", command_id)
        })?;
        let branch_target_path = task.dir.join(".branch-target");
        std::fs::write(&branch_target_path, branch)?;
        println!("  Target branch: {}", branch);
    }

    println!();

    // Ensure tmux sessions exist for all repos
    for repo in &task.meta.repos {
        println!("Ensuring tmux session for {}...", repo.repo_name);
        Tmux::ensure_session(&repo.tmux_session, &repo.worktree_path)?;
    }

    // Update task to running state
    task.update_status(TaskStatus::Running)?;

    // Dispatch the flow execution to tmux (non-blocking) so the caller returns immediately.
    // For multi-repo tasks, also ensure the parent-dir session exists and dispatch there.
    let tmux_target = if task.meta.is_multi_repo() {
        let parent_dir = task.meta.parent_dir.as_ref().expect("multi-repo task must have parent_dir");
        let session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
        if !Tmux::session_exists(&session) {
            println!("Recreating parent-dir tmux session...");
            Tmux::create_session_with_windows(&session, parent_dir)?;
        }
        session
    } else {
        task.meta.primary_repo().tmux_session.clone()
    };
    let mut flow_cmd = format!(
        "agman command-flow-run {} {}",
        task.meta.task_id(),
        command_id
    );
    if let Some(b) = branch {
        flow_cmd.push_str(&format!(" --branch {}", b));
    }
    Tmux::send_keys_to_window(&tmux_target, "agman", &flow_cmd)?;

    println!("Command flow started in tmux.");
    println!("To watch: agman attach {}", task.meta.task_id());

    Ok(())
}

/// Run a stored command's flow inside tmux (blocking is fine here since this
/// runs in the tmux agman window, not in the TUI process).
fn cmd_command_flow_run(
    config: &Config,
    task_id: &str,
    command_id: &str,
    branch: Option<&str>,
) -> Result<()> {
    config.init_default_files(false)?;

    let mut task = Task::load_by_id(config, task_id)?;

    if !task.meta.has_repos() {
        anyhow::bail!("Task '{}' has no repos configured yet", task.meta.task_id());
    }

    // Load the command
    let cmd = command::StoredCommand::get_by_id(&config.commands_dir, command_id)?
        .ok_or_else(|| anyhow::anyhow!("Command '{}' not found", command_id))?;

    // Wipe REVIEW.md at the start of review-pr (and continue) flows for a clean slate
    if command_id == "review-pr" {
        for repo in &task.meta.repos {
            let _ = Tmux::wipe_review_md(&repo.worktree_path);
        }
    }

    println!("Running command: {}", cmd.name);
    println!("  Task: {}", task.meta.task_id());

    // If the command requires a branch arg, write it to .branch-target in the task dir
    // (may already be written by cmd_run_command, but handle the case where
    // command-flow-run is invoked directly)
    if cmd.requires_arg.as_deref() == Some("branch") {
        if let Some(b) = branch {
            let branch_target_path = task.dir.join(".branch-target");
            std::fs::write(&branch_target_path, b)?;
            println!("  Target branch: {}", b);
        }
    }

    println!();

    // Update task to running state
    task.update_status(TaskStatus::Running)?;

    // Load the flow from the command YAML (not from ~/.agman/flows/)
    let flow = Flow::load(&cmd.flow_path)?;

    println!("Starting command flow: {}", flow.name);
    println!();

    let runner = agent::AgentRunner::new(config.clone());

    // Save original flow settings so we can restore after the command flow
    let original_flow = task.meta.flow_name.clone();
    let original_step = task.meta.flow_step;

    task.meta.flow_step = 0;
    task.save_meta()?;

    // Run the pre-loaded flow directly (blocking — this runs in tmux)
    let result = runner.run_flow_with(&mut task, &flow)?;

    println!();
    println!("Command '{}' finished with: {}", cmd.name, result);

    // Check for post_action after successful completion
    let is_success = result == flow::StopCondition::AgentDone || result == flow::StopCondition::TaskComplete;
    if is_success && matches!(cmd.post_action.as_deref(), Some("archive_task") | Some("delete_task")) {
        println!();
        println!("Post-action: archiving task after successful merge...");

        let task_id = task.meta.task_id();

        // Collect all tmux sessions to kill last (we may be running in one).
        // For multi-repo tasks, also include the parent-dir session.
        let mut tmux_sessions: Vec<String> = task.meta.repos.iter().map(|r| r.tmux_session.clone()).collect();
        if task.meta.is_multi_repo() {
            tmux_sessions.push(Config::tmux_session_name(&task.meta.name, &task.meta.branch_name));
        }

        // Archive the task (removes worktrees/branches, sets archived_at)
        use_cases::archive_task(config, &mut task, false)?;

        println!("Task '{}' archived after successful merge.", task_id);

        // Kill tmux sessions LAST — this process runs inside a tmux session,
        // so killing it will terminate us. All cleanup must happen before this.
        println!("  Killing tmux sessions...");
        for session in &tmux_sessions {
            let _ = Tmux::kill_session(session);
        }
    } else {
        // Restore original flow settings (only if we're NOT archiving the task)
        task.meta.flow_name = original_flow;
        task.meta.flow_step = original_step;
        task.save_meta()?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CEO & PM command handlers
// ---------------------------------------------------------------------------

fn cmd_send_message(
    config: &Config,
    target: &str,
    message: Option<&str>,
    file: Option<&std::path::Path>,
    from: Option<&str>,
) -> Result<()> {
    use std::io::{IsTerminal, Read as _};

    let resolved = if let Some(path) = file {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read message file: {}", path.display()))?
    } else if let Some(msg) = message {
        msg.to_string()
    } else if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)
            .context("Failed to read message from stdin")?;
        buf
    } else {
        anyhow::bail!("No message provided. Pass as argument, --file, or pipe via stdin.");
    };

    let sender = from.unwrap_or("unknown");
    use_cases::send_message(config, target, sender, resolved.trim_end())?;
    println!("Message sent to '{}'", target);
    Ok(())
}

fn cmd_create_project(config: &Config, name: &str, description: Option<&str>) -> Result<()> {
    let desc = description.unwrap_or("");
    let project = use_cases::create_project(config, name, desc)?;
    println!("Project '{}' created at {}", project.meta.name, project.dir.display());
    Ok(())
}

fn cmd_list_projects(config: &Config) -> Result<()> {
    let projects = use_cases::list_projects(config)?;
    if projects.is_empty() {
        println!("No projects.");
        return Ok(());
    }

    let archived = Task::list_archived(config);
    println!("{:<20} {:<8} {:<8} {:<8}", "NAME", "TASKS", "ACTIVE", "ARCHIVED");
    println!("{}", "-".repeat(48));
    for p in &projects {
        let tasks = use_cases::list_project_tasks(config, &p.meta.name)
            .unwrap_or_default();
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
    if status.archived_tasks > 0 {
        println!(
            "Tasks: {} total, {} active, +{} archived",
            status.total_tasks, status.active_tasks, status.archived_tasks
        );
    } else {
        println!("Tasks: {} total, {} active", status.total_tasks, status.active_tasks);
    }

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
        let parent_session =
            Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
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
        .args(["rev-parse", "--verify", &format!("refs/heads/{}", task_name)])
        .output()?;
    if output.status.success() {
        anyhow::bail!(
            "Branch '{}' already exists — pick a unique task name",
            task_name
        );
    }

    let task = use_cases::create_pm_task(
        config,
        project,
        repo,
        task_name,
        description.as_deref().unwrap_or(""),
    )?;
    let task_id = task.meta.task_id();

    let worktree_path = task.meta.primary_repo().worktree_path.clone();
    let session = &task.meta.primary_repo().tmux_session;

    Tmux::create_session_with_windows(session, &worktree_path)?;
    let _ = Tmux::add_review_window(session, &worktree_path);

    let flow_cmd = format!("agman flow-run {}", task_id);
    tracing::info!(task_id = %task_id, "dispatching flow-run for PM task");
    let _ = Tmux::send_keys_to_window(session, "agman", &flow_cmd);

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

fn cmd_status(config: &Config) -> Result<()> {
    let status = use_cases::aggregated_status(config)?;

    println!("=== agman status ===");

    for group in &status.projects {
        println!();
        let task_word = if group.tasks.len() == 1 { "task" } else { "tasks" };
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
            let step_str = match t.total_steps {
                Some(total) => format!("step {}/{}", t.flow_step, total),
                None => format!("step {}", t.flow_step),
            };
            let agent_str = match &t.current_agent {
                Some(agent) => format!(" ({})", agent),
                None => String::new(),
            };
            let time_str = format_relative_time(t.updated_at);
            println!(
                "  {:<40} {:<14} {}{:<20} {}",
                t.task_id,
                format!("{}", t.status),
                step_str,
                agent_str,
                time_str
            );
        }
    }

    if !status.unassigned.is_empty() || status.archived_unassigned > 0 {
        println!();
        let task_word = if status.unassigned.len() == 1 { "task" } else { "tasks" };
        let archived_suffix = if status.archived_unassigned > 0 {
            format!(", +{} archived", status.archived_unassigned)
        } else {
            String::new()
        };
        println!("Unassigned ({} {}{})", status.unassigned.len(), task_word, archived_suffix);
        for t in &status.unassigned {
            let step_str = match t.total_steps {
                Some(total) => format!("step {}/{}", t.flow_step, total),
                None => format!("step {}", t.flow_step),
            };
            let agent_str = match &t.current_agent {
                Some(agent) => format!(" ({})", agent),
                None => String::new(),
            };
            let time_str = format_relative_time(t.updated_at);
            println!(
                "  {:<40} {:<14} {}{:<20} {}",
                t.task_id,
                format!("{}", t.status),
                step_str,
                agent_str,
                time_str
            );
        }
    }

    Ok(())
}

fn cmd_queue_feedback(config: &Config, task_id: &str, feedback: &str) -> Result<()> {
    let task = Task::load_by_id(config, task_id)?;
    let count = use_cases::queue_feedback(&task, feedback)?;
    tracing::info!(task_id = %task_id, count, "queued feedback via CLI");
    println!("Feedback queued for '{}' ({} item(s) in queue)", task_id, count);
    Ok(())
}
