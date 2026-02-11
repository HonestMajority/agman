mod cli;
mod logging;
mod tui;

use anyhow::Result;
use clap::Parser;

use agman::agent;
use agman::command;
use agman::config::Config;
use agman::flow::{self, Flow};
use agman::git::Git;
use agman::task::{Task, TaskStatus};
use agman::tmux::Tmux;
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

            // Wipe REVIEW.md for a clean slate
            let _ = Tmux::wipe_review_md(&task.meta.worktree_path);

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

    // Wipe REVIEW.md to start fresh
    let _ = Tmux::wipe_review_md(&task.meta.worktree_path);

    // Log feedback to agent.log for history
    let _ = task.append_feedback_to_log(&feedback_text);

    println!("Continuing task: {}", task.meta.task_id());
    println!("Feedback: {}", feedback_text);
    println!();

    // Update task state
    task.meta.flow_name = flow_name.to_string();
    task.reset_flow_step()?;
    task.update_status(TaskStatus::Running)?;

    // Ensure tmux session exists
    if !Tmux::session_exists(&task.meta.tmux_session) {
        println!("Recreating tmux session...");
        Tmux::create_session_with_windows(&task.meta.tmux_session, &task.meta.worktree_path)?;
        Tmux::add_review_window(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    // Start the flow in tmux
    let flow_cmd = format!("agman flow-run {}", task.meta.task_id());
    Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &flow_cmd)?;

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

    // Ensure tmux session exists
    if !Tmux::session_exists(&task.meta.tmux_session) {
        println!("Recreating tmux session...");
        Tmux::create_session_with_windows(&task.meta.tmux_session, &task.meta.worktree_path)?;
        Tmux::add_review_window(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    // Update task to running state
    task.update_status(TaskStatus::Running)?;

    // Dispatch the flow execution to tmux (non-blocking) so the caller returns immediately
    let mut flow_cmd = format!(
        "agman command-flow-run {} {}",
        task.meta.task_id(),
        command_id
    );
    if let Some(b) = branch {
        flow_cmd.push_str(&format!(" --branch {}", b));
    }
    Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &flow_cmd)?;

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

    // Load the command
    let cmd = command::StoredCommand::get_by_id(&config.commands_dir, command_id)?
        .ok_or_else(|| anyhow::anyhow!("Command '{}' not found", command_id))?;

    // Wipe REVIEW.md at the start of review-pr (and continue) flows for a clean slate
    if command_id == "review-pr" {
        let _ = Tmux::wipe_review_md(&task.meta.worktree_path);
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
    if is_success && cmd.post_action.as_deref() == Some("delete_task") {
        println!();
        println!("Post-action: deleting task after successful merge...");

        let task_id = task.meta.task_id();
        let repo_path = config.repo_path(&task.meta.repo_name);
        let worktree_path = task.meta.worktree_path.clone();
        let tmux_session = task.meta.tmux_session.clone();
        let branch_name = task.meta.branch_name.clone();

        // Remove worktree
        println!("  Removing git worktree...");
        let _ = Git::remove_worktree(&repo_path, &worktree_path);

        // Delete branch
        println!("  Deleting git branch...");
        let _ = Git::delete_branch(&repo_path, &branch_name);

        // Delete task directory
        println!("  Removing task directory...");
        task.delete(config)?;

        println!("Task '{}' deleted after successful merge.", task_id);

        // Kill tmux session LAST — this process runs inside the tmux session,
        // so killing it will terminate us. All cleanup must happen before this.
        println!("  Killing tmux session...");
        let _ = Tmux::kill_session(&tmux_session);
    } else {
        // Restore original flow settings (only if we're NOT deleting the task)
        task.meta.flow_name = original_flow;
        task.meta.flow_step = original_step;
        task.save_meta()?;
    }

    Ok(())
}
