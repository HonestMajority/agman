use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A stored command that can be run on any task without user input.
/// Commands are specialized flows for common workflows like creating PRs
/// or addressing review comments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCommand {
    /// Display name (e.g., "Create Draft PR")
    pub name: String,
    /// Identifier used in CLI and file names (e.g., "create-pr")
    pub id: String,
    /// Description of what the command does
    pub description: String,
    /// If set, the command requires an argument of this type before running (e.g., "branch")
    #[serde(default)]
    pub requires_arg: Option<String>,
    /// Path to the YAML flow file (relative to commands dir, stored as absolute)
    #[serde(skip)]
    pub flow_path: PathBuf,
}

impl StoredCommand {
    /// Load a stored command from a YAML file
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read command file: {}", path.display()))?;
        let mut cmd: StoredCommand = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse command file: {}", path.display()))?;
        cmd.flow_path = path.to_path_buf();
        Ok(cmd)
    }

    /// List all available stored commands from the commands directory
    pub fn list_all(commands_dir: &Path) -> Result<Vec<StoredCommand>> {
        if !commands_dir.exists() {
            return Ok(Vec::new());
        }

        let mut commands = Vec::new();
        for entry in std::fs::read_dir(commands_dir)? {
            let entry = entry?;
            let path = entry.path();

            // Only load .yaml files
            if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                match Self::load(&path) {
                    Ok(cmd) => commands.push(cmd),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to load command");
                    }
                }
            }
        }

        // Sort by name for consistent ordering
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(commands)
    }

    /// Get a command by its ID
    pub fn get_by_id(commands_dir: &Path, id: &str) -> Result<Option<StoredCommand>> {
        let path = commands_dir.join(format!("{}.yaml", id));
        if path.exists() {
            Ok(Some(Self::load(&path)?))
        } else {
            Ok(None)
        }
    }
}
