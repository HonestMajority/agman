use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;

pub struct Git;

/// Parse `(owner, repo)` from a GitHub remote URL.
/// Handles both HTTPS (`https://github.com/owner/repo.git`) and
/// SSH (`git@github.com:owner/repo.git`) formats.
pub fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    let trimmed = remote_url.trim();

    // Try HTTPS: https://github.com/owner/repo.git
    if let Some(rest) = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
    {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // Try SSH: git@github.com:owner/repo.git
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

impl Git {
    /// Get the origin remote URL for a repository.
    pub fn get_remote_url(repo_path: &PathBuf) -> Result<String> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["remote", "get-url", "origin"])
            .output()
            .context("Failed to get remote URL")?;

        if !output.status.success() {
            anyhow::bail!(
                "No origin remote found: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if a remote exists
    fn has_remote(repo_path: &PathBuf, remote_name: &str) -> bool {
        Command::new("git")
            .current_dir(repo_path)
            .args(["remote", "get-url", remote_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Check if a ref exists (branch, tag, or remote ref)
    fn ref_exists(repo_path: &PathBuf, ref_name: &str) -> bool {
        Command::new("git")
            .current_dir(repo_path)
            .args(["rev-parse", "--verify", ref_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Fetch origin for a repository (returns Ok even if no remote)
    pub fn fetch_origin(repo_path: &PathBuf) -> Result<bool> {
        if !Self::has_remote(repo_path, "origin") {
            return Ok(false);
        }

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["fetch", "origin"])
            .output()
            .context("Failed to execute git fetch")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(repo = %repo_path.display(), error = %err, "failed to fetch origin");
            return Ok(false);
        }

        Ok(true)
    }

    /// Find the best base ref for creating a new branch
    /// Tries in order: origin/main, origin/master, main, master, HEAD
    fn find_base_ref(repo_path: &PathBuf) -> String {
        let candidates = [
            "origin/main",
            "origin/master",
            "refs/heads/main",
            "refs/heads/master",
            "HEAD",
        ];

        for candidate in candidates {
            if Self::ref_exists(repo_path, candidate) {
                return candidate.to_string();
            }
        }

        // Fallback to HEAD (should always exist in a valid repo)
        "HEAD".to_string()
    }

    /// Create a new worktree with a new branch
    /// Tries to base on origin/main if available, falls back to local main or HEAD
    pub fn create_worktree(config: &Config, repo_name: &str, branch_name: &str) -> Result<PathBuf> {
        Self::create_worktree_impl(config, repo_name, branch_name, false)
    }

    /// Create a new worktree with a new branch (quiet mode for TUI)
    pub fn create_worktree_quiet(
        config: &Config,
        repo_name: &str,
        branch_name: &str,
    ) -> Result<PathBuf> {
        Self::create_worktree_impl(config, repo_name, branch_name, true)
    }

    fn create_worktree_impl(
        config: &Config,
        repo_name: &str,
        branch_name: &str,
        quiet: bool,
    ) -> Result<PathBuf> {
        tracing::info!(repo = repo_name, branch = branch_name, "creating worktree");
        let repo_path = config.repo_path(repo_name);

        if !repo_path.exists() {
            anyhow::bail!("Repository does not exist: {}", repo_path.display());
        }

        // Verify it's a git repo
        if !repo_path.join(".git").exists() && !Self::ref_exists(&repo_path, "HEAD") {
            anyhow::bail!("Not a git repository: {}", repo_path.display());
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

        // Try to fetch origin (non-fatal if no remote)
        if !quiet {
            print!("  Fetching origin... ");
        }
        if Self::fetch_origin(&repo_path)? {
            if !quiet {
                println!("done");
            }
        } else if !quiet {
            println!("skipped (no remote)");
        }

        // Find the best base ref
        let base_ref = Self::find_base_ref(&repo_path);
        if !quiet {
            println!(
                "  Creating worktree with new branch '{}' based on {}...",
                branch_name, base_ref
            );
        }

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args([
                "worktree",
                "add",
                "-b",
                branch_name,
                worktree_path.to_str().unwrap(),
                &base_ref,
            ])
            .output()
            .context("Failed to create worktree")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to create worktree: {}", err);
        }

        tracing::info!(path = %worktree_path.display(), "worktree created");
        Ok(worktree_path)
    }

    /// Create a worktree for an existing remote branch (quiet mode for TUI)
    pub fn create_worktree_for_existing_branch_quiet(
        config: &Config,
        repo_name: &str,
        branch_name: &str,
    ) -> Result<PathBuf> {
        Self::create_worktree_for_existing_branch_impl(config, repo_name, branch_name, true)
    }

    fn create_worktree_for_existing_branch_impl(
        config: &Config,
        repo_name: &str,
        branch_name: &str,
        quiet: bool,
    ) -> Result<PathBuf> {
        let repo_path = config.repo_path(repo_name);

        if !repo_path.exists() {
            anyhow::bail!("Repository does not exist: {}", repo_path.display());
        }

        if !repo_path.join(".git").exists() && !Self::ref_exists(&repo_path, "HEAD") {
            anyhow::bail!("Not a git repository: {}", repo_path.display());
        }

        let worktree_base = config.worktree_base(repo_name);
        let worktree_path = config.worktree_path(repo_name, branch_name);

        if worktree_path.exists() {
            anyhow::bail!(
                "Worktree already exists: {}\nIf it's stale, remove with: git -C {:?} worktree remove {:?}",
                worktree_path.display(),
                repo_path,
                worktree_path
            );
        }

        std::fs::create_dir_all(&worktree_base)
            .context("Failed to create worktree base directory")?;

        // Fetch origin
        if !quiet {
            print!("  Fetching origin... ");
        }
        if Self::fetch_origin(&repo_path)? {
            if !quiet {
                println!("done");
            }
        } else if !quiet {
            println!("skipped (no remote)");
        }

        // Check if branch exists locally or only on remote
        let local_exists = Self::ref_exists(&repo_path, &format!("refs/heads/{}", branch_name));
        let remote_exists = Self::ref_exists(&repo_path, &format!("refs/remotes/origin/{}", branch_name));

        if !quiet {
            println!(
                "  Creating worktree for existing branch '{}'...",
                branch_name
            );
        }

        let output = if local_exists {
            // Branch exists locally, just check it out into the worktree
            Command::new("git")
                .current_dir(&repo_path)
                .args([
                    "worktree",
                    "add",
                    worktree_path.to_str().unwrap(),
                    branch_name,
                ])
                .output()
                .context("Failed to create worktree")?
        } else if remote_exists {
            // Branch only on remote — create local tracking branch
            Command::new("git")
                .current_dir(&repo_path)
                .args([
                    "worktree",
                    "add",
                    "-b",
                    branch_name,
                    worktree_path.to_str().unwrap(),
                    &format!("origin/{}", branch_name),
                ])
                .output()
                .context("Failed to create worktree")?
        } else {
            anyhow::bail!(
                "Branch '{}' not found locally or on origin",
                branch_name
            );
        };

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Pull latest changes
        if !quiet {
            print!("  Pulling latest changes... ");
        }
        let pull_output = Command::new("git")
            .current_dir(&worktree_path)
            .args(["pull", "--ff-only"])
            .output();

        match pull_output {
            Ok(o) if o.status.success() => {
                if !quiet {
                    println!("done");
                }
            }
            _ => {
                if !quiet {
                    println!("skipped (no tracking branch or conflicts)");
                }
            }
        }

        Ok(worktree_path)
    }

    /// Remove a worktree and prune stale references
    pub fn remove_worktree(repo_path: &PathBuf, worktree_path: &PathBuf) -> Result<()> {
        tracing::info!(worktree = %worktree_path.display(), "removing worktree");
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
            let err = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(worktree = %worktree_path.display(), error = %err, "failed to remove worktree");
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
        tracing::debug!(branch = branch_name, "deleting branch");
        // Delete main branch
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["branch", "-D", branch_name])
            .output()
            .context("Failed to delete branch")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(branch = branch_name, error = %err, "failed to delete branch");
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
