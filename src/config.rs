use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub flows_dir: PathBuf,
    pub prompts_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self> {
        let base_dir = dirs::home_dir()
            .context("Could not find home directory")?
            .join(".agman");

        let tasks_dir = base_dir.join("tasks");
        let flows_dir = base_dir.join("flows");
        let prompts_dir = base_dir.join("prompts");

        Ok(Self {
            base_dir,
            tasks_dir,
            flows_dir,
            prompts_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.tasks_dir)
            .context("Failed to create tasks directory")?;
        std::fs::create_dir_all(&self.flows_dir)
            .context("Failed to create flows directory")?;
        std::fs::create_dir_all(&self.prompts_dir)
            .context("Failed to create prompts directory")?;
        Ok(())
    }

    pub fn task_dir(&self, branch_name: &str) -> PathBuf {
        self.tasks_dir.join(branch_name)
    }

    pub fn flow_path(&self, flow_name: &str) -> PathBuf {
        self.flows_dir.join(format!("{}.yaml", flow_name))
    }

    pub fn prompt_path(&self, agent_name: &str) -> PathBuf {
        self.prompts_dir.join(format!("{}.md", agent_name))
    }

    pub fn init_default_files(&self) -> Result<()> {
        self.ensure_dirs()?;

        // Create default flow if it doesn't exist
        let default_flow = self.flow_path("default");
        if !default_flow.exists() {
            std::fs::write(&default_flow, DEFAULT_FLOW)?;
        }

        let tdd_flow = self.flow_path("tdd");
        if !tdd_flow.exists() {
            std::fs::write(&tdd_flow, TDD_FLOW)?;
        }

        let review_flow = self.flow_path("review");
        if !review_flow.exists() {
            std::fs::write(&review_flow, REVIEW_FLOW)?;
        }

        // Create default prompts if they don't exist
        let prompts = [
            ("planner", PLANNER_PROMPT),
            ("coder", CODER_PROMPT),
            ("test-writer", TEST_WRITER_PROMPT),
            ("tester", TESTER_PROMPT),
            ("reviewer", REVIEWER_PROMPT),
        ];

        for (name, content) in prompts {
            let path = self.prompt_path(name);
            if !path.exists() {
                std::fs::write(&path, content)?;
            }
        }

        Ok(())
    }
}

const DEFAULT_FLOW: &str = r#"name: default
steps:
  - agent: planner
    until: AGENT_DONE
  - agent: coder
    until: TASK_COMPLETE
    on_blocked: pause
"#;

const TDD_FLOW: &str = r#"name: tdd
steps:
  - agent: planner
    until: AGENT_DONE
  - loop:
      - agent: test-writer
        until: AGENT_DONE
      - agent: coder
        until: AGENT_DONE
      - agent: tester
        until: TESTS_PASS
        on_fail: continue
    until: TASK_COMPLETE
"#;

const REVIEW_FLOW: &str = r#"name: review
steps:
  - agent: reviewer
    until: AGENT_DONE
  - agent: coder
    until: TASK_COMPLETE
    on_blocked: pause
"#;

const PLANNER_PROMPT: &str = r#"You are a planning agent. Your job is to analyze the task and create a detailed implementation plan.

Instructions:
1. Read and understand the task goal
2. Break it down into concrete, actionable steps
3. Identify any dependencies or prerequisites
4. Consider edge cases and potential issues
5. Write your plan to PLAN.md

When you are done planning, output: AGENT_DONE

If you need human input to proceed, output: TASK_BLOCKED
"#;

const CODER_PROMPT: &str = r#"You are a coding agent. Your job is to implement the task according to the plan.

Instructions:
1. Follow the plan in PLAN.md
2. Write clean, well-structured code
3. Update progress.md as you complete steps
4. Commit your changes with clear messages

When the task is fully implemented, output: TASK_COMPLETE
When you finish a chunk of work but the task isn't done, output: AGENT_DONE
If you're blocked and need human help, output: TASK_BLOCKED
"#;

const TEST_WRITER_PROMPT: &str = r#"You are a test-writing agent. Your job is to write tests for the task.

Instructions:
1. Read the plan and understand what needs to be tested
2. Write comprehensive unit tests
3. Write integration tests where appropriate
4. Ensure tests are runnable and properly structured

When you're done writing tests, output: AGENT_DONE
If you need human input, output: TASK_BLOCKED
"#;

const TESTER_PROMPT: &str = r#"You are a testing agent. Your job is to run tests and report results.

Instructions:
1. Run all relevant tests
2. Analyze any failures
3. Report results clearly

If all tests pass, output: TESTS_PASS
If tests fail, output: TESTS_FAIL
If you need human help, output: TASK_BLOCKED
"#;

const REVIEWER_PROMPT: &str = r#"You are a code review agent. Your job is to review code quality and suggest improvements.

Instructions:
1. Review the code for correctness, style, and best practices
2. Check for potential bugs or security issues
3. Suggest improvements where appropriate
4. Document your findings

When you're done reviewing, output: AGENT_DONE
If critical issues need human attention, output: TASK_BLOCKED
"#;
