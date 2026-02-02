use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub struct Git;

impl Git {
    pub fn get_repo_root() -> Result<PathBuf> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("Failed to execute git command")?;

        if !output.status.success() {
            anyhow::bail!(
                "Not in a git repository: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let path = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_string();
        Ok(PathBuf::from(path))
    }

    pub fn create_worktree(branch_name: &str, base_path: &PathBuf) -> Result<PathBuf> {
        let repo_root = Self::get_repo_root()?;
        let worktree_path = base_path.join(branch_name);

        // First, check if branch exists
        let branch_exists = Command::new("git")
            .current_dir(&repo_root)
            .args(["rev-parse", "--verify", branch_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if branch_exists {
            // Use existing branch
            let output = Command::new("git")
                .current_dir(&repo_root)
                .args([
                    "worktree",
                    "add",
                    worktree_path.to_str().unwrap(),
                    branch_name,
                ])
                .output()
                .context("Failed to create worktree")?;

            if !output.status.success() {
                anyhow::bail!(
                    "Failed to create worktree: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } else {
            // Create new branch
            let output = Command::new("git")
                .current_dir(&repo_root)
                .args([
                    "worktree",
                    "add",
                    "-b",
                    branch_name,
                    worktree_path.to_str().unwrap(),
                ])
                .output()
                .context("Failed to create worktree with new branch")?;

            if !output.status.success() {
                anyhow::bail!(
                    "Failed to create worktree: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        Ok(worktree_path)
    }

    pub fn remove_worktree(worktree_path: &PathBuf) -> Result<()> {
        let repo_root = Self::get_repo_root()?;

        let output = Command::new("git")
            .current_dir(&repo_root)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_path.to_str().unwrap(),
            ])
            .output()
            .context("Failed to remove worktree")?;

        if !output.status.success() {
            // Try to prune instead
            let _ = Command::new("git")
                .current_dir(&repo_root)
                .args(["worktree", "prune"])
                .output();
        }

        Ok(())
    }

    pub fn delete_branch(branch_name: &str) -> Result<()> {
        let repo_root = Self::get_repo_root()?;

        let output = Command::new("git")
            .current_dir(&repo_root)
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

        Ok(())
    }

    #[allow(dead_code)]
    pub fn list_worktrees() -> Result<Vec<(String, PathBuf)>> {
        let repo_root = Self::get_repo_root()?;

        let output = Command::new("git")
            .current_dir(&repo_root)
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
