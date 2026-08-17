//! Window anchor registry — runtime-only overlay for prompt-cache floor stability.
//!
//! # Overview
//!
//! The `WindowAnchorRegistry` partitions anchor state per [`AnchorKey`] scope so that
//! a Personal agent, a TeamShared coordinator, a TeamPerAgent child, and a Tasklist task
//! each maintain their own independent floor without leaking into one another.
//!
//! # Post-walk marker capture invariant
//!
//! The [`FloorMarker`] stored in a [`WindowAnchor`] is captured **after** the
//! pair-preservation walk has adjusted the slice start index, not before. Because
//! transcripts are append-only, re-locating the marker on the next turn is guaranteed
//! to be a no-op (the entry at the floor position is never overwritten), which is exactly
//! the CACHE HIT path in `history::select`.
//!
//! # `pinned_target` vs current count
//!
//! `max_window` is derived from `pinned_target` — the `compute_message_count` result at
//! the time the anchor was pinned — not from the current count. This prevents a
//! `compute_message_count` dip (e.g. the session crosses an hour boundary) from
//! immediately forcing a cache miss: the window is allowed to grow up to
//! `pinned_target * 2 + GRACE` before the floor rotates.
//!
//! # Runtime-only / no cross-restart persistence
//!
//! The registry lives in process memory only. On server restart the registry is empty and
//! the first turn pins a fresh anchor. One cache miss per restart is acceptable — it is
//! far better than the previous behaviour of missing on *every* turn.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::RwLock;

use ao_protocol::agent::AgentId;
use ao_protocol::team::TeamId;
use chrono::{DateTime, Utc};

use ao_protocol::transcript::TranscriptEntry;

/// Scope key that partitions anchor state — one independent floor per scope.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum AnchorKey {
    /// Standalone personal agent — keyed by agent ID.
    Personal(AgentId),
    /// Team coordinator shared transcript — keyed by team ID.
    TeamShared(TeamId),
    /// Team child per-agent transcript — keyed by (team ID, agent ID).
    TeamPerAgent(TeamId, AgentId),
    /// Tasklist task — keyed by filesystem path of the tasklist JSONL file.
    TasklistPath(PathBuf),
    /// Project channel — keyed by project ID.
    Project(String),
    /// A specific thread of an agent — keyed by the thread's transcript path.
    /// The path is unique per thread row, so different threads of the same
    /// agent never share anchor state. The agent_id is preserved so anchor
    /// state stays observable at the `(agent, thread)` granularity for logs.
    AgentThread(AgentId, PathBuf),
}

/// Stable marker identifying the floor entry within a transcript.
///
/// The hash is computed from `content + event_type` using [`DefaultHasher`],
/// which is deterministic within a single process. Cross-process stability
/// is NOT required — the registry is runtime-only.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FloorMarker {
    pub ts: DateTime<Utc>,
    pub content_hash: u64,
}

impl FloorMarker {
    /// Hash `content + event_type` into `content_hash`. Deterministic within a process.
    pub fn for_entry(entry: &TranscriptEntry) -> Self {
        let mut hasher = DefaultHasher::new();
        entry.content.hash(&mut hasher);
        entry.event_type.hash(&mut hasher);
        FloorMarker {
            ts: entry.ts,
            content_hash: hasher.finish(),
        }
    }
}

/// A pinned window anchor for one scope.
#[derive(Clone, Debug)]
pub struct WindowAnchor {
    /// Marker identifying the floor entry (captured post-walk).
    pub floor_marker: FloorMarker,
    /// `compute_message_count` result at pin time; drives `max_window` computation.
    pub pinned_target: usize,
    /// Wall-clock time when this anchor was last set.
    pub pinned_at: DateTime<Utc>,
}

/// Signal returned by [`WindowAnchorRegistry::set`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AnchorRotated {
    /// No prior anchor existed for this key — this is the first pin.
    Fresh,
    /// A prior anchor existed and has been overwritten (floor rotated).
    Rotated,
}

/// Runtime overlay that stores one [`WindowAnchor`] per [`AnchorKey`] scope.
///
/// Constructed empty; never persisted to disk. Both reads and writes are
/// protected by a single `std::sync::RwLock` (multiple concurrent readers /
/// exclusive writer).
pub struct WindowAnchorRegistry {
    inner: RwLock<HashMap<AnchorKey, WindowAnchor>>,
}

impl WindowAnchorRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self { inner: RwLock::new(HashMap::new()) }
    }

    /// Return a clone of the anchor for `key`, or `None` if not set.
    pub fn get(&self, key: &AnchorKey) -> Option<WindowAnchor> {
        self.inner.read().expect("anchor registry read lock").get(key).cloned()
    }

    /// Store `anchor` for `key`. Returns [`AnchorRotated::Fresh`] on first insert,
    /// [`AnchorRotated::Rotated`] when overwriting an existing anchor.
    pub fn set(&self, key: AnchorKey, anchor: WindowAnchor) -> AnchorRotated {
        let mut guard = self.inner.write().expect("anchor registry write lock");
        if guard.insert(key, anchor).is_some() {
            AnchorRotated::Rotated
        } else {
            AnchorRotated::Fresh
        }
    }

    /// Remove the anchor for `key`. No-op if not present.
    pub fn clear(&self, key: &AnchorKey) {
        self.inner.write().expect("anchor registry write lock").remove(key);
    }
}

impl Default for WindowAnchorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::transcript::TranscriptRole;
    use chrono::Utc;

    fn make_anchor(pinned_target: usize) -> WindowAnchor {
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: "test content".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        WindowAnchor {
            floor_marker: FloorMarker::for_entry(&entry),
            pinned_target,
            pinned_at: Utc::now(),
        }
    }

    #[test]
    fn set_then_get_round_trip() {
        let registry = WindowAnchorRegistry::new();
        let key = AnchorKey::Personal("agent-1".to_string());
        let anchor = make_anchor(20);
        let marker = anchor.floor_marker.clone();
        registry.set(key.clone(), anchor);
        let got = registry.get(&key).expect("anchor should be present");
        assert_eq!(got.floor_marker, marker);
        assert_eq!(got.pinned_target, 20);
    }

    #[test]
    fn set_overwrites_returns_rotated_signal() {
        let registry = WindowAnchorRegistry::new();
        let key = AnchorKey::TeamShared("team-1".to_string());
        let first = registry.set(key.clone(), make_anchor(20));
        let second = registry.set(key.clone(), make_anchor(20));
        assert_eq!(first, AnchorRotated::Fresh);
        assert_eq!(second, AnchorRotated::Rotated);
    }

    #[test]
    fn get_after_clear_returns_none() {
        let registry = WindowAnchorRegistry::new();
        let key = AnchorKey::Personal("agent-2".to_string());
        registry.set(key.clone(), make_anchor(20));
        registry.clear(&key);
        assert!(registry.get(&key).is_none());
    }

    #[tokio::test]
    async fn concurrent_sets_under_rwlock_do_not_corrupt() {
        use std::sync::Arc;
        let registry = Arc::new(WindowAnchorRegistry::new());
        let key = AnchorKey::Personal("concurrent-agent".to_string());
        let mut handles = Vec::new();
        for _ in 0..5 {
            let reg = Arc::clone(&registry);
            let k = key.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    reg.set(k.clone(), make_anchor(20));
                }
            }));
        }
        for h in handles {
            h.await.expect("task panicked");
        }
        // Final state must be a valid anchor (no corruption / no panic).
        let result = registry.get(&key);
        assert!(result.is_some(), "registry must have a value after concurrent sets");
    }

    #[test]
    fn floor_marker_equality_ignores_unrelated_fields() {
        let ts = Utc::now();
        let make = |hidden: bool| TranscriptEntry {
            ts,
            role: TranscriptRole::System("user".to_string()),
            content: "same content".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: hidden,
        };
        let marker_a = FloorMarker::for_entry(&make(false));
        let marker_b = FloorMarker::for_entry(&make(true));
        // Same ts + content + event_type → equal markers despite different hidden_from_user.
        assert_eq!(marker_a, marker_b);
    }
}
