# agman

Agent Manager — a TUI for orchestrating stateless AI agents across isolated git worktrees.

Each task gets its own git branch, worktree, and tmux session (1:1:1). Multiple agents can work on different features simultaneously without branch-switching or context pollution.

**Platform:** macOS. Linux should work but is untested. Windows is not supported.

## Prerequisites

All dependencies are required. agman checks for them on startup and will tell you what's missing.

| Dependency | Purpose |
|---|---|
| [Rust](https://www.rust-lang.org/) | Building from source |
| `git` | Version control |
| `tmux` | Terminal multiplexer — one session per task |
| [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code) (`claude`) | AI agent execution |
| `nvim` | Editor in tmux sessions |
| `lazygit` | Git TUI in tmux sessions |
| [GitHub CLI](https://cli.github.com/) (`gh`) | PR operations |
| `direnv` | Directory environment manager |

## Getting Started

```bash
# 1. Clone the repo
git clone <repo-url> && cd agman

# 2. Build and install
./release.sh

# 3. Add to PATH (if not already)
export PATH="$HOME/.agman/bin:$PATH"  # add to your shell profile

# 4. Launch
agman
```

## Configuration

agman stores its config and state in `~/.agman/`. On first launch:

- If `~/.agman/config.toml` doesn't exist, agman creates one with defaults
- The `repos_dir` key controls where agman looks for git repos (default: `~/repos/`)
- If the repos directory doesn't exist or has no git repos, a directory picker will appear

```toml
# ~/.agman/config.toml
repos_dir = "~/repos/"
```

## Features

- **Task management** — create tasks via wizard, track status, give feedback, restart from specific flow steps
- **Agent orchestration** — YAML-defined flows chain specialized agents (planner, coder, tester, reviewer, etc.) with stop conditions and loop support
- **Git worktree isolation** — automatic worktree creation/cleanup per task, branch management, rebase workflows
- **Tmux integration** — dedicated session per task with pre-configured windows (nvim, lazygit, claude, shell, agent)
- **GitHub integration** — draft PRs, CI monitoring, review tracking, local merge
- **Stored commands** — pre-packaged workflows: create-pr, review-pr, address-review, monitor-pr, rebase, local-merge
- **Vim-style TUI** — fully keyboard-driven with preview pane, built-in editors

## Tech Stack

Rust with [ratatui](https://github.com/ratatui/ratatui). Integrates with git (worktrees), tmux (sessions), Claude Code CLI (agents), and GitHub CLI (PRs).
