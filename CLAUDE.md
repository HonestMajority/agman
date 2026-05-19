# CLAUDE.md - AI Agent Guide for agman

## Project Overview

agman is a Rust CLI/TUI for coordinating long-lived AI agents. It manages projects, tasks, git worktrees, tmux sessions, and inbox delivery.

## Current Model

- A project owns a PM agent.
- A task owns branch/worktree/status metadata and exactly one attached Engineer agent.
- Researcher, Tester, Reviewer, and Operator agents use the same persisted agent model as Engineers.
- Non-engineer agents can be unattached project agents or attached to tasks.
- Communication is inbox-based. Use `agman send-message <target> ...`.
- Message targets are concrete agents or PMs, not task IDs. Examples:
  - `engineer:<project>--<name>`
  - `reviewer:<project>--<name>`
  - `tester:<project>--<name>`
  - `researcher:<project>--<name>`
  - `operator:<project>--<name>`
  - `<project>` for the PM
  - `chief-of-staff`
  - `telegram`

## Architecture

```
src/
├── main.rs          # CLI command handlers
├── cli.rs           # Clap command definitions
├── config.rs        # Paths and defaults
├── task.rs          # Task metadata and logs
├── agent_model.rs   # Persisted agent metadata, kinds, attachments
├── agent.rs         # Agent prompt and launch payload building
├── supervisor.rs    # Task engineer session helpers
├── use_cases.rs     # Shared business logic for CLI and TUI
├── git.rs           # Worktree operations
├── tmux.rs          # Tmux helpers
├── harness/         # Claude, Codex, Goose, Pi adapters
└── tui/             # Ratatui app and rendering
```

## State Layout

```
~/.agman/
  agents/<project>--<agent>/
    meta.json
    inbox.jsonl
    inbox.seq
    harness
    session-name
  projects/<project>/meta.json
  tasks/<repo>--<branch>/meta.json
  prompts/engineer.md
```

## Development Commands

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Harnesses

agman supports Claude Code, Codex CLI, Goose CLI, and Pi. Long-lived agents stamp their harness on first launch so restarts resume the same conversation runtime. A global harness change applies to new launches after respawn.

## TUI Notes

The project detail view shows tasks and project agents. Attached agents are shown under their task; unattached project agents remain in the project agent list. Opening an agent row attaches to that agent chat.

The TUI does not expose task-file editors or staged task runners. PMs coordinate by messaging specific agents.
