# agman

Agent Manager — a Rust CLI/TUI for coordinating long-lived AI agents across projects, tasks, git worktrees, and tmux sessions.

> **Warning: agman is reckless by design.** Supported harnesses are launched with high-trust settings so agents can read, write, and execute shell commands without normal confirmation prompts. Do not use agman on a machine where unrestricted AI access to the filesystem and shell is unacceptable.

## What is agman?

agman organizes engineering work around:

- Projects with a long-lived PM agent.
- Tasks that own branch/worktree/status metadata and exactly one attached Engineer agent.
- Additional Researcher, Tester, Reviewer, and Operator agents that can be unattached project agents or attached to tasks.
- Inbox messages as the coordination primitive. Use `agman send-message <agent-target> ...` to communicate with PMs and agents.

Tasks do not run staged agent pipelines. A task-attached Engineer is a normal long-lived agent with broad authority to implement, test, rebase, push, create or update PRs, monitor CI, and address review requests when the PM asks.

## Prerequisites

| Dependency | Purpose |
|---|---|
| Rust | Building from source |
| `git` | Version control and worktrees |
| `tmux` | Agent sessions and popups |
| Claude Code CLI, Codex CLI, Goose CLI, or Pi | Agent execution |
| `nvim` | Optional editor windows |
| `lazygit` | Optional git TUI windows |
| GitHub CLI (`gh`) | PR operations |
| `direnv` | Directory environment setup |

## Getting Started

```bash
git clone <repo-url> && cd agman
./release.sh
export PATH="$HOME/.agman/bin:$PATH"
agman
```

## Configuration

State lives in `~/.agman/`.

```toml
repos_dir = "~/repos/"
harness = "claude" # claude, codex, goose, or pi
```

New agent state is stored under `~/.agman/agents`.

## Core CLI

```bash
agman create-project myproj --description "UI rewrite"
agman create-pm-task myproj myrepo fix-bug --first-prompt "Fix the login bug"
agman create-agent --kind reviewer --name pr-1247 --project myproj --first-prompt "Review the PR"
agman send-message engineer:myproj--engineer-myrepo-fix-bug "Please create the PR"
agman send-message reviewer:myproj--pr-1247 "Please re-check the latest commit"
agman status
```

`create-pm-task --first-prompt` accepts inline text, `@file`, or `-` for stdin. Omitting it still creates the task, worktree, and attached Engineer, but leaves the Engineer idle with no initial inbox message.
`create-agent --first-prompt` behaves the same for project-scoped Researcher, Operator, Reviewer, and Tester agents. Omitting it creates and starts an idle agent with no initial inbox message until you use `agman send-message`.

## Harness Notes

Set `harness = "claude"`, `"codex"`, `"goose"`, or `"pi"` in config or via TUI settings. Long-lived agents stamp their launch harness in their state directory so existing conversations resume with the same runtime.

## Tech Stack

Rust with ratatui, git worktrees, tmux sessions, runtime-selectable agent CLIs, and GitHub CLI integration.
