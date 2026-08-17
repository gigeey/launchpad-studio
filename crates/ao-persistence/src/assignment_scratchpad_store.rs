//! Durable per-assignment dedup scratchpad persistence.
//!
//! The agent-driven watch detection tier re-polls its own agent on a plain
//! instruction rather than reading a deterministic event feed, so it needs
//! somewhere reliable to record what it has already acted on. That record
//! has to be code-owned and deterministic — never thread memory, which is
//! model-mediated, FIFO-cap-evicted, and would otherwise tax every future
//! poll's prompt. [`AssignmentScratchpadStore`] gives each assignment id a
//! small on-disk home for that state, read at poll time and rewritten only
//! by an explicit [`AssignmentScratchpadStore::set`] call — callers decide
//! when to commit, which lets the fire-then-persist ordering (act on the
//! result, then persist the cursor, so a crash between the two re-fires
//! rather than silently drops) live entirely in the caller.

use std::path::Path;
use std::time::SystemTime;

use ao_protocol::assignment_scratchpad::AssignmentScratchpad;
use ao_protocol::error::AoError;

#[cfg(test)]
use ao_protocol::assignment_scratchpad::ExtractionPath;

use crate::paths::DataRoot;

/// On-disk store for one [`AssignmentScratchpad`] per assignment id.
pub struct AssignmentScratchpadStore {
    data_root: DataRoot,
}

impl AssignmentScratchpadStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Loads the persisted scratchpad for `assignment_id`. Returns
    /// `Ok(None)` when nothing has been persisted yet — a fresh assignment,
    /// or one whose watch has never polled — which callers treat as "no
    /// items seen yet."
    pub async fn get(&self, assignment_id: &str) -> Result<Option<AssignmentScratchpad>, AoError> {
        let path = self.data_root.assignment_scratchpad_path(assignment_id);
        Self::read(&path).await
    }

    /// Atomically persists `scratchpad` for `assignment_id`, overwriting any
    /// prior value. Uses the write-to-.tmp-then-rename pattern (as
    /// `task_meta::write_task_meta` does) so a reader never observes a
    /// partially-written file.
    pub async fn set(
        &self,
        assignment_id: &str,
        scratchpad: &AssignmentScratchpad,
    ) -> Result<(), AoError> {
        let path = self.data_root.assignment_scratchpad_path(assignment_id);
        Self::write(&path, scratchpad).await
    }

    /// True if a push delivery with `delivery_id` was already recorded for
    /// `assignment_id` within the TTL window
    /// (`ao_protocol::assignment_scratchpad::DELIVERY_ID_TTL`). Purely
    /// additive to the poll-cursor `get`/`set` pair above — reads through
    /// the same scratchpad row, does not touch `last_seen_id`/`seen_ids`.
    pub async fn has_seen_delivery(&self, assignment_id: &str, delivery_id: &str) -> Result<bool, AoError> {
        let scratchpad = self.get(assignment_id).await?.unwrap_or_default();
        Ok(scratchpad.has_seen_delivery(delivery_id, SystemTime::now()))
    }

    /// Records `delivery_id` as seen for `assignment_id`, evicting any
    /// delivery-ids past the TTL in the same write. Callers fire the
    /// action first and call this after, so a crash between the two
    /// re-fires rather than silently drops (same fire-then-persist
    /// ordering the poll cursor relies on).
    pub async fn record_delivery(&self, assignment_id: &str, delivery_id: &str) -> Result<(), AoError> {
        let mut scratchpad = self.get(assignment_id).await?.unwrap_or_default();
        scratchpad.record_delivery(delivery_id, SystemTime::now());
        self.set(assignment_id, &scratchpad).await
    }

    async fn read(path: &Path) -> Result<Option<AssignmentScratchpad>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        let scratchpad: AssignmentScratchpad =
            serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(scratchpad))
    }

    async fn write(path: &Path, scratchpad: &AssignmentScratchpad) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json =
            serde_json::to_string_pretty(scratchpad).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("assignment_scratchpad"),
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
    use ao_protocol::assignment_scratchpad::{delivery_key, ItemSnapshot, SeenDelivery};
    use tempfile::tempdir;

    fn store_at(dir: &Path) -> AssignmentScratchpadStore {
        AssignmentScratchpadStore::new(DataRoot::new(dir))
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("assignment-a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn scratchpad_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let scratchpad = AssignmentScratchpad {
            last_seen_id: Some("item-101".to_string()),
            seen_ids: vec!["item-99".to_string(), "item-100".to_string(), "item-101".to_string()],
            seen_deliveries: vec![],
            ..Default::default()
        };

        store.set("assignment-a", &scratchpad).await.unwrap();
        let loaded = store.get("assignment-a").await.unwrap();

        assert_eq!(loaded, Some(scratchpad));
    }

    #[tokio::test]
    async fn set_overwrites_a_prior_scratchpad_for_the_same_assignment() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store
            .set(
                "assignment-b",
                &AssignmentScratchpad { last_seen_id: Some("item-1".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() },
            )
            .await
            .unwrap();
        store
            .set(
                "assignment-b",
                &AssignmentScratchpad { last_seen_id: Some("item-2".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() },
            )
            .await
            .unwrap();

        let loaded = store.get("assignment-b").await.unwrap();
        assert_eq!(
            loaded,
            Some(AssignmentScratchpad { last_seen_id: Some("item-2".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() })
        );
    }

    #[tokio::test]
    async fn different_assignments_are_isolated() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store
            .set(
                "assignment-c",
                &AssignmentScratchpad { last_seen_id: Some("c-item".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() },
            )
            .await
            .unwrap();
        store
            .set(
                "assignment-d",
                &AssignmentScratchpad { last_seen_id: Some("d-item".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() },
            )
            .await
            .unwrap();

        assert_eq!(
            store.get("assignment-c").await.unwrap(),
            Some(AssignmentScratchpad { last_seen_id: Some("c-item".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() })
        );
        assert_eq!(
            store.get("assignment-d").await.unwrap(),
            Some(AssignmentScratchpad { last_seen_id: Some("d-item".to_string()), seen_ids: vec![], seen_deliveries: vec![], ..Default::default() })
        );
    }

    #[tokio::test]
    async fn scratchpad_persists_across_a_fresh_store_instance() {
        let tmp = tempdir().unwrap();
        let scratchpad = AssignmentScratchpad {
            last_seen_id: Some("item-7".to_string()),
            seen_ids: vec!["item-6".to_string(), "item-7".to_string()],
            seen_deliveries: vec![],
            ..Default::default()
        };

        {
            let store = store_at(tmp.path());
            store.set("assignment-e", &scratchpad).await.unwrap();
        }

        let store2 = store_at(tmp.path());
        let loaded = store2.get("assignment-e").await.unwrap();
        assert_eq!(loaded, Some(scratchpad));
    }

    #[tokio::test]
    async fn has_seen_delivery_is_false_before_any_delivery_is_recorded() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert!(!store.has_seen_delivery("assignment-f", "delivery-1").await.unwrap());
    }

    #[tokio::test]
    async fn record_delivery_then_has_seen_delivery_persists_across_store_instances() {
        let tmp = tempdir().unwrap();
        {
            let store = store_at(tmp.path());
            store.record_delivery("assignment-f", "delivery-1").await.unwrap();
        }

        let store2 = store_at(tmp.path());
        assert!(store2.has_seen_delivery("assignment-f", "delivery-1").await.unwrap());
        assert!(!store2.has_seen_delivery("assignment-f", "delivery-2").await.unwrap());
    }

    #[tokio::test]
    async fn delivery_dedup_is_additive_and_does_not_disturb_the_poll_cursor() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store
            .set(
                "assignment-g",
                &AssignmentScratchpad {
                    last_seen_id: Some("cursor-42".to_string()),
                    seen_ids: vec!["a".to_string(), "b".to_string()],
                    seen_deliveries: vec![],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        store.record_delivery("assignment-g", "delivery-1").await.unwrap();

        let loaded = store.get("assignment-g").await.unwrap().unwrap();
        assert_eq!(loaded.last_seen_id.as_deref(), Some("cursor-42"), "poll cursor get/set semantics are untouched");
        assert_eq!(loaded.seen_ids, vec!["a".to_string(), "b".to_string()]);
        assert!(store.has_seen_delivery("assignment-g", "delivery-1").await.unwrap());
    }

    #[tokio::test]
    async fn different_assignments_have_isolated_delivery_dedup() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());

        store.record_delivery("assignment-h", "delivery-1").await.unwrap();

        assert!(store.has_seen_delivery("assignment-h", "delivery-1").await.unwrap());
        assert!(!store.has_seen_delivery("assignment-i", "delivery-1").await.unwrap());
    }

    #[tokio::test]
    async fn scratchpad_with_snapshots_and_contract_fingerprint_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let scratchpad = AssignmentScratchpad {
            last_seen_id: None,
            seen_ids: vec![],
            seen_deliveries: vec![SeenDelivery {
                id: delivery_key("assignment-j", "identity-1", "version-1", 1),
                seen_at: SystemTime::now(),
                permanent: true,
                status: ao_protocol::assignment_scratchpad::DeliveryStatus::Confirmed,
                run_id: None,
                identity_key: None,
                stale_notified: false,
                pending_poll_count: 0,
            }],
            snapshots: vec![ItemSnapshot {
                identity_key: "identity-1".to_string(),
                version_key: "version-1".to_string(),
                predicate_value: true,
                edge_counter: 1,
                last_seen_at: "2026-07-27T09:00:00Z".to_string(),
                payload: serde_json::json!({ "tag": "Very Important" }),
            }],
            contract_fingerprint: Some("fingerprint-123".to_string()),
            identity_keygen_version: Some(2),
            missing_required_field_streak: 0,
            truncation_notified: false,
            authoring_failure_streak: 0,
            contract_amendment_cycle_count: 0,
            authoring_input_fingerprint: None,
            last_extraction_path: ExtractionPath::Llm,
            last_inferred_tier: None,
            extraction_plan: None,
            extraction_plan_fingerprint: None,
            extraction_plan_degraded: false,
            extraction_plan_degraded_reason: None,
            consecutive_polls_without_new_items: 0,
            last_new_item_at: None,
            model_calls_by_day: Default::default(),
            ..Default::default()
        };

        store.set("assignment-j", &scratchpad).await.unwrap();
        let loaded = store.get("assignment-j").await.unwrap();

        assert_eq!(loaded, Some(scratchpad));
    }
}
