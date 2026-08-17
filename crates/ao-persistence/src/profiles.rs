use std::path::{Path, PathBuf};

use ao_protocol::agent::AgentProfile;
use ao_protocol::agent_home::ensure_agent_home;
use ao_protocol::error::AoError;
use uuid::Uuid;

use crate::paths::DataRoot;

/// Result of cloning an agent's home directory.
///
/// Communicates to the caller whether a new directory was provisioned on disk
/// (and therefore needs to be rolled back if subsequent orchestration steps
/// fail) or whether the clone simply reuses the parent's custom path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClonedHome {
    /// A fresh default home directory was created at this path and the parent's
    /// home contents were recursively copied into it. Rollback = remove this dir.
    NewDefault(PathBuf),
    /// The parent uses a custom home path; the clone shares the same path and
    /// nothing was written to disk. Rollback is a no-op for this variant.
    SharedCustom(PathBuf),
}

impl ClonedHome {
    /// Resolved home directory path for the cloned agent.
    pub fn path(&self) -> &Path {
        match self {
            ClonedHome::NewDefault(p) | ClonedHome::SharedCustom(p) => p,
        }
    }
}

/// YAML-based agent profile CRUD store.
pub struct AgentProfileStore {
    data_root: DataRoot,
}

impl AgentProfileStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// The data root this store resolves agent profile files against.
    ///
    /// Exposed so callers that only hold the store (not the full
    /// `PersistenceLayer`) can still resolve agent-relative paths, e.g.
    /// scaffolding an agent's home directory right after `create`.
    pub fn data_root(&self) -> &DataRoot {
        &self.data_root
    }

    /// Validate that an agent ID contains only alphanumeric characters, hyphens, and underscores.
    fn validate_id(id: &str) -> Result<(), AoError> {
        if id.is_empty() {
            return Err(AoError::Internal("Agent ID cannot be empty".into()));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AoError::Internal(format!(
                "Agent ID '{}' contains invalid characters. Only alphanumeric, hyphens, and underscores are allowed.",
                id
            )));
        }
        Ok(())
    }

    fn profile_path(&self, id: &str) -> std::path::PathBuf {
        self.data_root.agents_dir().join(format!("{}.yaml", id))
    }

    /// Create a new agent profile. Fails if the agent already exists.
    pub async fn create(&self, profile: &AgentProfile) -> Result<(), AoError> {
        Self::validate_id(&profile.id)?;
        let path = self.profile_path(&profile.id);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(AoError::AgentAlreadyExists(profile.id.clone()));
        }
        let yaml =
            serde_yaml::to_string(profile).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }

    /// Get an agent profile by ID. Returns None if not found.
    pub async fn get(&self, id: &str) -> Result<Option<AgentProfile>, AoError> {
        let path = self.profile_path(id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let profile: AgentProfile =
            serde_yaml::from_str(&contents).map_err(|e| AoError::Yaml(e.to_string()))?;
        Ok(Some(profile))
    }

    /// List all agent profiles from the agents directory.
    pub async fn list(&self) -> Result<Vec<AgentProfile>, AoError> {
        let agents_dir = self.data_root.agents_dir();
        if !tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut profiles = Vec::new();
        let mut entries = tokio::fs::read_dir(&agents_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                let contents = tokio::fs::read_to_string(&path).await?;
                match serde_yaml::from_str::<AgentProfile>(&contents) {
                    Ok(profile) => profiles.push(profile),
                    Err(e) => {
                        tracing::warn!("Failed to parse agent profile {:?}: {}", path, e);
                    }
                }
            }
        }
        Ok(profiles)
    }

    /// Update an existing agent profile. Fails if the agent does not exist.
    pub async fn update(&self, profile: &AgentProfile) -> Result<(), AoError> {
        Self::validate_id(&profile.id)?;
        let path = self.profile_path(&profile.id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(AoError::AgentNotFound(profile.id.clone()));
        }
        let yaml =
            serde_yaml::to_string(profile).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }

    /// Delete an agent profile. Returns false if not found.
    pub async fn delete(&self, id: &str) -> Result<bool, AoError> {
        let path = self.profile_path(id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }
        tokio::fs::remove_file(&path).await?;
        Ok(true)
    }

    /// Clone an existing agent profile, persisting a new row with a fresh UUID
    /// id and name `"<Parent Name> - copy"`. Most fields are copied verbatim
    /// from the parent, but `channels` (Telegram/Discord/Slack/Email/Webhook
    /// bindings) is deliberately reset to empty rather than copied.
    ///
    /// A channel binding is a live external listening surface: a Slack
    /// binding points at a shared workspace-level `connection_id`, a
    /// Telegram/Discord/Email binding points at a specific bridge thread and
    /// allow-list. If we copied `channels` verbatim, the clone would come up
    /// already wired to the exact same Slack connection / Telegram bot /
    /// inbox as the parent — two independent bridges would then both handle
    /// every inbound message on that connector, double-firing responses. The
    /// clone must start with no channels and have them configured explicitly.
    /// The parent row is left unmodified. Returns the new agent's id and the
    /// persisted profile.
    pub async fn clone_agent_profile(
        &self,
        parent_id: &str,
    ) -> Result<(String, AgentProfile), AoError> {
        let parent = self
            .get(parent_id)
            .await?
            .ok_or_else(|| AoError::AgentNotFound(parent_id.to_string()))?;

        let mut clone = parent.clone();
        clone.id = Uuid::new_v4().to_string();
        clone.name = format!("{} - copy", parent.name);
        clone.emoji = Some(random_agent_emoji().to_string());
        clone.channels = Vec::new();

        self.create(&clone).await?;
        Ok((clone.id.clone(), clone))
    }

    /// Clone an agent's home directory contents into a home for the new agent,
    /// following the rules from the CloneAgent PRD:
    ///
    /// - If the parent's effective home is the managed default path for its id
    ///   (i.e. `home_dir` is `None`, or points to
    ///   `data_root.agent_home_dir(parent.id)`), provision the default home for
    ///   `new_agent_id` and recursively copy the parent's home into it so the
    ///   clone gets its own isolated skills/rules/memory/etc.
    /// - If the parent has a custom home path, reuse it unchanged — the clone
    ///   shares the parent's home directory as a single source of truth.
    ///
    /// Returns a [`ClonedHome`] describing what happened so an orchestrator can
    /// roll back the newly-created default directory on failure.
    pub async fn clone_agent_home(
        &self,
        parent: &AgentProfile,
        new_agent_id: &str,
    ) -> Result<ClonedHome, AoError> {
        let parent_default = self.data_root.agent_home_dir(&parent.id);
        let parent_home: PathBuf = parent
            .home_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| parent_default.clone());

        if parent_home != parent_default {
            return Ok(ClonedHome::SharedCustom(parent_home));
        }

        let new_home = self.data_root.agent_home_dir(new_agent_id);
        ensure_agent_home(&new_home).await?;

        if tokio::fs::try_exists(&parent_home).await.unwrap_or(false) {
            copy_dir_recursive(parent_home, new_home.clone()).await?;
        }

        Ok(ClonedHome::NewDefault(new_home))
    }

    /// Atomically clone an agent: duplicate the profile row and the home
    /// directory (per [`clone_agent_home`] semantics), or roll back cleanly on
    /// failure so no orphaned state remains.
    ///
    /// On success the returned [`AgentProfile`] has `home_dir` set to `None`
    /// for default-home parents (so the clone continues to resolve against
    /// the managed default directory), or to the parent's custom path for
    /// custom-home parents. The persisted YAML row is updated to match, so
    /// subsequent loads see the same value.
    ///
    /// On failure of [`clone_agent_home`], the newly-created profile row is
    /// deleted and any default home directory created under
    /// `data_root.agent_home_dir(new_id)` is removed. The parent is untouched
    /// in all cases.
    pub async fn clone_agent(&self, parent_id: &str) -> Result<AgentProfile, AoError> {
        let parent = self
            .get(parent_id)
            .await?
            .ok_or_else(|| AoError::AgentNotFound(parent_id.to_string()))?;

        let (new_id, mut clone) = self.clone_agent_profile(parent_id).await?;

        let cloned_home = match self.clone_agent_home(&parent, &new_id).await {
            Ok(h) => h,
            Err(e) => {
                let _ = self.delete(&new_id).await;
                let _ =
                    tokio::fs::remove_dir_all(self.data_root.agent_home_dir(&new_id)).await;
                return Err(e);
            }
        };

        clone.home_dir = match &cloned_home {
            ClonedHome::NewDefault(_) => None,
            ClonedHome::SharedCustom(p) => Some(p.to_string_lossy().into_owned()),
        };
        self.update(&clone).await?;

        Ok(clone)
    }
}

/// Recursively copy the contents of `src` into `dst`, creating `dst` (and any
/// nested subdirectories) as needed. Runs blocking filesystem operations on a
/// blocking thread so the async runtime is not stalled by large copies.
async fn copy_dir_recursive(src: PathBuf, dst: PathBuf) -> Result<(), AoError> {
    tokio::task::spawn_blocking(move || copy_dir_recursive_sync(&src, &dst))
        .await
        .map_err(|e| AoError::Internal(format!("join error while copying agent home: {}", e)))?
        .map_err(AoError::Io)
}

const AGENT_EMOJI_POOL: &[&str] = &[
    "🤖", "🦾", "🧠", "🛸", "🚀", "✨", "🌟", "⚡", "🔮", "🎯",
    "🦊", "🦉", "🦁", "🐼", "🐙", "🦄", "🐝", "🦋", "🐬", "🦅",
    "🧙", "🧑‍🚀", "🧑‍💻", "🕵️", "🦸", "🧚", "🧞", "🥷",
    "📚", "🧭", "🔧", "🧰", "🧩", "🎨", "🎼", "📡",
];

fn random_agent_emoji() -> &'static str {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    AGENT_EMOJI_POOL[nanos % AGENT_EMOJI_POOL.len()]
}

fn copy_dir_recursive_sync(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_sync(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
