mod agent;
mod cli;
mod config;
mod flow;
mod git;
mod task;
mod tmux;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::{self, Write};

use cli::{Cli, Commands};
use config::Config;
use flow::Flow;
use git::Git;
use task::{Task, TaskStatus};
use tmux::Tmux;
use tui::run_tui;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_target(false)
        .init();

    // Setup better panic handling
    better_panic::install();

    // Load config
    let config = Config::load()?;

    // Parse CLI
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::New {
            repo_name,
            branch_name,
            description,
            flow,
        }) => cmd_new(&config, &repo_name, &branch_name, &description, &flow),

        Some(Commands::List) => cmd_list(&config),

        Some(Commands::Delete { task_id, force }) => cmd_delete(&config, &task_id, force),

        Some(Commands::Run {
            task_id,
            agent,
            r#loop,
        }) => cmd_run(&config, &task_id, &agent, r#loop),

        Some(Commands::FlowRun { task_id }) => cmd_flow_run(&config, &task_id),

        Some(Commands::Pause { task_id }) => cmd_pause(&config, &task_id),

        Some(Commands::Resume { task_id }) => cmd_resume(&config, &task_id),

        Some(Commands::Attach { task_id }) => cmd_attach(&config, &task_id),

        Some(Commands::Init) => cmd_init(&config),

        None => {
            // No subcommand - launch TUI
            config.ensure_dirs()?;
            run_tui(config)
        }
    }
}

fn cmd_new(
    config: &Config,
    repo_name: &str,
    branch_name: &str,
    description: &str,
    flow_name: &str,
) -> Result<()> {
    config.init_default_files()?;

    // Check if repo exists
    let repo_path = config.repo_path(repo_name);
    if !repo_path.exists() {
        anyhow::bail!(
            "Repository '{}' does not exist at {}",
            repo_name,
            repo_path.display()
        );
    }

    // Check if task already exists
    let task_dir = config.task_dir(repo_name, branch_name);
    if task_dir.exists() {
        anyhow::bail!(
            "Task '{}--{}' already exists",
            repo_name,
            branch_name
        );
    }

    // Verify flow exists
    let flow_path = config.flow_path(flow_name);
    if !flow_path.exists() {
        anyhow::bail!("Flow '{}' does not exist", flow_name);
    }

    println!("Creating task: {}--{}", repo_name, branch_name);

    // Create worktree (this fetches origin and creates from origin/main)
    println!("Creating git worktree...");
    let worktree_path = Git::create_worktree(config, repo_name, branch_name)
        .context("Failed to create git worktree")?;

    // Run direnv allow
    println!("  Running direnv allow...");
    Git::direnv_allow(&worktree_path)?;

    // Create task
    println!("Creating task directory...");
    let task = Task::create(
        config,
        repo_name,
        branch_name,
        description,
        flow_name,
        worktree_path.clone(),
    )?;

    // Create tmux session with windows (nvim, lazygit, claude, zsh)
    println!("Creating tmux session with windows...");
    Tmux::create_session_with_windows(&task.meta.tmux_session, &worktree_path)?;

    // Start the flow running in the tmux claude window
    let task_id = task.meta.task_id();
    println!("Starting flow '{}' in background...", flow_name);
    let flow_cmd = format!("agman flow-run {}", task_id);
    Tmux::send_keys_to_window(&task.meta.tmux_session, "claude", &flow_cmd)?;

    println!();
    println!("Task created successfully!");
    println!("  Task ID:   {}", task_id);
    println!("  Worktree:  {}", worktree_path.display());
    println!("  Tmux:      {}", task.meta.tmux_session);
    println!("  Flow:      {}", flow_name);
    println!();
    println!("Flow is running in tmux. To watch: agman attach {}", task_id);

    Ok(())
}

fn cmd_list(config: &Config) -> Result<()> {
    let tasks = Task::list_all(config)?;

    if tasks.is_empty() {
        println!("No tasks found.");
        println!("Create one with: agman new <repo> <branch> \"description\"");
        return Ok(());
    }

    println!(
        "{:<30} {:<10} {:<12} {:<10}",
        "TASK", "STATUS", "AGENT", "UPDATED"
    );
    println!("{}", "-".repeat(65));

    for task in tasks {
        let status_icon = match task.meta.status {
            TaskStatus::Working => "●",
            TaskStatus::Paused => "◐",
            TaskStatus::Done => "✓",
            TaskStatus::Failed => "✗",
        };

        let task_id = task.meta.task_id();

        println!(
            "{} {:<28} {:<10} {:<12} {}",
            status_icon,
            task_id,
            task.meta.status,
            task.meta.current_agent.as_deref().unwrap_or("-"),
            task.time_since_update()
        );
    }

    Ok(())
}

fn cmd_delete(config: &Config, task_id: &str, force: bool) -> Result<()> {
    let task = Task::load_by_id(config, task_id)?;

    if !force {
        print!(
            "Delete task '{}'? This will remove the worktree and tmux session. [y/N] ",
            task.meta.task_id()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let task_id = task.meta.task_id();
    let repo_path = config.repo_path(&task.meta.repo_name);
    let worktree_path = task.meta.worktree_path.clone();
    let tmux_session = task.meta.tmux_session.clone();
    let branch_name = task.meta.branch_name.clone();

    println!("Deleting task: {}", task_id);

    // Kill tmux session
    println!("  Killing tmux session...");
    let _ = Tmux::kill_session(&tmux_session);

    // Remove worktree
    println!("  Removing git worktree...");
    let _ = Git::remove_worktree(&repo_path, &worktree_path);

    // Delete branch
    println!("  Deleting git branch...");
    let _ = Git::delete_branch(&repo_path, &branch_name);

    // Delete task directory
    println!("  Removing task directory...");
    task.delete(config)?;

    println!("Task '{}' deleted.", task_id);

    Ok(())
}

fn cmd_run(config: &Config, task_id: &str, agent_name: &str, loop_mode: bool) -> Result<()> {
    config.init_default_files()?;

    let mut task = Task::load_by_id(config, task_id)?;

    // Check if agent prompt exists
    let prompt_path = config.prompt_path(agent_name);
    if !prompt_path.exists() {
        anyhow::bail!("Agent '{}' does not exist", agent_name);
    }

    // Ensure tmux session exists
    if !Tmux::session_exists(&task.meta.tmux_session) {
        Tmux::create_session_with_windows(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    let runner = agent::AgentRunner::new(config.clone());

    if loop_mode {
        println!("Running agent '{}' in loop mode...", agent_name);
        let result = runner.run_agent_loop(&mut task, agent_name)?;
        println!("Agent loop finished with: {}", result);
    } else {
        println!("Running agent '{}'...", agent_name);
        runner.run_agent_in_tmux(&mut task, agent_name)?;
        println!(
            "Agent started in tmux session: {}",
            task.meta.tmux_session
        );
        println!("To attach: agman attach {}", task.meta.task_id());
    }

    Ok(())
}

fn cmd_flow_run(config: &Config, task_id: &str) -> Result<()> {
    config.init_default_files()?;

    let mut task = Task::load_by_id(config, task_id)?;

    println!("Running flow '{}' for task '{}'", task.meta.flow_name, task.meta.task_id());
    println!();

    let runner = agent::AgentRunner::new(config.clone());
    let result = runner.run_flow(&mut task)?;

    println!();
    println!("Flow finished with: {}", result);

    Ok(())
}

fn cmd_pause(config: &Config, task_id: &str) -> Result<()> {
    let mut task = Task::load_by_id(config, task_id)?;
    task.update_status(TaskStatus::Paused)?;
    println!("Task '{}' paused.", task.meta.task_id());
    Ok(())
}

fn cmd_resume(config: &Config, task_id: &str) -> Result<()> {
    let mut task = Task::load_by_id(config, task_id)?;
    task.update_status(TaskStatus::Working)?;

    // Ensure tmux session exists
    if !Tmux::session_exists(&task.meta.tmux_session) {
        println!("Recreating tmux session...");
        Tmux::create_session_with_windows(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    // Resume the flow by running the current agent
    let flow = Flow::load(&config.flow_path(&task.meta.flow_name))?;

    if let Some(step) = flow.get_step(task.meta.flow_step) {
        let agent_name = match step {
            flow::FlowStep::Agent(a) => a.agent.clone(),
            flow::FlowStep::Loop(l) => l.steps.first().map(|s| s.agent.clone()).unwrap_or_default(),
        };

        if !agent_name.is_empty() {
            let runner = agent::AgentRunner::new(config.clone());
            runner.run_agent_in_tmux(&mut task, &agent_name)?;
            println!(
                "Task '{}' resumed with agent '{}'.",
                task.meta.task_id(),
                agent_name
            );
        } else {
            println!("Task '{}' resumed.", task.meta.task_id());
        }
    } else {
        println!(
            "Task '{}' resumed (no more flow steps).",
            task.meta.task_id()
        );
    }

    Ok(())
}

fn cmd_attach(config: &Config, task_id: &str) -> Result<()> {
    let task = Task::load_by_id(config, task_id)?;

    if !Tmux::session_exists(&task.meta.tmux_session) {
        // Create session if it doesn't exist
        println!("Creating tmux session...");
        Tmux::create_session_with_windows(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    Tmux::attach_session(&task.meta.tmux_session)?;

    Ok(())
}

fn cmd_init(config: &Config) -> Result<()> {
    config.init_default_files()?;

    println!("Initialized agman at: {}", config.base_dir.display());
    println!();
    println!("Created directories:");
    println!("  {}/", config.tasks_dir.display());
    println!("  {}/", config.flows_dir.display());
    println!("  {}/", config.prompts_dir.display());
    println!();
    println!("Created default flows:");
    println!("  default.yaml");
    println!("  tdd.yaml");
    println!("  review.yaml");
    println!();
    println!("Created default agent prompts:");
    println!("  planner.md");
    println!("  coder.md");
    println!("  test-writer.md");
    println!("  tester.md");
    println!("  reviewer.md");
    println!();
    println!("You can customize these files to fit your workflow.");

    Ok(())
}
