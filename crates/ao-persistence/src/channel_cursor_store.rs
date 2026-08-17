//! Durable per-binding dedup cursor persistence.
//!
//! Each channel transport's inbound cursor (Telegram's `getUpdates` offset,
//! Discord's seen-message-id set + gateway session) used to live only in a
//! process-local variable, so a backend restart reset it to empty and the
//! channel re-served every message the previous process hadn't yet
//! acknowledged — the agent would answer an already-answered message again.
//! [`ChannelCursorStore`] gives each `(agent_id, binding_id)` binding a small
//! on-disk home for its cursor, restored by the transport when it starts and
//! rewritten as the cursor advances. Not a secret, so unlike
//! `ChannelSecretStore` this never touches the OS keychain — a plain JSON
//! file per binding under the data root is all it needs.

use std::path::Path;

use ao_protocol::channel_cursor::ChannelCursor;
use ao_protocol::error::AoError;

use crate::paths::DataRoot;

/// On-disk store for one [`ChannelCursor`] per `(agent_id, binding_id)`.
pub struct ChannelCursorStore {
    data_root: DataRoot,
}

impl ChannelCursorStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Loads the persisted cursor for `(agent_id, binding_id)`. Returns
    /// `Ok(None)` when nothing has been persisted yet — a fresh binding, or
    /// one created before this store existed — which callers treat as "start
    /// from scratch," the same behavior as before this store existed.
    pub async fn get(
        &self,
        agent_id: &str,
        binding_id: &str,
    ) -> Result<Option<ChannelCursor>, AoError> {
        let path = self.data_root.channel_cursor_path(agent_id, binding_id);
        Self::read(&path).await
    }

    /// Atomically persists `cursor` for `(agent_id, binding_id)`, overwriting
    /// any prior value. Uses the write-to-.tmp-then-rename pattern (as
    /// `task_meta::write_task_meta` does) so a reader never observes a
    /// partially-written file.
    pub async fn set(
        &self,
        agent_id: &str,
        binding_id: &str,
        cursor: &ChannelCursor,
    ) -> Result<(), AoError> {
        let path = self.data_root.channel_cursor_path(agent_id, binding_id);
        Self::write(&path, cursor).await
    }

    async fn read(path: &Path) -> Result<Option<ChannelCursor>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        let cursor: ChannelCursor =
            serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(cursor))
    }

    async fn write(path: &Path, cursor: &ChannelCursor) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(cursor).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("channel_cursor"),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store_at(dir: &Path) -> ChannelCursorStore {
        ChannelCursorStore::new(DataRoot::new(dir))
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn telegram_cursor_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let cursor = ChannelCursor::Telegram { offset: Some(101) };

        store.set("agent-a", "telegram", &cursor).await.unwrap();
        let loaded = store.get("agent-a", "telegram").await.unwrap();

        assert_eq!(loaded, Some(cursor));
    }

    #[tokio::test]
    async fn discord_cursor_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let cursor = ChannelCursor::Discord {
            seen_message_ids: vec!["10".to_string(), "11".to_string(), "12".to_string()],
            session_id: Some("session-xyz".to_string()),
            seq: Some(99),
        };

        store.set("agent-b", "discord", &cursor).await.unwrap();
        let loaded = store.get("agent-b", "discord").await.unwrap();

        assert_eq!(loaded, Some(cursor));
    }

    #[tokio::test]
    async fn set_overwrites_a_prior_cursor_for_the_same_binding() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-c", "telegram", &ChannelCursor::Telegram { offset: Some(1) }).await.unwrap();
        store.set("agent-c", "telegram", &ChannelCursor::Telegram { offset: Some(2) }).await.unwrap();

        let loaded = store.get("agent-c", "telegram").await.unwrap();
        assert_eq!(loaded, Some(ChannelCursor::Telegram { offset: Some(2) }));
    }

    #[tokio::test]
    async fn different_bindings_on_the_same_agent_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-d", "telegram", &ChannelCursor::Telegram { offset: Some(5) }).await.unwrap();
        store
            .set(
                "agent-d",
                "discord",
                &ChannelCursor::Discord { seen_message_ids: vec!["1".to_string()], session_id: None, seq: None },
            )
            .await
            .unwrap();

        assert_eq!(
            store.get("agent-d", "telegram").await.unwrap(),
            Some(ChannelCursor::Telegram { offset: Some(5) })
        );
        assert_eq!(
            store.get("agent-d", "discord").await.unwrap(),
            Some(ChannelCursor::Discord {
                seen_message_ids: vec!["1".to_string()],
                session_id: None,
                seq: None
            })
        );
    }

    #[tokio::test]
    async fn different_agents_with_the_same_binding_id_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-e", "telegram", &ChannelCursor::Telegram { offset: Some(7) }).await.unwrap();
        store.set("agent-f", "telegram", &ChannelCursor::Telegram { offset: Some(8) }).await.unwrap();

        assert_eq!(
            store.get("agent-e", "telegram").await.unwrap(),
            Some(ChannelCursor::Telegram { offset: Some(7) })
        );
        assert_eq!(
            store.get("agent-f", "telegram").await.unwrap(),
            Some(ChannelCursor::Telegram { offset: Some(8) })
        );
    }
}
