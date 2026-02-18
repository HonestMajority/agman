# CLAUDE.md - AI Assistant Guide for agman

## Project Overview

agman (Agent Manager) is a Rust CLI/TUI tool for orchestrating stateless AI agents across isolated git worktrees. It manages tasks where each task has a 1:1:1 relationship between:
- A git branch
- A git worktree
- A tmux session

## Design Principles

### TUI-only application

agman is exclusively an interactive TUI application. **No user-facing CLI subcommands should ever be added.** The only CLI subcommands permitted are hidden internal commands (`#[command(hide = true)]`) that the TUI invokes as background processes via subprocess or tmux. Current internal commands:

- `flow-run` — runs a flow for a task in a tmux session
- `continue` — continues a paused task with feedback
- `run-command` — executes a stored command in a worktree
- `command-flow-run` — runs a command-driven flow

If you need new functionality, expose it through the TUI interface and implement the business logic in `src/use_cases.rs`. Only add a hidden internal command if the TUI needs to run something in a separate process (e.g., inside a tmux session).

## Quick Reference

```bash
# Build and install
./release.sh

# Launch TUI (the only user-facing interface)
agman
```

## Architecture

```
src/
├── main.rs      # Entry point, internal command handlers
├── cli.rs       # Clap CLI definitions
├── config.rs    # Paths, default flows/prompts
├── task.rs      # Task state management
├── agent.rs     # Agent execution, prompt building, flow runner
├── flow.rs      # Flow/step parsing from YAML
├── git.rs       # Worktree operations
├── tmux.rs      # Tmux session management
└── tui/
    ├── mod.rs
    ├── app.rs   # TUI state and event handling
    └── ui.rs    # Ratatui rendering
```

## Key Concepts

### Task
- Lives in `~/.agman/tasks/<repo>--<branch>/`
- Contains: `meta.json`, `TASK.md`, `notes.md`, `agent.log`
- Status: `working`, `paused`, `done`, `failed`

### TASK.md Format
```markdown
# Goal
[What we're trying to achieve - the high-level objective]

# Plan
## Completed
- [x] Step that was done

## Remaining
- [ ] Next step to do
- [ ] Another step
```

### Flow
- YAML file in `~/.agman/flows/`
- Defines sequence of agents with stop conditions
- Example: `default.yaml` (planner → coder), `continue.yaml` (refiner → coder)

### Agent
- Prompt template in `~/.agman/prompts/<name>.md`
- Executed via `claude -p --dangerously-skip-permissions`
- Outputs magic strings: `AGENT_DONE`, `TASK_COMPLETE`, `INPUT_NEEDED`

### Stop Conditions
- `AGENT_DONE` - Agent finished its work, advance to next step
- `TASK_COMPLETE` - Task is done, mark as complete
- `INPUT_NEEDED` - Needs human input, pause

## File Locations

```
~/.agman/
├── tasks/           # Task state directories
├── flows/           # Flow YAML files
└── prompts/         # Agent prompt templates

~/repos/
├── <repo>/          # Main repository
└── <repo>-wt/       # Worktrees directory
    └── <branch>/    # Individual worktree
```

## Common Modifications

### Adding a new internal command
Internal commands are hidden subcommands invoked by the TUI via subprocess or tmux:
1. Add variant to `Commands` enum in `cli.rs` with `#[command(hide = true)]`
2. Add match arm in `main.rs`
3. Implement `cmd_<name>` function

### Adding a new agent
1. Create prompt in `~/.agman/prompts/<name>.md`
2. Reference in a flow YAML file

### Adding a new flow
1. Create `~/.agman/flows/<name>.yaml`
2. Reference in task creation or continue logic

### Modifying TUI
- State/logic: `src/tui/app.rs`
- Rendering: `src/tui/ui.rs`
- Key bindings are in `handle_*_event` methods

## TUI Key Bindings

### Task List
- `j/k` - Navigate, `g/G` - Jump to first/last
- `l/Enter` - Preview
- `n` - New task, `r` - Review wizard
- `R` - Rerun task (edit TASK.md + pick flow step)
- `S` - Stop running task
- `f` - Give feedback, `t` - Edit TASK.md, `x` - Run command
- `a` - Answer (InputNeeded tasks only)
- `o` - Open linked PR in browser
- `H` - Hold/unhold task
- `P` - Set linked PR, `c` - Clear review-addressed
- `d` - Delete task
- `N` - Notifications, `m` - Notes
- `Ctrl+R` - Refresh task list
- `q/Esc` - Quit

### Preview
- `Tab` - Switch pane (logs/notes)
- `j/k` - Scroll, `g/G` - Jump to top/bottom, `Ctrl+D/U` - Half-page scroll
- `i` - Edit notes
- `R` - Rerun task
- `S` - Stop running task
- `f` - Give feedback, `t` - Edit TASK.md, `x` - Run command
- `a` - Answer (InputNeeded tasks only)
- `o` - Open linked PR in browser
- `H` - Hold/unhold task, `P` - Set linked PR
- `Q` - Queue feedback
- `Enter` - Attach to tmux
- `q/Esc` - Back

## Build Commands

```bash
cargo build --release    # Build
cargo nextest run        # Run tests (preferred)
cargo test               # Run tests (fallback)
./release.sh             # Build + install to ~/commands/
```

## Debugging

- Task logs: `~/.agman/tasks/<task_id>/agent.log`
- Task state: `~/.agman/tasks/<task_id>/meta.json`
- Check tmux: `tmux ls` then `tmux attach -t <session>`

## Logging Policy

agman uses `tracing` for structured logging to `~/.agman/agman.log` (file only, no stderr). Setup in `src/logging.rs`. Default filter: `agman=debug,warn`.

**Task ID rule.** Always include `task_id` as a structured field when task context is available — it is the primary correlation key. Example: `tracing::info!(task_id = %task.meta.task_id(), "deleting task");`

**Log levels.** `error!` — unrecoverable failures. `warn!` — degraded but non-fatal. `info!` — significant actions and state changes. `debug!` — troubleshooting details. `trace!` — high-frequency iteration detail.

**Error logging.** Log errors where they are *handled*, not where they are propagated. Never log-and-propagate (`error!` + `return Err(...)`) — this duplicates entries.

```rust
if let Err(e) = use_cases::delete_task(&config, &task_id) {
    tracing::error!(task_id = %task_id, error = %e, "failed to delete task");
    self.set_error(format!("Delete failed: {e}"));
}
```

**Action logging.** Every user-visible action and state change must produce at least one log line: use-case entry, TUI key handlers that modify state, agent start/stop and magic strings, flow step transitions, git worktree operations.

**Structured fields.** Use consistently: `task_id` (any task op), `repo`/`branch` (git ops), `agent`/`flow`/`step` (execution), `status` (transitions, old+new), `error` (`error = %e`).

**Console output vs. tracing.** `println!` is for user-visible progress in tmux sessions (internal commands). `tracing::*` is for `agman.log`. No `println!` in library code; no `tracing` for tmux user output.

## Testing

### Philosophy

- **One happy-path test per TUI feature.** Tests are organized around user-visible TUI features, not around internal modules.
- **Tests track features.** When a TUI feature is added, a test is added. When a feature changes, the test is updated. When a feature is removed, the test is removed.
- **TUI features only.** Do not test CLI command handlers — only test the code paths the TUI uses.

### Architecture

Business logic lives in `src/use_cases.rs` as standalone public functions. Each function takes `Config` (and relevant args) and returns results — no TUI state, no tmux calls, no `App` dependency. The TUI `App` methods in `app.rs` are thin wrappers that call use-case functions and then update UI state.

Tests call use-case functions directly with `TempDir`-backed configs.

### Test structure

```
tests/
├── helpers/mod.rs        # test_config(), init_test_repo(), create_test_task()
├── use_cases_test.rs     # TUI use-case tests (one per feature)
├── flow_test.rs          # Flow parsing and executor logic
├── agent_test.rs         # Agent loading and prompt building
├── config_test.rs        # Config path resolution
├── command_test.rs       # Stored command loading
├── repo_stats_test.rs    # Repo stats
└── git_test.rs           # Git worktree operations
```

### How to write a new test

1. Add the use-case function to `src/use_cases.rs`
2. Wire the TUI `App` method to call the new use-case function
3. Add a test in `tests/use_cases_test.rs`:

```rust
#[test]
fn my_new_feature() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    // Set up any needed state (repos, tasks, etc.)
    let _repo = init_test_repo(&tmp, "myrepo");

    // Call the use-case function
    let result = use_cases::my_function(&config, ...).unwrap();

    // Assert on filesystem state and return values
    assert!(result.dir.join("meta.json").exists());
}
```

### Rules

- **No model invocations.** The `claude` CLI must never be called during tests.
- **No machine pollution.** All state isolated to `tempfile::TempDir`. No real `~/.agman/`, `~/repos/`, or tmux sessions.
- **No mocking frameworks.** Real filesystem with temp dirs.
- **Happy paths only.** One simple test per TUI feature. No edge cases or error handling tests.
- **Test every use case.** Every TUI use case added to `src/use_cases.rs` must have a corresponding happy-path test in `tests/use_cases_test.rs`.
- **Run tests:** `cargo nextest run` (or `cargo test` as fallback)

### Test isolation pattern

Every test uses `TempDir` + `test_config()` for full isolation:

```rust
let tmp = tempfile::tempdir().unwrap();
let config = test_config(&tmp);        // Config rooted in temp dir
let _repo = init_test_repo(&tmp, "myrepo");  // Git repo with initial commit
let task = create_test_task(&config, "repo", "branch");  // Minimal task
```

## Important Patterns

1. **Borrow checker workarounds**: Extract values before mutable borrows (see `load_preview` in app.rs)

2. **macOS code signing**: Release builds must be ad-hoc signed (`codesign -s -`) to avoid being killed by Gatekeeper

3. **Terminal control chars**: Avoid Ctrl+H/L (intercepted by terminal). Use Tab or other keys instead.

4. **Flow progression**: Agents output magic strings, flow runner detects them and advances steps

5. **Feedback loop**: `continue` flow uses refiner to synthesize feedback into fresh TASK.md, avoiding context accumulation

6. **Status bar hints rule**: Every user-facing key binding in a view must have a corresponding hint in `draw_status_bar()` in `ui.rs`. Hints must only be shown when the action is applicable — e.g., "stop" only appears for running tasks, "answer" only for input-needed tasks. When adding a new key binding, always add the hint. When removing a binding, remove the hint.
