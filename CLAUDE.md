# CLAUDE.md - AI Assistant Guide for agman

## Project Overview

agman (Agent Manager) is a Rust CLI/TUI tool for orchestrating stateless AI agents across isolated git worktrees. It manages tasks where each task has a 1:1:1 relationship between:
- A git branch
- A git worktree
- A tmux session

## Quick Reference

```bash
# Build and install
./release.sh

# Commands
agman                    # Launch TUI
agman new <repo> <branch> "description" [--flow <flow>]
agman list
agman attach <task_id>
agman continue <task_id> "feedback" [--flow continue]
agman delete <task_id> [-f]
agman pause <task_id>
agman resume <task_id>
agman init
```

## Architecture

```
src/
├── main.rs      # CLI entry point, command handlers
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
- Outputs magic strings: `AGENT_DONE`, `TASK_COMPLETE`, `TASK_BLOCKED`, `TESTS_PASS`, `TESTS_FAIL`

### Stop Conditions
- `AGENT_DONE` - Agent finished its work, advance to next step
- `TASK_COMPLETE` - Task is done, mark as complete
- `TASK_BLOCKED` - Needs human intervention, pause
- `TESTS_PASS` / `TESTS_FAIL` - For test-driven flows

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

### Adding a new CLI command
1. Add variant to `Commands` enum in `cli.rs`
2. Add match arm in `main.rs`
3. Implement `cmd_<name>` function

### Adding a new agent
1. Create prompt in `~/.agman/prompts/<name>.md`
2. Reference in a flow YAML file

### Adding a new flow
1. Create `~/.agman/flows/<name>.yaml`
2. Use with `agman new --flow <name>` or `agman continue --flow <name>`

### Modifying TUI
- State/logic: `src/tui/app.rs`
- Rendering: `src/tui/ui.rs`
- Key bindings are in `handle_*_event` methods

## TUI Key Bindings

### Task List
- `j/k` - Navigate
- `l/Enter` - Preview
- `f` - Give feedback
- `p/r` - Pause/Resume
- `d` - Delete
- `q` - Quit

### Preview
- `Tab` - Switch pane (logs/notes)
- `j/k` - Scroll
- `i` - Edit notes
- `f` - Give feedback
- `Enter` - Attach to tmux
- `q` - Back

## Build Commands

```bash
cargo build --release    # Build
cargo test              # Run tests
./release.sh            # Build + install to ~/commands/
```

## Debugging

- Task logs: `~/.agman/tasks/<task_id>/agent.log`
- Task state: `~/.agman/tasks/<task_id>/meta.json`
- Check tmux: `tmux ls` then `tmux attach -t <session>`

## Important Patterns

1. **Borrow checker workarounds**: Extract values before mutable borrows (see `load_preview` in app.rs)

2. **macOS code signing**: Release builds must be ad-hoc signed (`codesign -s -`) to avoid being killed by Gatekeeper

3. **Terminal control chars**: Avoid Ctrl+H/L (intercepted by terminal). Use Tab or other keys instead.

4. **Flow progression**: Agents output magic strings, flow runner detects them and advances steps

5. **Feedback loop**: `continue` flow uses refiner to synthesize feedback into fresh TASK.md, avoiding context accumulation
