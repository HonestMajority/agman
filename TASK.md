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
- [x] Data model: `RepoEntry`, `TaskMeta` with `name`/`repos`/`parent_dir`, convenience methods (`primary_repo()`, `is_multi_repo()`, `has_repos()`)
- [x] TASK.md moved to task dir (`self.dir.join("TASK.md")`) for all tasks
- [x] `ensure_git_excludes_task()` simplified to REVIEW.md only, iterates all repos
- [x] `get_git_diff()` / `get_git_log_summary()` multi-repo support with repo name headers
- [x] `post_hook: Option<String>` field on `AgentStep` in flow.rs
- [x] `.pr-link` sidecar checks all repo worktree paths + parent_dir
- [x] `delete_task()` iterates all repos for worktree/branch/tmux cleanup
- [x] `new-multi` flow, `repo-inspector` prompt in config.rs, `init_default_files()` updated
- [x] `setup_repos_from_task_md()` use case — parser + idempotent setup with incremental saves
- [x] Post-hook execution in `run_flow_with()` via `execute_post_hook()`
- [x] `build_prompt()` adds `# Task Directory` and `# Repo Worktrees` sections
- [x] Multi-repo task creation: `TaskMeta::new_multi()`, `Task::create_multi()`, `create_multi_repo_task()` use case
- [x] TUI wizard: `scan_repos_with_parents()`, `is_multi_repo` tracking, `create_task_from_wizard()` multi-repo path
- [x] All `primary_repo()` calls in app.rs guarded (30+ sites: delete, stop, resume, restart, attach, PR poll, branch picker, set_linked_pr)
- [x] `[M]` visual indicator for multi-repo tasks in TUI
- [x] Test helpers and existing tests updated for new data model
- [x] 7 new multi-repo tests (creation, parsing, deletion, flow, setup_repos) — all 80 pass
- [x] Made tmux calls in `setup_repos_from_task_md()` non-fatal (warn + skip instead of propagating errors)
- [x] Guard setup-only task creation against multi-repo selection — shows error "Multi-repo tasks require a description"
- [x] Multi-repo attach session selection — `View::SessionPicker` overlay lists repo tmux sessions, user picks which to attach to
- [x] Fixed stale comments referencing "worktree" for TASK.md location (now writes to task dir)

## Remaining

(No remaining items — all planned features are implemented.)

## Status

### Iteration 9

**Build:** Compiles. **Tests:** All 80 pass.

**What was done:**

1. **Setup-only + multi-repo guard:** Added `is_multi_repo` check before the empty-description branch in `wizard_next_step()`. Multi-repo parent dirs now show error "Multi-repo tasks require a description" instead of attempting a single-repo setup-only task that would fail.

2. **Multi-repo attach session picker:** Added a new `View::SessionPicker` with full TUI overlay:
   - `app.rs`: New state fields (`session_picker_sessions`, `selected_session_index`, `attach_session_name`), event handler (`handle_session_picker_event`), updated preview attach logic to open picker for multi-repo tasks with >1 repo.
   - `ui.rs`: New `draw_session_picker()` function rendering a centered popup with repo name list, keybinding hints shared with `RebaseBranchPicker`.
   - `run_tui` loop: Checks `attach_session_name` first (set by picker) before falling back to primary_repo logic.

3. **Stale comments:** Fixed 3 comments in `task.rs` and `tests/use_cases_test.rs` that said "worktree" where TASK.md now lives in the task dir.
