use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub flows_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub commands_dir: PathBuf,
    pub repos_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Could not find home directory")?;
        let base_dir = home_dir.join(".agman");
        let repos_dir = home_dir.join("repos");

        let tasks_dir = base_dir.join("tasks");
        let flows_dir = base_dir.join("flows");
        let prompts_dir = base_dir.join("prompts");
        let commands_dir = base_dir.join("commands");

        Ok(Self {
            base_dir,
            tasks_dir,
            flows_dir,
            prompts_dir,
            commands_dir,
            repos_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.tasks_dir)
            .context("Failed to create tasks directory")?;
        std::fs::create_dir_all(&self.flows_dir)
            .context("Failed to create flows directory")?;
        std::fs::create_dir_all(&self.prompts_dir)
            .context("Failed to create prompts directory")?;
        std::fs::create_dir_all(&self.commands_dir)
            .context("Failed to create commands directory")?;
        Ok(())
    }

    /// Get task directory: ~/.agman/tasks/<repo>--<branch>/
    pub fn task_dir(&self, repo_name: &str, branch_name: &str) -> PathBuf {
        self.tasks_dir.join(format!("{}--{}", repo_name, branch_name))
    }

    /// Get task ID from repo and branch names
    pub fn task_id(repo_name: &str, branch_name: &str) -> String {
        format!("{}--{}", repo_name, branch_name)
    }

    /// Parse task ID into (repo_name, branch_name)
    pub fn parse_task_id(task_id: &str) -> Option<(String, String)> {
        let parts: Vec<&str> = task_id.splitn(2, "--").collect();
        if parts.len() == 2 {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    }

    /// Get main repo path: ~/repos/<repo>/
    pub fn repo_path(&self, repo_name: &str) -> PathBuf {
        self.repos_dir.join(repo_name)
    }

    /// Get worktree base path: ~/repos/<repo>-wt/
    pub fn worktree_base(&self, repo_name: &str) -> PathBuf {
        self.repos_dir.join(format!("{}-wt", repo_name))
    }

    /// Get worktree path: ~/repos/<repo>-wt/<branch>/
    pub fn worktree_path(&self, repo_name: &str, branch_name: &str) -> PathBuf {
        self.worktree_base(repo_name).join(branch_name)
    }

    /// Get tmux session name: (<repo>)__<branch>
    pub fn tmux_session_name(repo_name: &str, branch_name: &str) -> String {
        format!("({})__{}", repo_name, branch_name)
    }

    pub fn flow_path(&self, flow_name: &str) -> PathBuf {
        self.flows_dir.join(format!("{}.yaml", flow_name))
    }

    pub fn prompt_path(&self, agent_name: &str) -> PathBuf {
        self.prompts_dir.join(format!("{}.md", agent_name))
    }

    pub fn command_path(&self, command_id: &str) -> PathBuf {
        self.commands_dir.join(format!("{}.yaml", command_id))
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

        let continue_flow = self.flow_path("continue");
        if !continue_flow.exists() {
            std::fs::write(&continue_flow, CONTINUE_FLOW)?;
        }

        // Create default prompts if they don't exist
        let prompts = [
            ("planner", PLANNER_PROMPT),
            ("coder", CODER_PROMPT),
            ("test-writer", TEST_WRITER_PROMPT),
            ("tester", TESTER_PROMPT),
            ("reviewer", REVIEWER_PROMPT),
            ("refiner", REFINER_PROMPT),
            // Command-specific prompts
            ("pr-creator", PR_CREATOR_PROMPT),
            ("ci-monitor", CI_MONITOR_PROMPT),
            ("ci-fixer", CI_FIXER_PROMPT),
            ("review-reader", REVIEW_READER_PROMPT),
            ("review-fixer", REVIEW_FIXER_PROMPT),
            ("review-summarizer", REVIEW_SUMMARIZER_PROMPT),
        ];

        for (name, content) in prompts {
            let path = self.prompt_path(name);
            if !path.exists() {
                std::fs::write(&path, content)?;
            }
        }

        // Create default stored commands
        let commands = [
            ("create-pr", CREATE_PR_COMMAND),
            ("address-review", ADDRESS_REVIEW_COMMAND),
        ];

        for (name, content) in commands {
            let path = self.command_path(name);
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

const CONTINUE_FLOW: &str = r#"name: continue
steps:
  - agent: refiner
    until: AGENT_DONE
  - agent: coder
    until: TASK_COMPLETE
    on_blocked: pause
"#;

const PLANNER_PROMPT: &str = r#"You are a planning agent. Your job is to analyze the task and create a detailed implementation plan.

Instructions:
1. Explore the codebase to understand its structure
2. Read TASK.md and understand the Goal section
3. Break it down into concrete, actionable steps
4. Identify any dependencies or prerequisites
5. Update TASK.md - keep the Goal section and rewrite the Plan section with your detailed plan

The TASK.md format is:
```
# Goal
[The high-level objective]

# Plan
## Completed
- [x] Steps that are done

## Remaining
- [ ] Next step to do
- [ ] Another step
```

IMPORTANT:
- Do NOT ask questions or wait for input
- Make reasonable assumptions if something is unclear
- Just investigate, write the plan, and finish

When you are done, output exactly: AGENT_DONE
"#;

const CODER_PROMPT: &str = r#"You are a coding agent. Your job is to implement the task according to the plan.

Instructions:
1. Read TASK.md - understand the Goal and follow the Plan
2. Implement each step in the Remaining section in order
3. Write clean, well-structured code
4. As you complete steps, move them from Remaining to Completed in TASK.md
5. Commit your changes with clear messages

IMPORTANT:
- Do NOT ask questions or wait for input
- Make reasonable assumptions if something is unclear
- Just implement the code and finish

When the task is fully implemented, output exactly: TASK_COMPLETE
If you cannot complete it for some reason, output exactly: TASK_BLOCKED
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

const REFINER_PROMPT: &str = r#"You are a refiner agent. Your job is to synthesize feedback and create a clear, fresh context for the next agent.

You have been given:
- The previous TASK.md (which may be outdated)
- What has been done so far (git commits, current diff)
- Follow-up feedback from the user

Your job is to create a FRESH, SELF-CONTAINED context by rewriting TASK.md entirely.

The TASK.md format is:
```
# Goal
[The high-level objective - what we're trying to achieve NOW]

# Plan
## Completed
- [x] Steps that are done

## Remaining
- [ ] Next step to do
- [ ] Another step
```

Instructions:
1. Read and understand all the context provided
2. Focus primarily on the NEW FEEDBACK - this is what matters now
3. Rewrite TASK.md with:
   - A clear Goal section describing what we're trying to achieve NOW
   - A Plan section with Completed steps (what's been done) and Remaining steps
   - The coder should be able to follow it without any other context

IMPORTANT:
- Do NOT implement any changes yourself
- The Goal should be written as a fresh task, not as "changes to make"
- Forget about preserving history - create clean, focused context
- If the feedback is unclear, make reasonable assumptions

When you're done writing TASK.md, output exactly: AGENT_DONE
"#;

// ============================================================================
// Stored Command Definitions
// ============================================================================

const CREATE_PR_COMMAND: &str = r#"name: Create Draft PR
id: create-pr
description: Creates a draft PR with a good description, monitors CI, and fixes failures

steps:
  - agent: pr-creator
    until: AGENT_DONE
  - agent: ci-monitor
    until: AGENT_DONE
"#;

const ADDRESS_REVIEW_COMMAND: &str = r#"name: Address Review
id: address-review
description: Addresses all review comments with separate commits and generates response summaries

steps:
  - agent: review-reader
    until: AGENT_DONE
  - agent: review-fixer
    until: AGENT_DONE
  - agent: review-summarizer
    until: AGENT_DONE
"#;

// ============================================================================
// Command-specific Agent Prompts
// ============================================================================

const PR_CREATOR_PROMPT: &str = r#"You are a PR creation agent. Your job is to create a well-crafted draft pull request.

Instructions:
1. Analyze all commits on the current branch compared to main/master:
   - Run `git log origin/main..HEAD --oneline` to see commits
   - Run `git diff origin/main..HEAD` to see all changes
2. Understand what the changes accomplish - read through the diffs carefully
3. Write a clear, comprehensive PR description that:
   - Has a concise title (under 72 chars)
   - Summarizes what the PR does and why
   - Lists key changes
   - Notes any breaking changes or migration steps if applicable
4. Create the draft PR using the gh CLI:
   ```
   gh pr create --draft --title "Your title" --body "Your description"
   ```

IMPORTANT:
- Do NOT ask questions or wait for input
- If there's already a PR for this branch, just output AGENT_DONE
- Make sure the description is helpful for reviewers
- Include any relevant context from commit messages

When the PR is created (or already exists), output exactly: AGENT_DONE
If you cannot create the PR for some reason, output exactly: TASK_BLOCKED
"#;

const CI_MONITOR_PROMPT: &str = r#"You are a CI monitoring agent. Your job is to monitor CI checks and fix any failures.

Instructions:
1. Check the current PR's CI status:
   ```
   gh pr checks
   ```
2. If all checks pass, you're done!
3. If checks are still running, wait and check again (use `sleep 30` between checks)
4. If checks fail:
   a. Get the failed check details and logs
   b. Analyze what went wrong
   c. Fix the issue in the code
   d. Commit the fix with a clear message like "fix: [description of fix]"
   e. Push the changes: `git push`
   f. Go back to step 1 and monitor again

To get CI logs for a failed check:
```
gh run view <run-id> --log-failed
```

IMPORTANT:
- Do NOT ask questions or wait for input
- Make reasonable fixes based on the error messages
- If you've tried fixing 3 times and it still fails, output TASK_BLOCKED
- Each fix should be a separate commit

When all CI checks pass, output exactly: AGENT_DONE
If you cannot fix the CI after multiple attempts, output exactly: TASK_BLOCKED
"#;

const CI_FIXER_PROMPT: &str = r#"You are a CI fixer agent. Your job is to fix a specific CI failure.

Instructions:
1. You've been given information about a CI failure
2. Analyze the error logs to understand what went wrong
3. Fix the issue in the code
4. Commit with a clear message describing the fix
5. Push the changes

Common CI failures and fixes:
- Type errors: Fix the type annotations or add proper type guards
- Test failures: Fix the failing test or the code it's testing
- Lint errors: Fix formatting, unused imports, etc.
- Build errors: Fix syntax or missing dependencies

IMPORTANT:
- Do NOT ask questions or wait for input
- Make minimal, focused fixes - don't refactor unrelated code
- Each fix should be a separate commit

When the fix is committed and pushed, output exactly: AGENT_DONE
If you cannot fix the issue, output exactly: TASK_BLOCKED
"#;

const REVIEW_READER_PROMPT: &str = r#"You are a review reader agent. Your job is to analyze PR review comments and determine which need to be addressed.

Instructions:
1. Fetch all review comments on the current PR:
   ```
   gh pr view --comments --json reviews,comments
   ```
2. For each comment, categorize it:
   - MUST_ADDRESS: Bugs, security issues, incorrect logic, requested changes
   - SHOULD_ADDRESS: Style improvements, minor suggestions, optional enhancements
   - SKIP: Questions that were answered, nitpicks, praise, resolved discussions
3. Create a file `REVIEW_ITEMS.md` in the task directory with the format:
   ```markdown
   # Review Items to Address

   ## Must Address
   - [ ] [file:line] Description of issue (comment author)

   ## Should Address
   - [ ] [file:line] Description of suggestion (comment author)

   ## Skipped
   - [reason] Description (comment author)
   ```

IMPORTANT:
- Do NOT ask questions or wait for input
- Be thorough - read ALL comments carefully
- Include enough context that the fixer agent can understand each item
- Preserve the original commenter's intent

When REVIEW_ITEMS.md is created, output exactly: AGENT_DONE
If there are no review comments to address, output exactly: AGENT_DONE
If you cannot read the reviews, output exactly: TASK_BLOCKED
"#;

const REVIEW_FIXER_PROMPT: &str = r#"You are a review fixer agent. Your job is to address review comments one by one.

Instructions:
1. Read REVIEW_ITEMS.md to see what needs to be addressed
2. For each unchecked item in "Must Address" and "Should Address":
   a. Understand what change is requested
   b. Make the fix in the code
   c. Commit with a message that references the review item, e.g.:
      "fix: address review - [brief description]"
   d. Mark the item as done in REVIEW_ITEMS.md by changing [ ] to [x]
3. Push all commits when done: `git push`

IMPORTANT:
- Do NOT ask questions or wait for input
- Each review item should be a SEPARATE commit
- The commit message should clearly describe what was fixed
- If a comment is unclear, make a reasonable interpretation
- If you genuinely cannot address an item, mark it as [SKIPPED: reason]

When all items are addressed and pushed, output exactly: AGENT_DONE
If you cannot continue, output exactly: TASK_BLOCKED
"#;

const REVIEW_SUMMARIZER_PROMPT: &str = r#"You are a review summarizer agent. Your job is to generate response summaries for review comments.

Instructions:
1. Read REVIEW_ITEMS.md to see what was addressed
2. Get the recent commits that addressed each item:
   ```
   git log --oneline -10
   ```
3. Create a file `REVIEW_RESPONSES.md` with responses for each addressed item:
   ```markdown
   # Review Responses

   ## Response to [reviewer name]'s comment on [file:line]

   Fixed in commit `abc1234`.

   [Brief explanation of what was changed and why]

   ---

   ## Response to [reviewer name]'s comment on [file:line]

   ...
   ```

The responses should:
- Reference the specific commit hash that addresses the comment
- Briefly explain what was changed
- Be professional and courteous
- Be suitable for posting as a reply to the original comment

IMPORTANT:
- Do NOT ask questions or wait for input
- Include commit hashes so reviewers can see exactly what changed
- Keep responses concise but informative

When REVIEW_RESPONSES.md is created, output exactly: AGENT_DONE
If there's nothing to summarize, output exactly: AGENT_DONE
"#;
