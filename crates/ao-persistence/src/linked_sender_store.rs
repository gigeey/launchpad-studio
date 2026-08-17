//! Durable per-binding sender allow-list persistence — the server-authoritative
//! home for `ChannelBinding::allowed_senders`, which used to live inline on
//! the whole `AgentProfile` document and could be clobbered by any writer
//! that round-trips that whole document (see that field's doc comment).
//!
//! Mirrors [`crate::channel_cursor_store::ChannelCursorStore`]'s shape: one
//! small JSON file per `(agent_id, binding_id)` binding under the data root,
//! written with the tmp-then-rename pattern so a reader never observes a
//! partial write.

use std::path::Path;

use ao_protocol::error::AoError;
use ao_protocol::linked_sender_list::LinkedSenderList;

use crate::paths::DataRoot;

/// On-disk store for one [`LinkedSenderList`] per `(agent_id, binding_id)`.
pub struct LinkedSenderStore {
    data_root: DataRoot,
}

impl LinkedSenderStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Loads the persisted sender list for `(agent_id, binding_id)`. Returns
    /// `Ok(None)` when nothing has been persisted yet — a fresh binding, or
    /// one whose `allowed_senders` still only lives on the inline
    /// `ChannelBinding` field because it predates this store. Callers that
    /// need the one-time inline backfill should use [`Self::get_or_backfill`]
    /// instead of calling this directly.
    pub async fn get(&self, agent_id: &str, binding_id: &str) -> Result<Option<LinkedSenderList>, AoError> {
        let path = self.data_root.linked_sender_path(agent_id, binding_id);
        Self::read(&path).await
    }

    /// Atomically persists `list` for `(agent_id, binding_id)`, overwriting
    /// any prior value.
    pub async fn set(
        &self,
        agent_id: &str,
        binding_id: &str,
        list: &LinkedSenderList,
    ) -> Result<(), AoError> {
        let path = self.data_root.linked_sender_path(agent_id, binding_id);
        Self::write(&path, list).await
    }

    /// Loads the authoritative sender list for `(agent_id, binding_id)`,
    /// migrating `inline` into the store the first time this binding is read
    /// (see `ChannelBinding::allowed_senders`'s doc for why `inline` still
    /// exists at all). Once the store holds a value for this binding —
    /// whether from this backfill or from a prior [`Self::set`]/
    /// [`Self::add_sender`] — `inline` is never consulted again, so a stale
    /// or empty value passed here on a later call can't undo an already
    /// migrated allow-list.
    pub async fn get_or_backfill(
        &self,
        agent_id: &str,
        binding_id: &str,
        inline: &[String],
    ) -> Result<Vec<String>, AoError> {
        if let Some(existing) = self.get(agent_id, binding_id).await? {
            return Ok(existing.senders);
        }
        let inline = inline.to_vec();
        if !inline.is_empty() {
            self.set(agent_id, binding_id, &LinkedSenderList { senders: inline.clone() }).await?;
        }
        Ok(inline)
    }

    /// Adds `sender` to `(agent_id, binding_id)`'s allow-list, deduping
    /// against whatever is already there. Read-modify-write: safe against
    /// concurrent unrelated writers only in the same sense `set` is (last
    /// writer wins on this one file), but never touches the rest of the
    /// agent's profile document the way the old inline field's writer did.
    pub async fn add_sender(&self, agent_id: &str, binding_id: &str, sender: &str) -> Result<(), AoError> {
        let mut list = self.get(agent_id, binding_id).await?.unwrap_or_default();
        if !list.senders.iter().any(|s| s == sender) {
            list.senders.push(sender.to_string());
        }
        self.set(agent_id, binding_id, &list).await
    }

    /// Removes `sender` from `(agent_id, binding_id)`'s allow-list, if
    /// present. A no-op (not an error) when the store has nothing persisted
    /// yet, or when `sender` isn't on the list.
    pub async fn remove_sender(&self, agent_id: &str, binding_id: &str, sender: &str) -> Result<(), AoError> {
        let Some(mut list) = self.get(agent_id, binding_id).await? else {
            return Ok(());
        };
        list.senders.retain(|s| s != sender);
        self.set(agent_id, binding_id, &list).await
    }

    /// Revokes every sender for `(agent_id, binding_id)` — for when the whole
    /// binding is being torn down (e.g. its secret/token is deleted), so a
    /// previously authorized sender doesn't stay authorized if the binding is
    /// later reconfigured with a fresh secret.
    pub async fn clear(&self, agent_id: &str, binding_id: &str) -> Result<(), AoError> {
        self.set(agent_id, binding_id, &LinkedSenderList::default()).await
    }

    async fn read(path: &Path) -> Result<Option<LinkedSenderList>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        let list: LinkedSenderList =
            serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(list))
    }

    async fn write(path: &Path, list: &LinkedSenderList) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(list).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("linked_sender"),
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

    fn store_at(dir: &Path) -> LinkedSenderStore {
        LinkedSenderStore::new(DataRoot::new(dir))
    }

    fn list(senders: &[&str]) -> LinkedSenderList {
        LinkedSenderList { senders: senders.iter().map(|s| s.to_string()).collect() }
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-a", "telegram", &list(&["555"])).await.unwrap();
        let loaded = store.get("agent-a", "telegram").await.unwrap();

        assert_eq!(loaded, Some(list(&["555"])));
    }

    #[tokio::test]
    async fn set_overwrites_a_prior_value_for_the_same_binding() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-a", "telegram", &list(&["555"])).await.unwrap();
        store.set("agent-a", "telegram", &list(&["666"])).await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["666"])));
    }

    #[tokio::test]
    async fn add_sender_creates_a_fresh_list_when_none_exists() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.add_sender("agent-a", "telegram", "555").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["555"])));
    }

    #[tokio::test]
    async fn add_sender_dedupes_against_an_existing_entry() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.add_sender("agent-a", "telegram", "555").await.unwrap();
        store.add_sender("agent-a", "telegram", "555").await.unwrap();
        store.add_sender("agent-a", "telegram", "666").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["555", "666"])));
    }

    #[tokio::test]
    async fn remove_sender_drops_a_linked_sender() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-a", "telegram", &list(&["555", "666"])).await.unwrap();
        store.remove_sender("agent-a", "telegram", "555").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["666"])));
    }

    #[tokio::test]
    async fn remove_sender_on_an_empty_store_is_a_harmless_no_op() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.remove_sender("agent-a", "telegram", "555").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn different_bindings_on_the_same_agent_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-a", "telegram", &list(&["555"])).await.unwrap();
        store.set("agent-a", "email", &list(&["boss@example.com"])).await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["555"])));
        assert_eq!(store.get("agent-a", "email").await.unwrap(), Some(list(&["boss@example.com"])));
    }

    #[tokio::test]
    async fn clear_revokes_every_sender_for_a_binding() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.set("agent-a", "telegram", &list(&["111", "222"])).await.unwrap();
        store.clear("agent-a", "telegram").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&[])));
    }

    #[tokio::test]
    async fn clear_on_an_empty_store_is_a_harmless_no_op() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.clear("agent-a", "telegram").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&[])));
    }

    #[tokio::test]
    async fn different_agents_with_the_same_binding_id_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.add_sender("agent-a", "telegram", "555").await.unwrap();
        store.add_sender("agent-b", "telegram", "666").await.unwrap();

        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), Some(list(&["555"])));
        assert_eq!(store.get("agent-b", "telegram").await.unwrap(), Some(list(&["666"])));
    }

    #[tokio::test]
    async fn get_or_backfill_returns_stored_value_without_consulting_inline() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        store.set("agent-a", "telegram", &list(&["555"])).await.unwrap();

        let senders = store
            .get_or_backfill("agent-a", "telegram", &["999".to_string()])
            .await
            .unwrap();

        assert_eq!(senders, vec!["555".to_string()], "a populated store must win over a stale inline value");
    }

    #[tokio::test]
    async fn get_or_backfill_migrates_a_nonempty_inline_value_into_the_store() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        let senders = store
            .get_or_backfill("agent-a", "telegram", &["555".to_string()])
            .await
            .unwrap();

        assert_eq!(senders, vec!["555".to_string()]);
        assert_eq!(
            store.get("agent-a", "telegram").await.unwrap(),
            Some(list(&["555"])),
            "the inline value must be persisted into the store on first read"
        );
    }

    #[tokio::test]
    async fn get_or_backfill_leaves_the_store_untouched_when_inline_is_empty() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        let senders = store.get_or_backfill("agent-a", "telegram", &[]).await.unwrap();

        assert!(senders.is_empty());
        assert_eq!(store.get("agent-a", "telegram").await.unwrap(), None);
    }
}
