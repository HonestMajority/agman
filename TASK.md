# Goal

## What We're Building

Add multi-repo task support to agman. Currently, tasks have a strict 1:1:1 relationship between a git branch, a git worktree, and a tmux session. We want to generalize this so a single task can span multiple git repos, while keeping the same simple flow model (one agent at a time, sequential execution).

## User Experience

When creating a new task in the TUI wizard, the user can select a non-git directory (e.g. the `~/repos/` dir itself) instead of a specific repo. When this happens:

1. The user enters a branch name and description as usual
2. The flow starts with a **repo-inspector agent** — a dedicated first-step agent whose only job is to inspect the git repos under the chosen directory and determine which repos are involved in the task. It writes its findings back to TASK.md (e.g. a `# Repos` section listing the selected repos with brief rationale).
3. After the repo-inspector finishes, agman reads the repo list from TASK.md, creates one worktree per repo (all using the same branch name), and creates one tmux session per repo.
4. The flow then continues with the normal agents (prompt-builder → planner → coder↔checker loop), but these agents are aware they're working across multiple repos.

For single-repo tasks, the behavior is essentially identical to today — there's just one entry in the repos list.

## Architecture & Design Decisions

### Unified model — no separate code paths

**Do not build this as two separate solutions.** The data model and code should handle both single-repo and multi-repo tasks with one unified design. No backwards compatibility concerns — we can freely break the existing data model. Do not add any migration utilities; existing tasks will simply break and users will recreate them.

### TASK.md moves to the `.agman` task dir

**Decision: Move TASK.md from the worktree to `~/.agman/tasks/<task_id>/TASK.md` for ALL tasks.**

Currently TASK.md lives at `worktree_path/TASK.md`. For multi-repo tasks there's no single worktree. Moving TASK.md to the task dir solves this cleanly and is actually simpler — no need for git excludes, no risk of accidentally committing it. This applies to single-repo tasks too (unified model).

Implications for the codebase:
- `Task::write_task()` (currently at `task.rs:288-292`) changes from `self.meta.worktree_path.join("TASK.md")` to `self.dir.join("TASK.md")`
- `Task::read_task()` (currently at `task.rs:377-380`) changes the same way
- `Task::ensure_git_excludes_task()` (at `task.rs:298-358`) can be simplified — only REVIEW.md needs excluding now, not TASK.md
- `Agent::build_prompt()` (at `agent.rs:27-87`) reads TASK.md via `task.read_task()` which will transparently use the new location
- The coder agent currently reads/writes TASK.md in the worktree — the task dir path must be communicated to the agent via the prompt so it can find TASK.md
- In `use_cases::delete_task()` (at `use_cases.rs:153-176`), the `DeleteMode::TaskOnly` branch currently removes `worktree_path.join("TASK.md")` — this should instead just delete the task dir (TASK.md is already there)

### Task data model: generalize to a list of repos

**Decision: Replace singular `repo_name`/`worktree_path`/`tmux_session` with a `Vec<RepoEntry>` in `TaskMeta`.**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub repo_name: String,
    pub worktree_path: PathBuf,
    pub tmux_session: String,
}

pub struct TaskMeta {
    pub name: String,                 // repo name for single-repo, parent dir name for multi-repo
    pub repos: Vec<RepoEntry>,        // replaces repo_name, worktree_path, tmux_session
    pub branch_name: String,          // shared across all repos
    pub status: TaskStatus,
    pub flow_name: String,
    pub current_agent: Option<String>,
    pub flow_step: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub review_after: bool,
    pub linked_pr: Option<LinkedPr>,
    pub last_review_count: Option<u64>,
    pub review_addressed: bool,
}
```

For single-repo tasks, `repos` has exactly one entry. The `name` field is used for task ID generation (`name--branch`). For single-repo tasks, `name` equals the repo name. For multi-repo tasks, `name` is the parent directory name (e.g. `repos` if the user selected `~/repos/`).

**Important:** The current code accesses `task.meta.repo_name`, `task.meta.worktree_path`, and `task.meta.tmux_session` in many places. All of these must be updated. Add convenience methods like:
- `task.meta.primary_repo()` → `&RepoEntry` (first entry)
- `task.meta.is_multi_repo()` → `bool` (repos.len() > 1)
- `task.meta.task_id()` → uses `self.name` instead of `self.repo_name`

The existing `Config::task_id(repo_name, branch_name)` static method stays the same — callers just pass `name` (the parent dir name or repo name) instead of always `repo_name`.

### Task ID for multi-repo tasks

**Decision: Task ID uses parent directory name.** Format remains `<name>--<branch>`. For single-repo tasks, `<name>` is the repo name (no change). For multi-repo tasks, `<name>` is the parent directory name (e.g. if user selected `~/repos/`, the name is `repos`). Keep it simple — no special naming in the wizard.

### Agent working directory for multi-repo tasks

**Decision: One agent instance works in the parent directory.** For multi-repo tasks, set the agent's working directory (`current_dir` in `Agent::run_direct()` at `agent.rs:129`) to the parent directory (the non-git dir the user selected). The agent navigates into individual repo worktrees as needed. The prompt lists all repo worktree paths explicitly.

For single-repo tasks, the working directory remains the worktree of the single repo entry (no change from today's behavior).

This means `Agent::run_direct()` needs to determine the working directory based on the task:
- Single-repo: `task.meta.repos[0].worktree_path`
- Multi-repo: the parent directory (needs to be stored somewhere — see "Multi-repo parent path" below)

### Multi-repo parent path

The task needs to know the parent directory path for multi-repo tasks (to set as agent working directory). Store this as an `Option<PathBuf>` field on `TaskMeta`:

```rust
pub struct TaskMeta {
    // ... existing fields ...
    /// For multi-repo tasks: the parent directory containing all repos.
    /// None for single-repo tasks.
    pub parent_dir: Option<PathBuf>,
}
```

When `parent_dir` is `Some`, the agent's working directory is that path. When `None`, the agent works in `repos[0].worktree_path`.

### Agent prompt changes for multi-repo awareness

`Agent::build_prompt()` (at `agent.rs:27-87`) will include information about all repos in the task. For multi-repo tasks, the prompt will include:
- A `# Repos` section listing all repos with their worktree paths
- The task dir path (so agents can read/write TASK.md at `~/.agman/tasks/<task_id>/TASK.md`)

For single-repo tasks, the prompt will include the task dir path (since TASK.md has moved there) but the repos section is optional/minimal.

For git context (`get_git_diff`, `get_git_log_summary`), these currently run in `self.meta.worktree_path`. For multi-repo tasks, they should run in each repo's worktree and concatenate the results (with repo name headers).

### Post-step hook mechanism in the flow runner

**Decision: Add a post-step hook mechanism to the flow runner.** After the repo-inspector agent finishes (outputs AGENT_DONE), agman needs to:
1. Parse the `# Repos` section from TASK.md
2. Create a worktree for each listed repo (using `Git::create_worktree_quiet`)
3. Create a tmux session for each repo (using `Tmux::create_session_with_windows`)
4. Populate `task.meta.repos` with the new entries and save

**Implementation approach:** Add an optional `post_hook` field to `AgentStep` in `flow.rs`:

```yaml
- agent: repo-inspector
  until: AGENT_DONE
  post_hook: setup_repos   # new field
```

```rust
pub struct AgentStep {
    pub agent: String,
    pub until: StopCondition,
    pub on_blocked: Option<BlockedAction>,
    pub on_fail: Option<FailAction>,
    pub post_hook: Option<String>,   // new field
}
```

In the flow runner (`run_flow_with` at `agent.rs:260`), after an agent completes and before advancing the flow step, check if `post_hook` is set. If it's `"setup_repos"`, run the repo-setup logic. This keeps the hook system generic (just a string identifier) but only implement the one hook we need right now.

The `setup_repos` hook logic should be a function in `use_cases.rs` that:
1. Reads TASK.md from the task dir
2. Parses the `# Repos` section (expects a list of repo names — the repo-inspector agent writes these)
3. For each repo: creates a worktree via `Git::create_worktree_quiet`, creates a tmux session via `Tmux::create_session_with_windows`
4. Populates `task.meta.repos` with `RepoEntry` for each repo
5. Saves the updated meta

### New "repo-inspector" agent

A new agent prompt at `~/.agman/prompts/repo-inspector.md`. This agent:
- Receives the task description and the parent directory path
- Inspects git repos under that directory (can look at READMEs, code structure, etc.)
- Writes a `# Repos` section into TASK.md listing which repos are involved and why
- Outputs `AGENT_DONE`

The prompt should be added as a `const` in `config.rs` (like existing `DEFAULT_FLOW`, etc.) and written by `init_default_files()`.

The `# Repos` section format in TASK.md should be machine-parseable. Suggested format:
```markdown
# Repos
- repo-name-1: Brief rationale for including this repo
- repo-name-2: Brief rationale for including this repo
```

The parser in the `setup_repos` hook extracts repo names from lines matching `- <name>:` under the `# Repos` heading.

### New `new-multi` flow

Add a `NEW_MULTI_FLOW` constant in `config.rs` and write it in `init_default_files()`:

```yaml
name: new-multi
steps:
  - agent: repo-inspector
    until: AGENT_DONE
    post_hook: setup_repos
  - agent: prompt-builder
    until: AGENT_DONE
  - agent: planner
    until: AGENT_DONE
  - loop:
      - agent: coder
        until: AGENT_DONE
        on_blocked: pause
      - agent: checker
        until: AGENT_DONE
    until: TASK_COMPLETE
```

### Tmux session management for multi-repo

Each repo in a multi-repo task gets its own tmux session, using the same naming convention: `(<repo>)__<branch>`. The TUI's "attach" action (`Enter` key in preview) for a multi-repo task should present a selection list letting the user choose which repo's session to attach to. For single-repo tasks, it attaches directly (no change).

### Wizard changes

The TUI wizard's `scan_repos()` (in `app.rs`) currently only returns git repos. Changes needed:

1. `scan_repos()` should also return non-git directories that contain git repos (i.e., parent directories like `~/repos/`). These should be visually distinguished in the list (e.g., prefixed with `[multi]` or similar).
2. When a non-git dir is selected:
   - Skip the branch-source step entirely (always `NewBranch`, since there's no single repo to pick existing branches from)
   - Use `flow_name = "new-multi"` instead of `"new"`
   - The `name` for task ID is the selected directory's name
3. The wizard state machine (`NewTaskWizard`) needs to handle this new path through the steps.

### Multi-repo task creation flow

When the wizard creates a multi-repo task:
1. `use_cases::create_task()` is called with `name = parent_dir_name`, `parent_dir = Some(path)`, `repos = vec![]` (empty — repos haven't been determined yet), `flow_name = "new-multi"`
2. TASK.md is written to `~/.agman/tasks/<name>--<branch>/TASK.md` with the goal description
3. No worktree or tmux sessions are created yet (repos haven't been determined)
4. A single temporary tmux session is created for the repo-inspector agent to run in (working dir = parent directory)
5. `agman flow-run <task_id>` is sent to the tmux session
6. The repo-inspector agent runs, writes `# Repos` to TASK.md, outputs AGENT_DONE
7. The `setup_repos` post-hook fires: creates worktrees + tmux sessions for each repo, populates `task.meta.repos`
8. The flow continues with prompt-builder → planner → coder↔checker

For step 4, the initial tmux session for the repo-inspector can use a session name based on the task name (e.g., `(repos)__<branch>`). After repos are set up, the individual repo sessions are created. The initial session can remain (it becomes the "parent" session) or be killed.

### Delete task for multi-repo

When deleting a multi-repo task with `DeleteMode::Everything`, iterate over all `repos` entries and remove each worktree/branch/tmux session. The existing `delete_task` in `use_cases.rs` (line 153) needs to loop over `task.meta.repos` instead of using the single `repo_name`/`worktree_path`.

### Git diff/log for multi-repo

`Task::get_git_diff()` and `Task::get_git_log_summary()` (at `task.rs:696-720`) currently run in `self.meta.worktree_path`. For multi-repo tasks, they should iterate over all repos in `self.meta.repos`, run git commands in each worktree, and concatenate results with repo name headers. For single-repo tasks, behavior is unchanged.

### `.pr-link` sidecar handling for multi-repo

In `AgentRunner::run_agent()` (at `agent.rs:216-229`), the `.pr-link` file is checked in `task.meta.worktree_path`. For multi-repo tasks, check in each repo's worktree path, or in the parent directory. The `linked_pr` field on TaskMeta should probably become repo-specific eventually, but for now keeping it task-level is fine — just check all worktree paths.

## Key Files to Modify

- `src/task.rs` — `TaskMeta` struct (add `RepoEntry`, `repos`, `name`, `parent_dir`; remove `repo_name`, `worktree_path`, `tmux_session`), `Task::read_task()`/`write_task()` (use `self.dir`), `get_git_diff()`/`get_git_log_summary()` (multi-repo support), `ensure_git_excludes_task()` (simplify to REVIEW.md only), `delete()` method
- `src/config.rs` — Add `NEW_MULTI_FLOW` and `REPO_INSPECTOR_PROMPT` constants, update `init_default_files()`, no changes to `task_id()`/`parse_task_id()` (they stay generic)
- `src/flow.rs` — Add `post_hook: Option<String>` to `AgentStep`
- `src/agent.rs` — `Agent::build_prompt()` (add repos section, task dir path, multi-repo git context), `Agent::run_direct()` (determine working dir from task), `AgentRunner::run_flow_with()` (check and execute post-hooks after agent steps)
- `src/use_cases.rs` — Generalize `create_task()` for multi-repo (accept `name`, `parent_dir`, initial `repos`), generalize `delete_task()` to loop over repos, add `setup_repos_from_task_md()` function for the post-hook
- `src/tui/app.rs` — Wizard changes (`scan_repos` to include non-git parent dirs, skip branch step for multi-repo, use `new-multi` flow), attach logic (session selection for multi-repo), create_task_from_wizard changes
- `src/tui/ui.rs` — Display multi-repo tasks differently (show repo count or `[multi]` indicator)
- `src/tmux.rs` — May need a helper to create sessions for multiple repos in a task
- `src/main.rs` — `cmd_flow_run` may need minor adjustments for multi-repo working directory
- `tests/use_cases_test.rs` — Add happy-path tests for multi-repo task creation, deletion, and the setup_repos hook

## Constraints from CLAUDE.md

- **TUI-only**: No new user-facing CLI subcommands. The `flow-run` hidden command is fine.
- **Testing**: Every new use-case function needs a happy-path test in `tests/use_cases_test.rs`. No mocking — use `TempDir` + real filesystem.
- **Logging**: All state changes need structured logging with `task_id`. Log where errors are handled, not where they propagate.
- **Avoid over-engineering**: Keep the implementation minimal — don't add features beyond what's needed for multi-repo support.
- **No migration**: Existing tasks will break. That's acceptable.

# Plan

## Completed
- [x] **1.1 Add `RepoEntry` struct and update `TaskMeta` in `src/task.rs`** — Added RepoEntry, replaced singular fields with `name` + `repos: Vec<RepoEntry>` + `parent_dir: Option<PathBuf>`, added `primary_repo()` and `is_multi_repo()` convenience methods
- [x] **1.2 Move TASK.md to the task dir** — `read_task()` and `write_task()` now use `self.dir.join("TASK.md")` instead of worktree path
- [x] **1.3 Simplify `ensure_git_excludes_task()`** — Only excludes REVIEW.md now, iterates over all `self.meta.repos`
- [x] **1.4 Update `get_git_diff()` and `get_git_log_summary()`** — Multi-repo support with repo name headers, empty-repos guard
- [x] **2.1 Add `post_hook` field to `AgentStep`** — Added `#[serde(default)] pub post_hook: Option<String>` to AgentStep in flow.rs
- [x] **3.1 Update `Agent::run_direct()` working directory** — Uses `parent_dir` if set, otherwise `primary_repo().worktree_path`
- [x] **3.3 Update `.pr-link` sidecar handling** — Checks all repo worktree paths and parent_dir
- [x] **3.4 Update `Agent::run_in_tmux()`** — Uses `primary_repo().tmux_session`
- [x] **4.2 Generalize `delete_task()`** — Iterates over all repos for worktree/branch removal; TaskOnly just deletes task dir
- [x] **5.1-5.3 Update all main.rs command handlers** — All `worktree_path`/`tmux_session`/`repo_name` references updated to use `primary_repo()`
- [x] **6.6-6.8 Update TUI references** — All `repo_name`→`name`, `worktree_path`→`primary_repo().worktree_path`, `tmux_session`→`primary_repo().tmux_session` across app.rs and ui.rs
- [x] **7.1-7.2 Update test helpers and existing tests** — `create_test_task()` and all assertions updated for new data model
- [x] **8.1-8.2 Build and test verification** — cargo build succeeds, all 73 tests pass

## Remaining

### Phase 2: Flow System (continued)

- [ ] **2.2 Add `new-multi` flow and `repo-inspector` prompt constants in `src/config.rs`**
  - Add `const NEW_MULTI_FLOW` with the YAML for new-multi (repo-inspector with post_hook → prompt-builder → planner → coder↔checker loop)
  - Add `const REPO_INSPECTOR_PROMPT` with the agent prompt for inspecting repos
  - Update `init_default_files()` to write these files

- [ ] **2.3 Add `setup_repos_from_task_md()` use case in `src/use_cases.rs`**
  - Extract `parse_repos_from_task_md(content: &str) -> Vec<String>` as a pure function (testable)
  - Full function: reads TASK.md, parses `# Repos` section, creates worktrees + tmux sessions, populates `task.meta.repos`

- [ ] **2.4 Implement post-hook execution in `AgentRunner::run_flow_with()` in `src/agent.rs`**
  - After AgentDone detected: check `agent_step.post_hook == Some("setup_repos")`, call `setup_repos_from_task_md()`

### Phase 3: Agent Prompt Changes

- [ ] **3.2 Update `Agent::build_prompt()` for multi-repo awareness in `src/agent.rs`**
  - Add `# Task Directory` section with `task.dir.display()` so agents know where TASK.md lives
  - For multi-repo: add `# Repos` section listing each repo with its worktree path

### Phase 4: Use Cases Updates

- [ ] **4.1 Generalize `create_task()` in `src/use_cases.rs`**
  - Add `parent_dir: Option<PathBuf>` parameter
  - For multi-repo: skip worktree creation, create task with empty repos, set parent_dir
  - For single-repo: behavior unchanged

- [ ] **4.3 Generalize `create_setup_only_task()` in `src/use_cases.rs`** (minimal — single-repo only)

- [ ] **4.4 Generalize `create_review_task()` in `src/use_cases.rs`** (pass-through name parameter)

### Phase 5: Main Command Handler Hardening

- [ ] **5.1-5.3 Add empty-repos guards in main.rs command handlers**
  - Guard `wipe_review_md` calls with `!task.meta.repos.is_empty()`
  - Guard tmux session recreation for multi-repo (iterate all repos)
  - Guard `cmd_command_flow_run` post_action delete to iterate all repos

### Phase 6: TUI Changes

- [ ] **6.1 Update `scan_repos()` to include non-git parent directories**
- [ ] **6.2 Update `NewTaskWizard` for multi-repo path (skip branch source, use new-multi flow)**
- [ ] **6.3 Update `create_task_from_wizard()` for multi-repo**
- [ ] **6.4 Update `delete_task()` TUI method to kill ALL tmux sessions in repos**
- [ ] **6.5 Update attach logic for multi-repo (initially: attach to first repo's session)**
- [ ] **6.8 Add visual indicator for multi-repo tasks in ui.rs**

### Phase 7: Tests

- [ ] **7.3 Add multi-repo task creation test**
- [ ] **7.4 Add `parse_repos_from_task_md()` test**
- [ ] **7.5 Add multi-repo task deletion test**
- [ ] **7.6 Add flow post_hook parsing test, update config test for new-multi flow**

## Status

### Iteration 1 — Data Model Foundation

**What was done:** Completed the entire Phase 1 (data model changes) and Phase 2.1 (flow post_hook field), plus all cascading compilation fixes across the entire codebase. This was the largest and riskiest change — replacing singular `repo_name`/`worktree_path`/`tmux_session` fields with a `Vec<RepoEntry>` and a `name` field, plus moving TASK.md from the worktree to the task directory.

**Key changes:**
- `task.rs`: New `RepoEntry` struct, updated `TaskMeta` with `name`, `repos`, `parent_dir`, convenience methods `primary_repo()` and `is_multi_repo()`
- `task.rs`: `read_task()`/`write_task()` now use `self.dir.join("TASK.md")` instead of worktree
- `task.rs`: `ensure_git_excludes_task()` simplified to only REVIEW.md, iterates all repos
- `task.ts`: `get_git_diff()`/`get_git_log_summary()` handle multi-repo with headers
- `flow.rs`: `AgentStep.post_hook: Option<String>` added
- `agent.rs`: Working dir uses `parent_dir` if set; `.pr-link` checks all repos; tmux uses `primary_repo()`
- `use_cases.rs`: `delete_task()` iterates all repos
- `main.rs`, `app.rs`, `ui.rs`: All references updated to new accessor pattern
- All 73 tests pass, build is clean

**No problems encountered.** The Rust compiler made this straightforward — every reference to the old fields was caught as a compile error and fixed systematically.

**Next iteration should focus on:** Phase 2.2-2.4 (new-multi flow, repo-inspector prompt, setup_repos hook, post-hook execution), then Phase 4.1 (generalize create_task for multi-repo). These are the core runtime pieces that actually enable multi-repo task creation.
