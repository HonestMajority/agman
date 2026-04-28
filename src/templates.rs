//! Project templates — pre-written PM briefs for recurring orchestration patterns.
//!
//! Templates are plain Markdown files stored in `~/.agman/project-templates/<name>.md`.
//! No schema, no frontmatter. The filename (without `.md`) is the template name; the
//! first non-empty line of the body is the short description shown in
//! `agman list-templates`.
//!
//! The CEO workflow is:
//!   1. `agman list-templates` to discover available templates
//!   2. `agman get-template <name> > /tmp/brief.md` to copy the body to a scratch file
//!   3. Edit the scratch file to fit the instance
//!   4. `agman create-project <proj> --description "<label>" --initial-message @/tmp/brief.md`
//!
//! The stored template is never modified; each project gets a customized copy.

use anyhow::{bail, Context, Result};

use crate::config::Config;

/// A single template summary: name + first non-empty line of the body.
#[derive(Debug, Clone)]
pub struct TemplateSummary {
    pub name: String,
    pub description: String,
}

/// Validate template names: alphanumeric, hyphens, underscores.
/// Mirrors the project name rules to keep filenames safe.
fn validate_template_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("template name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "template name '{name}' is invalid: only alphanumeric characters, hyphens, and underscores are allowed"
        );
    }
    Ok(())
}

/// Extract the first non-empty trimmed line of a body, used as a short description.
fn first_nonempty_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// List all templates available in `~/.agman/project-templates/`.
/// Returns templates sorted by name. Missing directory yields an empty list.
pub fn list_templates(config: &Config) -> Result<Vec<TemplateSummary>> {
    let dir = config.templates_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read templates dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read template {}", path.display()))?;
        summaries.push(TemplateSummary {
            name: name.to_string(),
            description: first_nonempty_line(&body),
        });
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

/// Read a template's full body. Errors if the template does not exist.
pub fn read_template(config: &Config, name: &str) -> Result<String> {
    validate_template_name(name)?;
    let path = config.template_path(name);
    if !path.exists() {
        bail!("template '{name}' not found at {}", path.display());
    }
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read template {}", path.display()))
}

/// Write a template body. Creates the templates directory if it doesn't exist.
/// Overwrites an existing template with the same name.
pub fn write_template(config: &Config, name: &str, body: &str) -> Result<()> {
    validate_template_name(name)?;
    let dir = config.templates_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create templates dir {}", dir.display()))?;
    let path = config.template_path(name);
    std::fs::write(&path, body)
        .with_context(|| format!("failed to write template {}", path.display()))?;
    Ok(())
}
