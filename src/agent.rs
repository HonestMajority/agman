use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::flow::{BlockedAction, Flow, FlowStep, StopCondition};
use crate::task::{Task, TaskStatus};
use crate::tmux::Tmux;

pub struct Agent {
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
        let feedback = task.read_feedback()?;

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
            prompt.push_str("\n\n");
        }

        // Include feedback if present (for refiner agent)
        if !feedback.is_empty() {
            prompt.push_str("# Follow-up Feedback\n");
            prompt.push_str(&feedback);
            prompt.push_str("\n\n");

            // Also include git diff for context
            if let Ok(diff) = task.get_git_diff() {
                if !diff.is_empty() {
                    prompt.push_str("# Current Git Diff\n");
                    prompt.push_str("```diff\n");
                    // Truncate if too long
                    if diff.len() > 10000 {
                        prompt.push_str(&diff[..10000]);
                        prompt.push_str("\n... (truncated)\n");
                    } else {
                        prompt.push_str(&diff);
                    }
                    prompt.push_str("```\n\n");
                }
            }

            if let Ok(log) = task.get_git_log_summary() {
                if !log.is_empty() {
                    prompt.push_str("# Recent Commits\n");
                    prompt.push_str("```\n");
                    prompt.push_str(&log);
                    prompt.push_str("```\n");
                }
            }
        }

        Ok(prompt)
    }

    /// Run agent in tmux's claude window
    pub fn run_in_tmux(&self, task: &Task) -> Result<()> {
        let prompt = self.build_prompt(task)?;

        // Write prompt to a temp file in the task directory
        let prompt_file = task.dir.join(".current-prompt.md");
        std::fs::write(&prompt_file, &prompt)?;

        // Build the claude command for tmux
        // Use -p (print mode) and --dangerously-skip-permissions to skip interactive prompts
        // Pipe output to tee to log it
        let cmd = format!(
            "claude -p --dangerously-skip-permissions \"$(cat '{}')\" 2>&1 | tee -a '{}'",
            prompt_file.display(),
            task.dir.join("agent.log").display()
        );

        // Send to the claude window in tmux
        Tmux::send_keys_to_window(&task.meta.tmux_session, "claude", &cmd)?;

        Ok(())
    }

    /// Run agent directly (blocking) and capture output
    pub fn run_direct(&self, task: &Task) -> Result<Option<StopCondition>> {
        let prompt = self.build_prompt(task)?;

        // Log start
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        task.append_agent_log(&format!(
            "\n--- Agent: {} started at {} ---\n",
            self.name, timestamp
        ))?;

        // Run claude with -p flag (print mode, non-interactive)
        // Pass prompt via stdin
        let mut child = Command::new("claude")
            .arg("-p") // print mode
            .arg("--dangerously-skip-permissions") // allow file operations without confirmation
            .current_dir(&task.meta.worktree_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn claude process. Is claude CLI installed?")?;

        // Write prompt to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes())?;
        }

        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        let mut last_condition: Option<StopCondition> = None;

        for line in reader.lines() {
            let line = line?;

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

        // Log completion
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let result_str = last_condition
            .map(|r| r.to_string())
            .unwrap_or_else(|| "no signal".to_string());
        task.append_agent_log(&format!(
            "\n--- Agent: {} finished at {} with: {} (exit: {}) ---\n",
            self.name,
            timestamp,
            result_str,
            status.code().unwrap_or(-1)
        ))?;

        if !status.success() {
            tracing::warn!(
                "Claude process exited with status: {}",
                status.code().unwrap_or(-1)
            );
        }

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

    /// Run a single agent and return its stop condition
    pub fn run_agent(&self, task: &mut Task, agent_name: &str) -> Result<Option<StopCondition>> {
        let agent = Agent::load(&self.config, agent_name)?;

        // Update task to show current agent
        task.update_agent(Some(agent_name.to_string()))?;

        // Run the agent directly (blocking)
        let result = agent.run_direct(task)?;

        // If this was the refiner, clear the feedback (it's been synthesized into PROMPT.md/PLAN.md)
        if agent_name == "refiner" {
            task.clear_feedback()?;
        }

        Ok(result)
    }

    /// Run agent in tmux (non-blocking, for interactive use)
    pub fn run_agent_in_tmux(&self, task: &mut Task, agent_name: &str) -> Result<()> {
        let agent = Agent::load(&self.config, agent_name)?;

        // Update task to show current agent
        task.update_agent(Some(agent_name.to_string()))?;

        // Log start
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        task.append_agent_log(&format!(
            "\n--- Agent: {} started at {} (tmux) ---\n",
            agent_name, timestamp
        ))?;

        // Run in tmux (non-blocking)
        agent.run_in_tmux(task)?;

        Ok(())
    }

    /// Run a single agent in a loop until it returns a stop condition
    pub fn run_agent_loop(&self, task: &mut Task, agent_name: &str) -> Result<StopCondition> {
        loop {
            let result = self.run_agent(task, agent_name)?;

            match result {
                Some(condition) => return Ok(condition),
                None => {
                    // No magic string detected, keep looping
                    tracing::info!("No stop condition detected, continuing agent loop");
                    println!("No stop condition detected, running agent again...");
                }
            }
        }
    }

    /// Run an entire flow to completion
    pub fn run_flow(&self, task: &mut Task) -> Result<StopCondition> {
        let flow = Flow::load(&self.config.flow_path(&task.meta.flow_name))?;

        println!("Starting flow: {}", flow.name);
        println!();

        loop {
            let step_index = task.meta.flow_step;

            let Some(step) = flow.get_step(step_index) else {
                // No more steps, flow is complete
                println!("Flow complete - no more steps");
                task.update_status(TaskStatus::Done)?;
                return Ok(StopCondition::TaskComplete);
            };

            match step {
                FlowStep::Agent(agent_step) => {
                    println!(
                        "Step {}: Running agent '{}' (until: {})",
                        step_index, agent_step.agent, agent_step.until
                    );

                    let result = self.run_agent(task, &agent_step.agent)?;

                    match result {
                        Some(StopCondition::TaskComplete) => {
                            println!("Task marked complete by agent");
                            task.update_status(TaskStatus::Done)?;
                            return Ok(StopCondition::TaskComplete);
                        }
                        Some(StopCondition::TaskBlocked) => {
                            println!("Task blocked - needs human intervention");
                            match agent_step.on_blocked {
                                Some(BlockedAction::Continue) => {
                                    println!("on_blocked: continue - advancing to next step");
                                    task.advance_flow_step()?;
                                }
                                _ => {
                                    println!("on_blocked: pause - pausing flow");
                                    task.update_status(TaskStatus::Paused)?;
                                    return Ok(StopCondition::TaskBlocked);
                                }
                            }
                        }
                        Some(StopCondition::AgentDone) => {
                            if agent_step.until == StopCondition::AgentDone {
                                println!("Agent done - advancing to next step");
                                task.advance_flow_step()?;
                            } else {
                                println!(
                                    "Agent done but waiting for {:?} - running again",
                                    agent_step.until
                                );
                            }
                        }
                        Some(StopCondition::TestsPass) => {
                            if agent_step.until == StopCondition::TestsPass {
                                println!("Tests pass - advancing to next step");
                                task.advance_flow_step()?;
                            }
                        }
                        Some(StopCondition::TestsFail) => {
                            println!("Tests failed");
                            // For test failures, we typically want to loop back or continue
                            // The flow should handle this
                        }
                        None => {
                            // No stop condition - agent will be run again
                            println!("No stop condition from agent - running again");
                        }
                    }
                }
                FlowStep::Loop(loop_step) => {
                    println!(
                        "Step {}: Entering loop (until: {})",
                        step_index, loop_step.until
                    );

                    // Run the loop until its condition is met
                    let result = self.run_loop(task, &loop_step.steps, loop_step.until)?;

                    match result {
                        StopCondition::TaskComplete => {
                            task.update_status(TaskStatus::Done)?;
                            return Ok(StopCondition::TaskComplete);
                        }
                        StopCondition::TaskBlocked => {
                            task.update_status(TaskStatus::Paused)?;
                            return Ok(StopCondition::TaskBlocked);
                        }
                        _ => {
                            // Loop completed, advance to next step
                            task.advance_flow_step()?;
                        }
                    }
                }
            }
        }
    }

    /// Run a loop of agent steps until the loop condition is met
    fn run_loop(
        &self,
        task: &mut Task,
        steps: &[crate::flow::AgentStep],
        until: StopCondition,
    ) -> Result<StopCondition> {
        let mut iteration = 0;
        const MAX_ITERATIONS: usize = 100; // Safety limit

        loop {
            iteration += 1;
            if iteration > MAX_ITERATIONS {
                println!("Loop exceeded {} iterations, breaking", MAX_ITERATIONS);
                return Ok(StopCondition::AgentDone);
            }

            println!("Loop iteration {}", iteration);

            for (i, agent_step) in steps.iter().enumerate() {
                println!("  Step {}: Running agent '{}'", i, agent_step.agent);

                let result = self.run_agent(task, &agent_step.agent)?;

                match result {
                    Some(StopCondition::TaskComplete) => {
                        return Ok(StopCondition::TaskComplete);
                    }
                    Some(StopCondition::TaskBlocked) => {
                        return Ok(StopCondition::TaskBlocked);
                    }
                    Some(condition) if condition == until => {
                        println!("Loop condition {:?} met", until);
                        return Ok(condition);
                    }
                    Some(StopCondition::TestsFail) => {
                        // Tests failed, loop back to start
                        println!("Tests failed, restarting loop");
                        break; // Break inner loop to restart
                    }
                    _ => {
                        // Continue to next agent in loop
                    }
                }
            }
        }
    }
}
