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
        let task_content = task.read_task()?;
        let feedback = task.read_feedback()?;

        let mut prompt = self.prompt_template.clone();

        // Append skill-awareness footer before task content so the agent sees it early
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Skills\n");
        prompt.push_str("Before starting, check if the repository has Claude Code skills defined in `.claude/skills/` or `.claude/commands/`. If any are relevant to your task, use them.\n");

        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Current Task\n");
        prompt.push_str(&task_content);
        prompt.push_str("\n\n");

        // Include git context for refiner and checker agents
        let needs_git_context =
            !feedback.is_empty() || self.name == "checker" || self.name == "refiner";

        if needs_git_context {
            // Include feedback if present
            if !feedback.is_empty() {
                prompt.push_str("# Follow-up Feedback\n");
                prompt.push_str(&feedback);
                prompt.push_str("\n\n");
            }

            // Include git diff for context
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

        // Append self-improve footer to all prompts
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Self-Improvement\n");
        prompt.push_str("**Before outputting your final stop condition**, check if the repository has a self-improvement skill (commonly named \"self-improve\" or similar) in `.claude/skills/` or `.claude/commands/`. If one exists, run it. This helps keep project documentation and conventions up to date.\n");

        Ok(prompt)
    }

    /// Run agent in tmux's agman window
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

        // Send to the agman window in tmux
        Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &cmd)?;

        Ok(())
    }

    /// Run agent directly (blocking) and capture output
    pub fn run_direct(&self, task: &Task) -> Result<Option<StopCondition>> {
        tracing::info!(agent = %self.name, task_id = %task.meta.task_id(), "starting agent (direct)");
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
                tracing::info!(agent = %self.name, task_id = %task.meta.task_id(), condition = %condition, "stop condition detected");
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
                agent = %self.name,
                task_id = %task.meta.task_id(),
                exit_code = status.code().unwrap_or(-1),
                "Claude process exited with non-zero status"
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
        tracing::debug!(agent = agent_name, task_id = %task.meta.task_id(), "loading and running agent");
        let agent = Agent::load(&self.config, agent_name)?;

        // Update task to show current agent
        task.update_agent(Some(agent_name.to_string()))?;

        // Run the agent directly (blocking)
        let result = agent.run_direct(task)?;

        // If this was the refiner, clear the feedback (it's been synthesized into TASK.md)
        if agent_name == "refiner" {
            task.clear_feedback()?;
        }

        // Check for .pr-link sidecar file (written by pr-creator or any agent)
        let pr_link_path = task.meta.worktree_path.join(".pr-link");
        if pr_link_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&pr_link_path) {
                let lines: Vec<&str> = contents.lines().collect();
                if lines.len() >= 2 {
                    if let Ok(number) = lines[0].trim().parse::<u64>() {
                        let url = lines[1].trim().to_string();
                        tracing::info!(task_id = %task.meta.task_id(), pr_number = number, pr_url = %url, "detected .pr-link, storing linked PR");
                        task.set_linked_pr(number, url, true)?;
                    }
                }
            }
            // Clean up the sidecar file
            let _ = std::fs::remove_file(&pr_link_path);
        }

        Ok(result)
    }

    /// Run a single agent in a loop until it returns a stop condition
    pub fn run_agent_loop(&self, task: &mut Task, agent_name: &str) -> Result<StopCondition> {
        loop {
            let result = self.run_agent(task, agent_name)?;

            match result {
                Some(condition) => return Ok(condition),
                None => {
                    // No magic string detected, keep looping
                    tracing::info!(agent = agent_name, task_id = %task.meta.task_id(), "no stop condition detected, continuing agent loop");
                    println!("No stop condition detected, running agent again...");
                }
            }
        }
    }

    /// Run an entire flow to completion
    pub fn run_flow(&self, task: &mut Task) -> Result<StopCondition> {
        tracing::info!(flow = %task.meta.flow_name, task_id = %task.meta.task_id(), "starting flow");
        let flow = Flow::load(&self.config.flow_path(&task.meta.flow_name))?;
        self.run_flow_with(task, &flow)
    }

    /// Run a pre-loaded flow to completion (used by stored commands whose
    /// flow definitions live outside ~/.agman/flows/)
    pub fn run_flow_with(&self, task: &mut Task, flow: &Flow) -> Result<StopCondition> {
        println!("Starting flow: {}", flow.name);
        println!();

        loop {
            let step_index = task.meta.flow_step;

            let Some(step) = flow.get_step(step_index) else {
                // No more steps, flow is complete
                tracing::info!(task_id = %task.meta.task_id(), "flow complete - no more steps");
                println!("Flow complete - no more steps");
                task.update_status(TaskStatus::Stopped)?;

                // Check for queued feedback and process it
                if let Some(result) = self.process_queued_feedback(task)? {
                    return Ok(result);
                }

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
                            task.update_status(TaskStatus::Stopped)?;

                            // Check for queued feedback and process it
                            if let Some(result) = self.process_queued_feedback(task)? {
                                return Ok(result);
                            }

                            return Ok(StopCondition::TaskComplete);
                        }
                        Some(StopCondition::InputNeeded) => {
                            println!("Agent needs user input - pausing for answers");
                            task.update_status(TaskStatus::InputNeeded)?;
                            // Do NOT advance the flow step — re-run same agent after user answers
                            return Ok(StopCondition::InputNeeded);
                        }
                        Some(StopCondition::TaskBlocked) => {
                            println!("Task blocked - needs human intervention");
                            match agent_step.on_blocked {
                                Some(BlockedAction::Continue) => {
                                    println!("on_blocked: continue - advancing to next step");
                                    task.advance_flow_step()?;
                                }
                                _ => {
                                    println!("on_blocked: stop - stopping flow");
                                    task.update_status(TaskStatus::Stopped)?;

                                    // Check for queued feedback and process it
                                    if let Some(result) = self.process_queued_feedback(task)? {
                                        return Ok(result);
                                    }

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
                            task.update_status(TaskStatus::Stopped)?;

                            // Check for queued feedback and process it
                            if let Some(result) = self.process_queued_feedback(task)? {
                                return Ok(result);
                            }

                            return Ok(StopCondition::TaskComplete);
                        }
                        StopCondition::InputNeeded => {
                            task.update_status(TaskStatus::InputNeeded)?;
                            return Ok(StopCondition::InputNeeded);
                        }
                        StopCondition::TaskBlocked => {
                            task.update_status(TaskStatus::Stopped)?;

                            // Check for queued feedback and process it
                            if let Some(result) = self.process_queued_feedback(task)? {
                                return Ok(result);
                            }

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

    /// Process queued feedback if any exists
    /// Returns Some(StopCondition) if feedback was processed and a new flow completed,
    /// or None if no feedback was queued
    fn process_queued_feedback(&self, task: &mut Task) -> Result<Option<StopCondition>> {
        // Queue is stored in a separate file (feedback_queue.json), so no need
        // to reload meta — the queue methods read directly from disk.
        if !task.has_queued_feedback() {
            return Ok(None);
        }

        println!();
        println!("=== Processing queued feedback ({} items) ===", task.queued_feedback_count());

        // Pop the first feedback item
        let feedback = task.pop_feedback_queue()?.expect("Queue was not empty");

        println!("Feedback: {}", if feedback.len() > 100 {
            format!("{}...", &feedback[..100])
        } else {
            feedback.clone()
        });

        // Write feedback to FEEDBACK.md
        task.write_feedback(&feedback)?;

        // Reset flow to use continue flow and start from step 0
        task.meta.flow_name = "continue".to_string();
        task.reset_flow_step()?;
        task.update_status(TaskStatus::Running)?;

        println!();

        // Run the continue flow
        let result = self.run_flow(task)?;

        Ok(Some(result))
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
                    Some(StopCondition::InputNeeded) => {
                        return Ok(StopCondition::InputNeeded);
                    }
                    Some(StopCondition::TaskBlocked) => {
                        return Ok(StopCondition::TaskBlocked);
                    }
                    Some(condition) if condition == until => {
                        println!("Loop condition {:?} met", until);
                        return Ok(condition);
                    }
                    Some(StopCondition::AgentDone) if until == StopCondition::TaskComplete => {
                        // In a loop that waits for TASK_COMPLETE, an AGENT_DONE from
                        // the checker (or any agent) triggers a remaining-work check.
                        // If TASK.md's ## Remaining section is empty, the task is done.
                        if !task.has_remaining_work() {
                            tracing::info!(
                                task_id = %task.meta.task_id(),
                                agent = %agent_step.agent,
                                "no remaining work in TASK.md — treating as TASK_COMPLETE"
                            );
                            println!("No remaining work items — task complete");
                            return Ok(StopCondition::TaskComplete);
                        }
                        // Otherwise continue the loop
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
