use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::agent_model::AgentRecord;
use crate::config::Config;
use crate::harness::shell_single_quote;
use crate::project::Project;
use crate::use_cases::{agent_kind_name, parse_send_target, SendTarget};

const AUTH_HINT: &str =
    "relaunch this sender through agman to supply its authenticated environment";

struct SenderIdentity {
    id: String,
    state_dir: PathBuf,
}

impl SenderIdentity {
    fn resolve(config: &Config, from: &str) -> Result<Self> {
        let state_dir = (|| {
            if matches!(from, "telegram" | "system" | "user" | "codex" | "unknown") {
                bail!("reserved or unsupported CLI sender");
            }
            if from.is_empty()
                || !from
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
            {
                bail!("expected an exact sender identity");
            }
            match parse_send_target(config, from)? {
                SendTarget::ChiefOfStaff => {
                    let dir = config.chief_of_staff_dir();
                    if !dir.is_dir() {
                        bail!("chief-of-staff has not been initialized");
                    }
                    Ok(dir)
                }
                SendTarget::Project(name) => {
                    let project = Project::load_by_name(config, &name)?;
                    if project.meta.name != from {
                        bail!("sender does not match project metadata");
                    }
                    Ok(project.dir)
                }
                SendTarget::AgentRecord { project, name, .. } => {
                    let agent = AgentRecord::load(config.agent_dir(&project, &name))?;
                    let actual = format!(
                        "{}:{}--{}",
                        agent_kind_name(&agent.meta.kind),
                        agent.meta.project,
                        agent.meta.name
                    );
                    if actual != from {
                        bail!("sender does not match agent metadata");
                    }
                    Ok(agent.dir)
                }
                SendTarget::Telegram => bail!("reserved CLI sender"),
            }
        })()
        .with_context(|| format!("invalid sender '{from}'"))?;
        Ok(Self {
            id: from.to_owned(),
            state_dir,
        })
    }

    fn token_path(&self) -> PathBuf {
        self.state_dir.join("sender-token")
    }
}

/// Proof of exact sender ownership, obtained only by checking the launch environment.
pub struct AuthenticatedSender {
    id: String,
}

impl AuthenticatedSender {
    pub fn from_env(config: &Config, from: &str) -> Result<Self> {
        let sender = SenderIdentity::resolve(config, from)?;
        let env_sender = std::env::var("AGMAN_SENDER").ok();
        let env_token = std::env::var("AGMAN_SENDER_TOKEN").ok();
        if env_sender.as_deref() != Some(from) {
            bail!("sender authentication failed for '{from}': AGMAN_SENDER must match --from; {AUTH_HINT}");
        }
        let expected = read_token(&sender.token_path())
            .with_context(|| format!("sender authentication failed for '{from}'; {AUTH_HINT}"))?;
        if env_token.as_deref() != Some(&expected) {
            bail!("sender authentication failed for '{from}': missing or incorrect credential; {AUTH_HINT}");
        }
        Ok(Self { id: sender.id })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

fn read_token(path: &Path) -> Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).context("cannot open sender-token")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("sender-token must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        if metadata.mode() & 0o777 != 0o600 || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("sender-token must be owned by the current user with permissions 0600");
        }
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .context("cannot read sender-token")?;
    if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid sender-token format");
    }
    Ok(token)
}

fn ensure_token(sender: &SenderIdentity) -> Result<()> {
    let path = sender.token_path();
    if !path.try_exists()? {
        let mut temp = tempfile::NamedTempFile::new_in(&sender.state_dir)?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        temp.write_all(token.as_bytes())?;
        temp.as_file().sync_all()?;
        // Publish a complete 0600 file without replacing another launch's token.
        if let Err(err) = temp.persist_noclobber(&path) {
            if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err.error).context("cannot create sender-token");
            }
        }
    }
    read_token(&path)?;
    Ok(())
}

/// Supply credentials to fresh and resumed harnesses without pasting secrets into tmux.
pub fn launch_command(config: &Config, from: &str, command: &str) -> Result<String> {
    let sender = SenderIdentity::resolve(config, from)?;
    ensure_token(&sender)?;
    let token_path = sender.token_path();
    let token_path = token_path
        .to_str()
        .context("sender-token path must be UTF-8")?;
    Ok(format!(
        "AGMAN_SENDER={} AGMAN_SENDER_TOKEN=\"$(cat {})\" {command}",
        shell_single_quote(&sender.id),
        shell_single_quote(token_path)
    ))
}
