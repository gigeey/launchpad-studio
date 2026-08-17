//! Durable Slack workspace connection record persistence.
//!
//! Mirrors [`crate::channel_cursor_store::ChannelCursorStore`]'s shape
//! exactly: one plain JSON file per key, written with the
//! write-to-`.tmp`-then-rename pattern so a reader never observes a partial
//! write. The key here is an opaque `connection_id` rather than
//! `(agent_id, binding_id)`, because the whole point of this store is that a
//! connection's lifetime is independent of any one binding — see the module
//! doc on [`ao_protocol::slack_connection::SlackConnection`].
//!
//! Not a secret store: [`SlackConnection`] holds identity only (`team_id`,
//! `team_name`, `bot_user_id`), never the bot or app token, so — like
//! `ChannelCursorStore` — this never touches the OS keychain.

use std::path::Path;

use ao_protocol::error::AoError;
use ao_protocol::slack_connection::SlackConnection;

use crate::paths::DataRoot;

/// On-disk store for one [`SlackConnection`] per opaque `connection_id`.
pub struct SlackConnectionStore {
    data_root: DataRoot,
}

impl SlackConnectionStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Loads the persisted connection record for `connection_id`. Returns
    /// `Ok(None)` when nothing has been persisted yet — an unprovisioned or
    /// unknown connection id.
    pub async fn get(&self, connection_id: &str) -> Result<Option<SlackConnection>, AoError> {
        let path = self.data_root.slack_connection_path(connection_id);
        Self::read(&path).await
    }

    /// Atomically persists `connection` for `connection_id`, overwriting any
    /// prior value. This is the only write path — a binding's
    /// `connection_id` field merely references a record written here, so
    /// deleting a binding never touches this store.
    pub async fn set(&self, connection_id: &str, connection: &SlackConnection) -> Result<(), AoError> {
        let path = self.data_root.slack_connection_path(connection_id);
        Self::write(&path, connection).await
    }

    /// Removes the connection record for `connection_id`. No-op if absent.
    /// A distinct operation from deleting a binding — a binding only ever
    /// stops *pointing at* a connection; something else (workspace-level
    /// disconnect UI, later phase) decides when the record itself goes away.
    pub async fn delete(&self, connection_id: &str) -> Result<(), AoError> {
        let path = self.data_root.slack_connection_path(connection_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn read(path: &Path) -> Result<Option<SlackConnection>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        let connection: SlackConnection =
            serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(connection))
    }

    async fn write(path: &Path, connection: &SlackConnection) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json =
            serde_json::to_string_pretty(connection).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("slack_connection"),
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

    fn store_at(dir: &Path) -> SlackConnectionStore {
        SlackConnectionStore::new(DataRoot::new(dir))
    }

    fn sample() -> SlackConnection {
        SlackConnection {
            team_id: "T0123ABCD".to_string(),
            team_name: "Acme Corp".to_string(),
            bot_user_id: "U0456WXYZ".to_string(),
        }
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("conn-a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn connection_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let connection = sample();

        store.set("conn-a", &connection).await.unwrap();
        let loaded = store.get("conn-a").await.unwrap();

        assert_eq!(loaded, Some(connection));
    }

    #[tokio::test]
    async fn set_overwrites_a_prior_record_for_the_same_connection_id() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("conn-a", &sample()).await.unwrap();
        let renamed = SlackConnection { team_name: "Acme Corp Renamed".to_string(), ..sample() };
        store.set("conn-a", &renamed).await.unwrap();

        let loaded = store.get("conn-a").await.unwrap();
        assert_eq!(loaded, Some(renamed));
    }

    #[tokio::test]
    async fn different_connection_ids_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        let conn_a = sample();
        let conn_b = SlackConnection {
            team_id: "T9999ZZZZ".to_string(),
            team_name: "Other Workspace".to_string(),
            bot_user_id: "U9999YYYY".to_string(),
        };

        store.set("conn-a", &conn_a).await.unwrap();
        store.set("conn-b", &conn_b).await.unwrap();

        assert_eq!(store.get("conn-a").await.unwrap(), Some(conn_a));
        assert_eq!(store.get("conn-b").await.unwrap(), Some(conn_b));
    }

    /// Proves the reason this store exists as its own key space rather than
    /// living on a binding: deleting whatever *would* reference a connection
    /// is a separate operation from deleting the connection record itself.
    /// `delete` here stands in for that — nothing about removing a binding
    /// (which this store doesn't know about at all) calls it implicitly.
    #[tokio::test]
    async fn delete_removes_the_record_and_is_a_noop_when_absent() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("conn-a", &sample()).await.unwrap();
        store.delete("conn-a").await.unwrap();
        assert_eq!(store.get("conn-a").await.unwrap(), None);

        // Deleting again (or a connection id that never existed) must not error.
        store.delete("conn-a").await.unwrap();
        store.delete("never-existed").await.unwrap();
    }
}
