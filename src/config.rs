use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::harness::{Harness, HarnessKind};

/// Replace `/` with `-` in branch names so task directories stay flat.
/// The real branch name is preserved in `meta.json`; the task ID is just a
/// filesystem-safe lookup key.
fn sanitize_branch_for_id(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Sanitize a branch name for use in tmux session names.
/// Replaces characters that tmux interprets as target syntax separators:
/// `.` (pane separator), `:` (window separator), `/` (path separator).
fn sanitize_for_tmux(branch: &str) -> String {
    branch.replace('/', "-").replace(['.', ':'], "_")
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_dir: PathBuf,
    pub tasks_dir: PathBuf,
    pub prompts_dir: PathBuf,
    pub repos_dir: PathBuf,
    pub notes_dir: PathBuf,
}

/// On-disk config file (~/.agman/config.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    pub repos_dir: Option<String>,
    pub archive_retention_days: Option<u64>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    /// Which agent harness to use for newly-spawned agents. `"claude"`,
    /// `"codex"`, `"goose"`, or `"pi"`. Defaults to `"claude"` when absent.
    pub harness: Option<String>,
}

/// Read `<base_dir>/config.toml`, returning defaults if missing or unparseable.
pub fn load_config_file(base_dir: &Path) -> ConfigFile {
    let path = base_dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => match toml::from_str::<ConfigFile>(&contents) {
            Ok(cf) => cf,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse config.toml, using defaults");
                ConfigFile::default()
            }
        },
        Err(_) => ConfigFile::default(),
    }
}

/// Write a `ConfigFile` to `<base_dir>/config.toml`.
pub fn save_config_file(base_dir: &Path, config_file: &ConfigFile) -> Result<()> {
    let path = base_dir.join("config.toml");
    let contents =
        toml::to_string_pretty(config_file).context("failed to serialize config.toml")?;
    std::fs::write(&path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

impl Config {
    pub fn new(base_dir: PathBuf, repos_dir: PathBuf) -> Self {
        let tasks_dir = base_dir.join("tasks");
        let prompts_dir = base_dir.join("prompts");
        let notes_dir = base_dir.join("notes");

        Self {
            base_dir,
            tasks_dir,
            prompts_dir,
            repos_dir,
            notes_dir,
        }
    }

    pub fn load() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Could not find home directory")?;
        let base_dir = home_dir.join(".agman");

        let config_file = load_config_file(&base_dir);
        let repos_dir = match config_file.repos_dir {
            Some(ref path) => PathBuf::from(path),
            None => home_dir.join("repos"),
        };

        let config = Self::new(base_dir, repos_dir);
        tracing::debug!(base_dir = %config.base_dir.display(), repos_dir = %config.repos_dir.display(), "config loaded");
        Ok(config)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        // Run any legacy CEO → Chief of Staff migrations BEFORE creating
        // the new directory layout. Idempotent and best-effort: see
        // `crate::migration` for details.
        std::fs::create_dir_all(&self.base_dir)
            .with_context(|| format!("Failed to create {}", self.base_dir.display()))?;
        if let Err(e) = crate::migration::run(self) {
            tracing::warn!(error = %e, "migration step failed; continuing");
        }

        std::fs::create_dir_all(&self.tasks_dir).context("Failed to create tasks directory")?;
        std::fs::create_dir_all(&self.prompts_dir).context("Failed to create prompts directory")?;
        std::fs::create_dir_all(&self.notes_dir).context("Failed to create notes directory")?;
        std::fs::create_dir_all(self.agents_dir()).context("Failed to create agents directory")?;
        Ok(())
    }

    /// Get task directory: ~/.agman/tasks/<repo>--<branch>/
    pub fn task_dir(&self, repo_name: &str, branch_name: &str) -> PathBuf {
        self.tasks_dir.join(Self::task_id(repo_name, branch_name))
    }

    pub fn task_dir_from_id(&self, task_id: &str) -> PathBuf {
        self.tasks_dir.join(task_id)
    }

    /// Get task inbox path: ~/.agman/tasks/<id>/inbox.jsonl
    pub fn task_inbox(&self, task_id: &str) -> PathBuf {
        self.tasks_dir.join(task_id).join("inbox.jsonl")
    }

    /// Get task inbox seq path: ~/.agman/tasks/<id>/inbox.seq
    pub fn task_inbox_seq(&self, task_id: &str) -> PathBuf {
        self.tasks_dir.join(task_id).join("inbox.seq")
    }

    /// Get task ID from repo and branch names.
    /// Sanitizes `/` in branch names to `-` so the task directory is always flat.
    pub fn task_id(repo_name: &str, branch_name: &str) -> String {
        format!("{}--{}", repo_name, sanitize_branch_for_id(branch_name))
    }

    /// Parse task ID into (repo_name, branch_name)
    pub fn parse_task_id(task_id: &str) -> Option<(String, String)> {
        let parts: Vec<&str> = task_id.splitn(2, "--").collect();
        if parts.len() == 2 {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    }

    /// Get main repo path: ~/repos/<repo>/
    pub fn repo_path(&self, repo_name: &str) -> PathBuf {
        self.repos_dir.join(repo_name)
    }

    /// Get main repo path, using `parent_dir` as base when provided (repos outside `repos_dir`).
    /// Falls back to `self.repo_path()` when `parent_dir` is `None`.
    pub fn repo_path_for(&self, parent_dir: Option<&Path>, repo_name: &str) -> PathBuf {
        match parent_dir {
            Some(parent) => parent.join(repo_name),
            None => self.repo_path(repo_name),
        }
    }

    /// Get worktree base path: ~/repos/<repo>-wt/
    pub fn worktree_base(&self, repo_name: &str) -> PathBuf {
        self.repos_dir.join(format!("{}-wt", repo_name))
    }

    /// Get worktree base path, using `parent_dir` as base when provided (repos outside `repos_dir`).
    pub fn worktree_base_for(&self, parent_dir: Option<&Path>, repo_name: &str) -> PathBuf {
        match parent_dir {
            Some(parent) => parent.join(format!("{}-wt", repo_name)),
            None => self.worktree_base(repo_name),
        }
    }

    /// Get worktree path: ~/repos/<repo>-wt/<branch>/
    /// Sanitizes `/` in branch names to `-` so the worktree directory is flat.
    pub fn worktree_path(&self, repo_name: &str, branch_name: &str) -> PathBuf {
        self.worktree_base(repo_name)
            .join(sanitize_branch_for_id(branch_name))
    }

    /// Get worktree path, using `parent_dir` as base when provided (repos outside `repos_dir`).
    pub fn worktree_path_for(
        &self,
        parent_dir: Option<&Path>,
        repo_name: &str,
        branch_name: &str,
    ) -> PathBuf {
        self.worktree_base_for(parent_dir, repo_name)
            .join(sanitize_branch_for_id(branch_name))
    }

    /// Get tmux session name: (<repo>)__<branch>
    /// Sanitizes tmux-special characters in branch names: `/` → `-`, `.` → `_`, `:` → `_`.
    pub fn tmux_session_name(repo_name: &str, branch_name: &str) -> String {
        format!("({})__{}", repo_name, sanitize_for_tmux(branch_name))
    }

    pub fn prompt_path(&self, agent_name: &str) -> PathBuf {
        self.prompts_dir.join(format!("{}.md", agent_name))
    }

    pub fn repo_stats_path(&self) -> PathBuf {
        self.base_dir.join("repo_stats.json")
    }

    pub fn dismissed_notifications_path(&self) -> PathBuf {
        self.base_dir.join("dismissed_notifications.json")
    }

    /// Resolve the configured harness kind. Falls back to `Claude` when the
    /// `harness` config key is absent or unparseable.
    pub fn harness_kind(&self) -> HarnessKind {
        let cf = load_config_file(&self.base_dir);
        cf.harness
            .as_deref()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(HarnessKind::Claude)
    }

    /// Return the configured harness as a trait object. Used at spawn sites
    /// for newly-launched long-lived agents.
    pub fn default_harness(&self) -> Box<dyn Harness> {
        self.harness_kind().select()
    }

    // --- Chief of Staff & Project paths ---

    pub fn chief_of_staff_dir(&self) -> PathBuf {
        self.base_dir.join("chief-of-staff")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.base_dir.join("projects")
    }

    pub fn project_dir(&self, name: &str) -> PathBuf {
        self.projects_dir().join(name)
    }

    pub fn chief_of_staff_inbox(&self) -> PathBuf {
        self.chief_of_staff_dir().join("inbox.jsonl")
    }

    pub fn chief_of_staff_seq(&self) -> PathBuf {
        self.chief_of_staff_dir().join("inbox.seq")
    }

    /// Pinned claude session UUID for the Chief of Staff. Written on first
    /// launch and re-read on subsequent launches so `--resume <uuid>` lands
    /// the user directly back in the prior conversation.
    pub fn chief_of_staff_session_id(&self) -> PathBuf {
        self.chief_of_staff_dir().join("session-id")
    }

    pub fn project_inbox(&self, name: &str) -> PathBuf {
        self.project_dir(name).join("inbox.jsonl")
    }

    pub fn project_seq(&self, name: &str) -> PathBuf {
        self.project_dir(name).join("inbox.seq")
    }

    /// Pinned claude session UUID for a project's PM agent.
    pub fn project_session_id(&self, name: &str) -> PathBuf {
        self.project_dir(name).join("session-id")
    }

    /// Stamped working directory for a long-lived codex/goose/pi session,
    /// captured on first launch. Reused on resume so the harness restarts
    /// from the original generation cwd.
    pub fn launch_cwd_path(state_dir: &Path) -> PathBuf {
        state_dir.join("launch-cwd")
    }

    pub fn chief_of_staff_tmux_session() -> &'static str {
        "agman-chief-of-staff"
    }

    pub fn pm_tmux_session(name: &str) -> String {
        format!("agman-pm-{name}")
    }

    pub fn telegram_dir(&self) -> PathBuf {
        self.base_dir.join("telegram")
    }

    pub fn telegram_outbox(&self) -> PathBuf {
        self.telegram_dir().join("outbox.jsonl")
    }

    pub fn telegram_outbox_seq(&self) -> PathBuf {
        self.telegram_dir().join("outbox.seq")
    }

    pub fn telegram_dead_letter(&self) -> PathBuf {
        self.telegram_dir().join("dead-letter.jsonl")
    }

    pub fn telegram_panic_log(&self) -> PathBuf {
        self.telegram_dir().join("last-panic.log")
    }

    pub fn telegram_current_agent_path(&self) -> PathBuf {
        self.telegram_dir().join("current-agent")
    }

    pub fn whisper_model_path(&self) -> PathBuf {
        self.base_dir.join("whisper").join("ggml-base.bin")
    }

    // --- Agent paths ---
    //
    // Agents share the on-disk layout under
    // `~/.agman/agents/<project>--<name>/`. The kind discriminator lives
    // inside `meta.json`. Tmux session names diverge by kind so a researcher
    // and reviewer with the same name+project don't collide on resume.

    pub fn agents_dir(&self) -> PathBuf {
        self.base_dir.join("agents")
    }

    pub fn agent_dir(&self, project: &str, name: &str) -> PathBuf {
        self.agents_dir().join(format!("{project}--{name}"))
    }

    pub fn agent_inbox(&self, project: &str, name: &str) -> PathBuf {
        self.agent_dir(project, name).join("inbox.jsonl")
    }

    pub fn agent_seq(&self, project: &str, name: &str) -> PathBuf {
        self.agent_dir(project, name).join("inbox.seq")
    }

    /// Pinned harness session UUID for an agent.
    pub fn agent_session_id(&self, project: &str, name: &str) -> PathBuf {
        self.agent_dir(project, name).join("session-id")
    }

    /// Tmux session name for a researcher. Preserved as-is so existing
    /// researcher sessions resume after the agent rename.
    pub fn researcher_tmux_session(project: &str, name: &str) -> String {
        format!("agman-researcher-{project}--{name}")
    }

    /// Tmux session name for an operator.
    pub fn operator_tmux_session(project: &str, name: &str) -> String {
        format!("agman-operator-{project}--{name}")
    }

    /// Tmux session name for a reviewer.
    pub fn reviewer_tmux_session(project: &str, name: &str) -> String {
        format!("agman-reviewer-{project}--{name}")
    }

    /// Tmux session name for a tester.
    pub fn tester_tmux_session(project: &str, name: &str) -> String {
        format!("agman-tester-{project}--{name}")
    }

    /// Tmux session name for a task-attached engineer.
    pub fn engineer_tmux_session(project: &str, name: &str) -> String {
        format!("agman-engineer-{project}--{name}")
    }

    // --- Project template paths ---

    /// Directory where project templates are stored: ~/.agman/project-templates/
    pub fn templates_dir(&self) -> PathBuf {
        self.base_dir.join("project-templates")
    }

    /// Path for a single template: ~/.agman/project-templates/<name>.md
    pub fn template_path(&self, name: &str) -> PathBuf {
        self.templates_dir().join(format!("{name}.md"))
    }

    pub fn init_default_files(&self, force: bool) -> Result<()> {
        self.ensure_dirs()?;

        let prompts = [("engineer", ENGINEER_PROMPT)];

        for (name, content) in prompts {
            let path = self.prompt_path(name);
            if force || !path.exists() {
                std::fs::write(&path, content)?;
            }
        }

        Ok(())
    }
}

const ENGINEER_PROMPT: &str = r#"You are a long-lived task-attached engineer agent.

You own one agman task at a time. Work from PM inbox messages, keep state across the session, and handle implementation, tests, commits, rebases, pushes, pull requests, CI monitoring, and review-addressing when the PM asks.

Report progress, blockers, and completion back to the PM with `agman send-message`. Ask only when genuinely blocked.
"#;
