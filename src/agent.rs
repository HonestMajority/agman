use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::flow::StopCondition;
use crate::task::Task;
use crate::tmux::Tmux;

pub struct Agent {
    #[allow(dead_code)]
    pub name: String,
    pub prompt_template: String,
}

impl Agent {
    pub fn load(config: &Config, name: &str) -> Result<Self> {
        let prompt_path = config.prompt_path(name);
        let prompt_template = std::fs::read_to_string(&prompt_path)
            .with_context(|| format!("Failed to load agent prompt: {}", name))?;

        Ok(Self {
            name: name.to_string(),
            prompt_template,
        })
    }

    pub fn build_prompt(&self, task: &Task) -> Result<String> {
        let goal = task.read_prompt()?;
        let progress = task.read_progress()?;
        let context = task.read_context()?;
        let plan = task.read_plan()?;

        let mut prompt = self.prompt_template.clone();
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Task Goal\n");
        prompt.push_str(&goal);
        prompt.push_str("\n\n");

        if !plan.is_empty() {
            prompt.push_str("# Implementation Plan\n");
            prompt.push_str(&plan);
            prompt.push_str("\n\n");
        }

        if !progress.is_empty() {
            prompt.push_str("# Progress So Far\n");
            prompt.push_str(&progress);
            prompt.push_str("\n\n");
        }

        if !context.is_empty() {
            prompt.push_str("# Relevant Context\n");
            prompt.push_str(&context);
        }

        Ok(prompt)
    }

    pub fn run_in_tmux(&self, task: &Task, _config: &Config) -> Result<()> {
        let prompt = self.build_prompt(task)?;

        // Write prompt to a temp file
        let prompt_file = task.dir.join(".current-prompt.md");
        std::fs::write(&prompt_file, &prompt)?;

        // Build the claude command
        let cmd = format!(
            "claude --print '{}' 2>&1 | tee -a '{}'",
            prompt_file.display(),
            task.dir.join("agent.log").display()
        );

        // Send to tmux
        Tmux::send_keys(&task.meta.tmux_session, &cmd)?;

        Ok(())
    }

    pub fn run_direct(&self, task: &Task) -> Result<Option<StopCondition>> {
        let prompt = self.build_prompt(task)?;

        // Write prompt to temp file
        let prompt_file = task.dir.join(".current-prompt.md");
        std::fs::write(&prompt_file, &prompt)?;

        // Run claude directly and capture output
        let mut child = Command::new("claude")
            .arg("--print")
            .arg(&prompt_file)
            .current_dir(&task.meta.worktree_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn claude process")?;

        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        let mut full_output = String::new();
        let mut last_condition: Option<StopCondition> = None;

        for line in reader.lines() {
            let line = line?;
            full_output.push_str(&line);
            full_output.push('\n');

            // Log to agent.log
            task.append_agent_log(&line)?;

            // Check for magic strings
            if let Some(condition) = StopCondition::from_output(&line) {
                last_condition = Some(condition);
            }

            // Print to stdout for visibility
            println!("{}", line);
        }

        let status = child.wait()?;
        if !status.success() {
            tracing::warn!("Claude process exited with non-zero status");
        }

        // Clean up temp file
        let _ = std::fs::remove_file(&prompt_file);

        Ok(last_condition)
    }
}

pub struct AgentRunner {
    config: Config,
}

impl AgentRunner {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn run_agent(&self, task: &mut Task, agent_name: &str) -> Result<Option<StopCondition>> {
        let agent = Agent::load(&self.config, agent_name)?;

        // Update task to show current agent
        task.update_agent(Some(agent_name.to_string()))?;

        // Log start
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        task.append_agent_log(&format!("\n--- Agent: {} started at {} ---\n", agent_name, timestamp))?;

        // Run the agent
        let result = agent.run_direct(task)?;

        // Log completion
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let result_str = result.map(|r| r.to_string()).unwrap_or_else(|| "no signal".to_string());
        task.append_agent_log(&format!("\n--- Agent: {} finished at {} with: {} ---\n", agent_name, timestamp, result_str))?;

        Ok(result)
    }

    pub fn run_agent_in_tmux(&self, task: &mut Task, agent_name: &str) -> Result<()> {
        let agent = Agent::load(&self.config, agent_name)?;

        // Update task to show current agent
        task.update_agent(Some(agent_name.to_string()))?;

        // Run in tmux
        agent.run_in_tmux(task, &self.config)?;

        Ok(())
    }

    pub fn run_agent_loop(&self, task: &mut Task, agent_name: &str) -> Result<StopCondition> {
        loop {
            let result = self.run_agent(task, agent_name)?;

            match result {
                Some(StopCondition::TaskComplete) => return Ok(StopCondition::TaskComplete),
                Some(StopCondition::TaskBlocked) => return Ok(StopCondition::TaskBlocked),
                Some(StopCondition::AgentDone) => return Ok(StopCondition::AgentDone),
                Some(other) => return Ok(other),
                None => {
                    // No magic string, keep looping
                    tracing::info!("No stop condition detected, continuing agent loop");
                }
            }
        }
    }
}
