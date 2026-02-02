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
            branch_name,
            description,
            flow,
        }) => cmd_new(&config, &branch_name, &description, &flow),

        Some(Commands::List) => cmd_list(&config),

        Some(Commands::Delete { branch_name, force }) => cmd_delete(&config, &branch_name, force),

        Some(Commands::Run {
            branch_name,
            agent,
            r#loop,
        }) => cmd_run(&config, &branch_name, &agent, r#loop),

        Some(Commands::Pause { branch_name }) => cmd_pause(&config, &branch_name),

        Some(Commands::Resume { branch_name }) => cmd_resume(&config, &branch_name),

        Some(Commands::Attach { branch_name }) => cmd_attach(&config, &branch_name),

        Some(Commands::Init) => cmd_init(&config),

        None => {
            // No subcommand - launch TUI
            config.ensure_dirs()?;
            run_tui(config)
        }
    }
}

fn cmd_new(config: &Config, branch_name: &str, description: &str, flow_name: &str) -> Result<()> {
    config.init_default_files()?;

    // Check if task already exists
    let task_dir = config.task_dir(branch_name);
    if task_dir.exists() {
        anyhow::bail!("Task '{}' already exists", branch_name);
    }

    // Verify flow exists
    let flow_path = config.flow_path(flow_name);
    if !flow_path.exists() {
        anyhow::bail!("Flow '{}' does not exist", flow_name);
    }
    let flow = Flow::load(&flow_path)?;

    println!("Creating task: {}", branch_name);

    // Create worktree
    let worktrees_base = config.base_dir.join("worktrees");
    std::fs::create_dir_all(&worktrees_base)?;

    println!("Creating git worktree...");
    let worktree_path = Git::create_worktree(branch_name, &worktrees_base)
        .context("Failed to create git worktree")?;

    // Create task
    println!("Creating task directory...");
    let mut task = Task::create(config, branch_name, description, flow_name, worktree_path.clone())?;

    // Create tmux session
    println!("Creating tmux session...");
    Tmux::create_session(&task.meta.tmux_session, &worktree_path)?;

    // Start the first agent if flow has steps
    if let Some(step) = flow.get_step(0) {
        let agent_name = match step {
            flow::FlowStep::Agent(a) => a.agent.clone(),
            flow::FlowStep::Loop(l) => l.steps.first().map(|s| s.agent.clone()).unwrap_or_default(),
        };

        if !agent_name.is_empty() {
            println!("Starting agent: {}", agent_name);
            task.update_agent(Some(agent_name.clone()))?;

            let runner = agent::AgentRunner::new(config.clone());
            runner.run_agent_in_tmux(&mut task, &agent_name)?;
        }
    }

    println!();
    println!("Task created successfully!");
    println!("  Branch:    {}", branch_name);
    println!("  Worktree:  {}", worktree_path.display());
    println!("  Tmux:      {}", task.meta.tmux_session);
    println!("  Flow:      {}", flow_name);
    println!();
    println!("To attach: agman attach {}", branch_name);
    println!("Or run:    agman");

    Ok(())
}

fn cmd_list(config: &Config) -> Result<()> {
    let tasks = Task::list_all(config)?;

    if tasks.is_empty() {
        println!("No tasks found.");
        println!("Create one with: agman new <branch-name> \"description\"");
        return Ok(());
    }

    println!("{:<20} {:<10} {:<12} {:<10}", "BRANCH", "STATUS", "AGENT", "UPDATED");
    println!("{}", "-".repeat(55));

    for task in tasks {
        let status_icon = match task.meta.status {
            TaskStatus::Working => "●",
            TaskStatus::Paused => "◐",
            TaskStatus::Done => "✓",
            TaskStatus::Failed => "✗",
        };

        println!(
            "{} {:<18} {:<10} {:<12} {}",
            status_icon,
            task.meta.branch_name,
            task.meta.status,
            task.meta.current_agent.as_deref().unwrap_or("-"),
            task.time_since_update()
        );
    }

    Ok(())
}

fn cmd_delete(config: &Config, branch_name: &str, force: bool) -> Result<()> {
    let task = Task::load(config, branch_name)?;

    if !force {
        print!("Delete task '{}'? This will remove the worktree and tmux session. [y/N] ", branch_name);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!("Deleting task: {}", branch_name);

    // Kill tmux session
    println!("  Killing tmux session...");
    let _ = Tmux::kill_session(&task.meta.tmux_session);

    // Remove worktree
    println!("  Removing git worktree...");
    let _ = Git::remove_worktree(&task.meta.worktree_path);

    // Delete branch
    println!("  Deleting git branch...");
    let _ = Git::delete_branch(branch_name);

    // Delete task directory
    println!("  Removing task directory...");
    task.delete(config)?;

    println!("Task '{}' deleted.", branch_name);

    Ok(())
}

fn cmd_run(config: &Config, branch_name: &str, agent_name: &str, loop_mode: bool) -> Result<()> {
    config.init_default_files()?;

    let mut task = Task::load(config, branch_name)?;

    // Check if agent prompt exists
    let prompt_path = config.prompt_path(agent_name);
    if !prompt_path.exists() {
        anyhow::bail!("Agent '{}' does not exist", agent_name);
    }

    // Ensure tmux session exists
    if !Tmux::session_exists(&task.meta.tmux_session) {
        Tmux::create_session(&task.meta.tmux_session, &task.meta.worktree_path)?;
    }

    let runner = agent::AgentRunner::new(config.clone());

    if loop_mode {
        println!("Running agent '{}' in loop mode...", agent_name);
        let result = runner.run_agent_loop(&mut task, agent_name)?;
        println!("Agent loop finished with: {}", result);
    } else {
        println!("Running agent '{}'...", agent_name);
        runner.run_agent_in_tmux(&mut task, agent_name)?;
        println!("Agent started in tmux session: {}", task.meta.tmux_session);
        println!("To attach: agman attach {}", branch_name);
    }

    Ok(())
}

fn cmd_pause(config: &Config, branch_name: &str) -> Result<()> {
    let mut task = Task::load(config, branch_name)?;
    task.update_status(TaskStatus::Paused)?;
    println!("Task '{}' paused.", branch_name);
    Ok(())
}

fn cmd_resume(config: &Config, branch_name: &str) -> Result<()> {
    let mut task = Task::load(config, branch_name)?;
    task.update_status(TaskStatus::Working)?;

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
            println!("Task '{}' resumed with agent '{}'.", branch_name, agent_name);
        } else {
            println!("Task '{}' resumed.", branch_name);
        }
    } else {
        println!("Task '{}' resumed (no more flow steps).", branch_name);
    }

    Ok(())
}

fn cmd_attach(config: &Config, branch_name: &str) -> Result<()> {
    let task = Task::load(config, branch_name)?;

    if !Tmux::session_exists(&task.meta.tmux_session) {
        // Create session if it doesn't exist
        Tmux::create_session(&task.meta.tmux_session, &task.meta.worktree_path)?;
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
