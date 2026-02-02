use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;

pub struct Git;

impl Git {
    /// Fetch origin for a repository
    pub fn fetch_origin(repo_path: &PathBuf) -> Result<()> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["fetch", "origin"])
            .output()
            .context("Failed to execute git fetch")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to fetch origin: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Create a new worktree with a new branch based on origin/main
    /// This matches tlana's behavior: git worktree add -b <branch> <wt_dir> origin/main
    pub fn create_worktree(
        config: &Config,
        repo_name: &str,
        branch_name: &str,
    ) -> Result<PathBuf> {
        let repo_path = config.repo_path(repo_name);

        if !repo_path.exists() {
            anyhow::bail!("Repository does not exist: {}", repo_path.display());
        }

        let worktree_base = config.worktree_base(repo_name);
        let worktree_path = config.worktree_path(repo_name, branch_name);

        // Check if worktree already exists
        if worktree_path.exists() {
            anyhow::bail!(
                "Worktree already exists: {}\nIf it's stale, remove with: git -C {:?} worktree remove {:?}",
                worktree_path.display(),
                repo_path,
                worktree_path
            );
        }

        // Create worktree base directory if needed
        std::fs::create_dir_all(&worktree_base)
            .context("Failed to create worktree base directory")?;

        // Fetch origin first
        println!("  Fetching origin...");
        Self::fetch_origin(&repo_path)?;

        // Create worktree with new branch based on origin/main
        println!("  Creating worktree with new branch '{}' based on origin/main...", branch_name);
        let output = Command::new("git")
            .current_dir(&repo_path)
            .args([
                "worktree",
                "add",
                "-b",
                branch_name,
                worktree_path.to_str().unwrap(),
                "origin/main",
            ])
            .output()
            .context("Failed to create worktree")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(worktree_path)
    }

    /// Remove a worktree and prune stale references
    pub fn remove_worktree(repo_path: &PathBuf, worktree_path: &PathBuf) -> Result<()> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_path.to_str().unwrap(),
            ])
            .output()
            .context("Failed to remove worktree")?;

        if !output.status.success() {
            tracing::warn!(
                "Failed to remove worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Prune stale worktree references
        let _ = Command::new("git")
            .current_dir(repo_path)
            .args(["worktree", "prune"])
            .output();

        Ok(())
    }

    /// Delete a local branch (and any backup branches)
    pub fn delete_branch(repo_path: &PathBuf, branch_name: &str) -> Result<()> {
        // Delete main branch
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["branch", "-D", branch_name])
            .output()
            .context("Failed to delete branch")?;

        if !output.status.success() {
            tracing::warn!(
                "Failed to delete branch {}: {}",
                branch_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Delete backup branches (like tlana does)
        let backup_output = Command::new("git")
            .current_dir(repo_path)
            .args(["branch", "--list", &format!("{}-BACKUP-*", branch_name)])
            .output();

        if let Ok(output) = backup_output {
            let backups = String::from_utf8_lossy(&output.stdout);
            for backup in backups.lines() {
                let backup = backup.trim().trim_start_matches("* ");
                if !backup.is_empty() {
                    let _ = Command::new("git")
                        .current_dir(repo_path)
                        .args(["branch", "-D", backup])
                        .output();
                }
            }
        }

        Ok(())
    }

    /// Run direnv allow in a directory
    pub fn direnv_allow(path: &PathBuf) -> Result<()> {
        let output = Command::new("direnv")
            .args(["allow", path.to_str().unwrap()])
            .output();

        match output {
            Ok(o) if o.status.success() => Ok(()),
            Ok(_) => {
                // direnv might not be available or configured, that's ok
                tracing::debug!("direnv allow failed (this is ok if direnv is not used)");
                Ok(())
            }
            Err(_) => {
                // direnv not installed, that's ok
                tracing::debug!("direnv not found (this is ok if direnv is not used)");
                Ok(())
            }
        }
    }

    #[allow(dead_code)]
    pub fn list_worktrees(repo_path: &PathBuf) -> Result<Vec<(String, PathBuf)>> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .context("Failed to list worktrees")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to list worktrees: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut worktrees = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;

        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(path));
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                current_branch = Some(branch.to_string());
            } else if line.is_empty() {
                if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                    worktrees.push((branch, path));
                }
                current_path = None;
                current_branch = None;
            }
        }

        // Handle last entry
        if let (Some(path), Some(branch)) = (current_path, current_branch) {
            worktrees.push((branch, path));
        }

        Ok(worktrees)
    }
}
