# agman

Agent Manager — a TUI for orchestrating stateless AI agents across isolated git worktrees.

> **Warning: agman is reckless by design.** Supported harnesses are launched with high-trust settings so agents can read, write, and execute shell commands without normal confirmation prompts. Pi has no permission prompt or sandbox bypass to preconfigure; agman gives it the full tool allowlist. Do not use agman on a machine where unrestricted AI access to the filesystem and shell is unacceptable.

## What is agman?

agman gives each AI-driven development task its own git branch, git worktree, and tmux session — a 1:1:1 mapping that keeps tasks fully isolated. Multiple agents can work on different features simultaneously without branch-switching pain or context pollution.

You manage everything from a single TUI dashboard: create tasks, monitor agent progress, give feedback, review PRs, and attach to any task's tmux session — all without leaving the terminal.

**Platform:** macOS. Linux should work but is untested. Windows is not supported.

## Prerequisites

All non-harness dependencies are required. agman also checks that the configured harness CLI is on PATH and will tell you what's missing.

| Dependency | Purpose |
|---|---|
| [Rust](https://www.rust-lang.org/) | Building from source |
| `git` | Version control |
| `tmux` | Terminal multiplexer — one session per task |
| [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code) (`claude`), Codex CLI (`codex`), Goose CLI (`goose`), or [Pi](https://pi.dev/docs/latest) (`pi`, install with `npm install -g @mariozechner/pi-coding-agent`) | AI agent execution, selected with `harness = "..."` |
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
# Optional: claude, codex, goose, or pi
harness = "claude"
```

## Features

- **Task management** — create tasks via wizard, track status, give feedback, restart from specific flow steps
- **Agent orchestration** — YAML-defined flows chain specialized agents (coder, checker, reviewer, refiner, repo-inspector, etc.) with stop conditions and loop support
- **Git worktree isolation** — automatic worktree creation/cleanup per task, branch management, rebase workflows
- **Tmux integration** — dedicated session per task with pre-configured windows (nvim, lazygit, shell, agent)
- **GitHub integration** — draft PRs, CI monitoring, review tracking, local merge
- **Stored commands** — pre-packaged workflows: create-pr, review-pr, address-review, monitor-pr, rebase, local-merge
- **Vim-style TUI** — fully keyboard-driven with preview pane, built-in editors

## Harness Notes

Set `harness = "claude"`, `"codex"`, `"goose"`, or `"pi"` in `~/.agman/config.toml`, or switch it in the TUI settings view. Task agents read the global setting at spawn time. Long-lived Chief of Staff, PM, and researcher agents pin their first-spawn harness in `<state_dir>/harness`.

Pi launches with `PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 pi --offline`, an agman identity file passed through `--append-system-prompt`, a private `--session-dir`, and the full tool allowlist `read,bash,edit,write,grep,find,ls`. Long-lived Pi sessions store their private session files in `<state_dir>/pi-sessions` and resume with `--continue` from the stamped `<state_dir>/launch-cwd`.

## Tech Stack

Rust with [ratatui](https://github.com/ratatui/ratatui). Integrates with git (worktrees), tmux (sessions), runtime-selectable agent CLIs, and GitHub CLI (PRs).
