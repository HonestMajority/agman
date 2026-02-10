# agman

Agent Manager — a TUI for orchestrating stateless AI agents across isolated git worktrees.

## What is agman?

agman gives each AI-driven development task its own git branch, git worktree, and tmux session — a 1:1:1 mapping that keeps tasks fully isolated. Multiple agents can work on different features simultaneously without branch-switching pain or context pollution.

You manage everything from a single TUI dashboard: create tasks, monitor agent progress, give feedback, review PRs, and attach to any task's tmux session — all without leaving the terminal.

## Features

**Task Management**
- Create tasks via interactive wizard with branch/worktree setup
- Track status: Running, Stopped, InputNeeded, OnHold
- Give feedback to running agents, answer agent questions
- Restart from specific flow steps, delete tasks

**Agent Orchestration**
- YAML-defined flows chain specialized agents (planner, coder, tester, reviewer, refiner, etc.)
- Stop conditions (`AGENT_DONE`, `TASK_COMPLETE`, `TASK_BLOCKED`, `TESTS_PASS`/`FAIL`, `INPUT_NEEDED`) control flow progression
- Loop support for iterative workflows like TDD

**Git Worktree Isolation**
- Automatic worktree creation and cleanup per task
- Branch management: create from main, use existing branches, or adopt existing worktrees
- Built-in rebase workflows

**Tmux Integration**
- Dedicated tmux session per task with pre-configured windows (nvim, lazygit, claude, zsh, agent)
- Attach to any task's session directly from the TUI

**GitHub Integration**
- Create draft PRs, monitor CI checks, retry flaky CI
- Track review comments, auto-trigger review-addressing flows
- Open PRs in browser, local merge after approval

**Stored Commands**
- Pre-packaged workflows: create-pr, review-pr, address-review, monitor-pr, rebase, local-merge

**TUI**
- Vim-style navigation throughout
- Preview pane with logs and notes
- Built-in text editors with vim keybindings
- Fully keyboard-driven

## Prerequisites

- [Rust](https://www.rust-lang.org/) (for building)
- git, tmux
- [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code) (AI agent execution)
- [GitHub CLI](https://cli.github.com/) (`gh`, for PR operations)

## Getting Started

```bash
./release.sh   # build and install
agman           # launch the TUI
```

## Tech Stack

Rust with [ratatui](https://github.com/ratatui/ratatui) for the TUI. Integrates with git (worktrees), tmux (sessions), Claude Code CLI (AI agents), and GitHub CLI (PR operations).
