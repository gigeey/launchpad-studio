use ao_protocol::error::AoError;
use ao_protocol::preferences::UserPreferences;

use crate::paths::DataRoot;

/// YAML-based user preferences store (single file, single user).
#[derive(Clone)]
pub struct UserPreferencesStore {
    data_root: DataRoot,
}

impl UserPreferencesStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Get user preferences. Returns None if the file doesn't exist (first launch).
    pub async fn get(&self) -> Result<Option<UserPreferences>, AoError> {
        let path = self.data_root.user_preferences_path();
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let prefs: UserPreferences =
            serde_yaml::from_str(&contents).map_err(|e| AoError::Yaml(e.to_string()))?;
        Ok(Some(prefs))
    }

    /// Save user preferences, overwriting any existing file.
    pub async fn save(&self, prefs: &UserPreferences) -> Result<(), AoError> {
        let path = self.data_root.user_preferences_path();
        let yaml = serde_yaml::to_string(prefs).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }
}
