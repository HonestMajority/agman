# CLAUDE.md - AI Assistant Guide for agman

## Project Overview

agman (Agent Manager) is a Rust CLI/TUI tool for orchestrating stateless AI agents across isolated git worktrees. It manages tasks where each task has a 1:1:1 relationship between:
- A git branch
- A git worktree
- A tmux session

## Design Principles

### TUI and CLI

agman has two interfaces: a TUI (`agman` with no args) for interactive use, and CLI subcommands (`agman <command>`) used by PM agents and for scripting. Business logic lives in `src/use_cases.rs` — both the TUI and CLI commands call into the same use-case functions.

## Quick Reference

```bash
# Build and install
./release.sh

# Launch TUI
agman

# List CLI subcommands
agman --help
```

## Architecture

```
src/
├── main.rs      # Entry point, CLI command handlers
├── cli.rs       # Clap CLI definitions
├── config.rs    # Paths, default flows/prompts
├── task.rs      # Task state management
├── agent.rs     # Agent execution, prompt building, flow runner
├── flow.rs      # Flow/step parsing from YAML
├── git.rs       # Worktree operations
├── tmux.rs      # Tmux session management
├── harness/     # Pluggable AI CLI harness (claude / codex)
│   ├── mod.rs   # Harness trait, HarnessKind, LaunchContext
│   ├── claude.rs
│   └── codex.rs
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

TASK.md is the task description. The only convention is a single `# Goal` heading at the top:

```markdown
# Goal
What the task is, scoped by the PM.
```

Agents may append a short `## Notes` section if the next iteration needs context that isn't visible from git history. The refiner rewrites TASK.md on feedback iterations.

### Flow
- YAML file in `~/.agman/flows/`
- Defines sequence of agents with stop conditions
- Example: `new.yaml` (coder ↔ checker loop), `continue.yaml` (refiner → coder ↔ checker loop)

### Agent
- Prompt template in `~/.agman/prompts/<name>.md`
- Executed via the configured **harness** (`claude` or `codex`) — see "Harnesses" below
- Outputs magic strings: `AGENT_DONE`, `TASK_COMPLETE`, `INPUT_NEEDED`

### Harnesses

agman supports two interactive AI agent CLIs, selected at runtime:

- **claude** (Anthropic Claude Code) — default
- **codex** (OpenAI Codex CLI)

The choice is set in the TUI settings view (`,` from the task list) on the **Harness** row (h/l to switch). It persists to `~/.agman/config.toml` as `harness = "..."` and applies to **newly-spawned agents only**.

Newly-spawned **task** agents always read the current global `harness` setting from `config.toml` at spawn time. Task agents have no per-task pin. The harness used at spawn is recorded on each `session_history[N]` entry so the kill path uses the right slash command (`/exit` for claude, `/quit` for codex).

Long-lived agents (Chief of Staff / PM / researcher) **stamp** their harness on first spawn at `<state_dir>/harness` so that resume-by-name (`claude --resume <agent-name>` / `codex resume <agent-name>`) always uses the harness that owns the conversation. A global flip does not affect a long-lived agent's existing conversation. To start a long-lived agent under a new harness, run `agman respawn-agent <target>` — respawn wipes the harness stamp (alongside `session-id` / `launch-cwd`) and the next spawn re-reads global. A tracing line is emitted on the re-read for visibility.

agman never resumes sessions programmatically. To revisit a historical conversation, the user runs the harness's resume command directly from a shell:
- `claude --resume agman-ceo` (or any name from `~/.agman/tasks/<id>/meta.json` `session_history[].name`)
- `codex resume agman-ceo`

Codex respects `AGENTS.md` in the worktree the same way claude respects `CLAUDE.md` — that's project-conventional, not agman's responsibility.

**Keep codex up to date.** Older codex versions display an update prompt at startup that blocks the TUI. agman registers the session name post-launch by paste-injecting `/rename <name>`; if codex is sitting on the update prompt, the rename gets swallowed and the session won't be resume-by-name. The session is still usable; just keep codex current.

**Restart-after-tmux-loss.** When agman *and* tmux both die, agman no longer auto-restores conversational context for CEO/PM. The `respawn_agent` handoff path (handoff.md + inbox.jsonl) is unchanged for in-process respawns. To recover full context after a tmux loss, manually `claude --resume` / `codex resume` from a shell and pick the agman session by name.

**Workspace trust is pre-stamped.** Both harnesses gate first launch in any new directory behind a "trust this folder?" dialog that the `--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` flags do NOT bypass. Before sending the launch command to tmux, agman calls `Harness::ensure_workspace_trusted(cwd)` to register the directory as trusted in the harness's user-global config:
- Claude: `~/.claude.json` → `projects[<cwd>].hasTrustDialogAccepted = true` (root-level dot file, NOT inside `~/.claude/`).
- Codex: `~/.codex/config.toml` → `[projects."<cwd>"] trust_level = "trusted"`.

The helper is idempotent (no-op when the entry is already trusted; preserves mtime), tolerates missing files/dirs, and preserves all other keys in the config. Failure is fatal: a launch that hits the trust dialog never reaches a usable agent state and downstream `/rename` paste-injects would run as shell commands.

Storage layout for harness stamps:
```
~/.agman/
  ceo/harness                       ← "claude" | "codex"
  projects/<name>/harness           ← "claude" | "codex"
  researchers/<project>--<n>/harness ← "claude" | "codex"
```

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

### Default file upgrades

`agman init` (run implicitly on first launch) only writes embedded default flows and prompts when the target file is *absent* — it never overwrites existing files. So when the embedded defaults change (e.g. a retired flow stage or a reworked prompt), users who ran agman before the change keep their stale copies on disk. To pick up new defaults, run `agman init --force` to re-materialize everything, or delete the specific file under `~/.agman/flows/` or `~/.agman/prompts/` and re-run `agman init`. Obsolete files from retired stages (e.g. `prompts/prompt-builder.md`, `prompts/planner.md`) are harmless but can be deleted manually — there is no automatic cleanup.

User-installed prompts in `~/.agman/prompts/` (`refiner.md`, `prompt-builder.md`, `planner.md`) carry hard-coded references to claude-specific concepts (e.g. `.claude/skills/`). Run `agman init --force` to refresh these after upgrading agman if you want them harness-neutral. The built-in `coder` / `checker` / `reviewer` prompts are sourced from the binary at runtime and updated automatically on each release.

## Common Modifications

### Adding a new CLI command
1. Add variant to `Commands` enum in `cli.rs`
2. Add match arm in `main.rs`
3. Implement `cmd_<name>` function (business logic goes in `use_cases.rs`)

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
- `n` - New task, `v` - Review wizard
- `r` - Rerun task (edit TASK.md + pick flow step)
- `s` - Stop running task
- `f` - Give feedback, `t` - Edit TASK.md, `x` - Run command
- `a` - Answer (InputNeeded tasks only)
- `o` - Open linked PR in browser
- `h` - Hold/unhold task
- `c` - Clear review-addressed
- `d` - Delete task
- `i` - Inbox (notifications), `p` - PRs, `m` - Notes
- `b` - Break timer reset
- `,` - Settings
- `Ctrl+C` - Quit

### Preview
- `Tab` - Switch pane (logs/notes)
- `j/k` - Scroll, `g/G` - Jump to top/bottom, `Ctrl+D/U` - Half-page scroll
- `i` - Edit notes
- `r` - Rerun task
- `s` - Stop running task
- `f` - Give feedback, `t` - Edit TASK.md, `x` - Run command
- `a` - Answer (InputNeeded tasks only)
- `o` - Open linked PR in browser
- `h` - Hold/unhold task
- `w` - Queue feedback
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

6. **Status bar hints rule**: Every user-facing key binding in a view must have a corresponding hint in `draw_status_bar()` in `ui.rs`. Hints must only be shown when the action is applicable — e.g., "stop" only appears for running tasks, "answer" only for input-needed tasks. When adding a new key binding, always add the hint. When removing a binding, remove the hint. In the main navigation views (ProjectList, TaskList, ResearcherList), all key binding hints must use lowercase letters only — no capitals, no modifier key combinations. Other views (editors, wizards, Notes) are exempt from this rule.
