use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MemorySource {
    Manual,
    Agent,
    GlobalPromotion,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MemoryScope {
    Agent,
    Project,
    Global,
    /// The WHO×WHERE cell that crosses agent identity with project identity:
    /// this agent's own learnings about this specific repo. Neither `Agent`
    /// (leaks across every repo the agent touches) nor `Project` (visible to
    /// every other agent working in the repo) represents it correctly.
    ///
    /// Reserved so the storage key shape exists ahead of need — see
    /// `resolve_scope_context` in `ao-engine-tools-engine/src/memory/store.rs`
    /// for how its key is derived. No write path constructs this variant yet;
    /// it exists purely so a future writer does not require a keying
    /// retrofit. Back-compat: this is a plain additional unit variant, so
    /// every existing persisted `Agent` / `Project` / `Global` value
    /// continues to (de)serialize exactly as before.
    AgentProject,
    /// Ephemeral, per-thread working memory: entries keyed by the current
    /// thread id, gone once the thread ends. The odd one out among these
    /// four cells — every other variant is durable and only ever widens
    /// visibility (agent → project → global); `Thread` is narrow and
    /// throwaway by design, a scratch tier for content that is useful right
    /// now but not yet worth a durable write. See `resolve_scope_context` in
    /// `ao-engine-tools-engine/src/memory/store.rs` for how the thread id is
    /// resolved (and why resolution fails outright, rather than silently
    /// falling back to another scope, when no thread id is available).
    Thread,
}

impl Default for MemoryScope {
    fn default() -> Self {
        MemoryScope::Agent
    }
}

fn default_updated_at() -> DateTime<Utc> {
    DateTime::UNIX_EPOCH
}

fn default_confidence() -> f32 {
    1.0
}

fn default_decay_score() -> f32 {
    1.0
}

/// Lifecycle state of a memory entry, distinct from the tombstone (`deleted_at`).
/// A `Superseded` or `Archived` entry stays on disk for provenance but is no
/// longer surfaced as live guidance.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Superseded,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub source: Option<MemorySource>,
    #[serde(default)]
    pub scope: MemoryScope,
    #[serde(default)]
    pub scope_key: Option<String>,
    #[serde(default = "default_updated_at")]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub deleted_at: Option<DateTime<Utc>>,
    /// How much the store trusts this entry. Defaults to full confidence for
    /// every existing write path; consumed by the eviction scorer.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Lifecycle state. Defaults to `Active` so rows written before this key
    /// existed read as live, matching their current behavior.
    // TODO(memory-supersede): set to `Superseded` instead of hard-editing
    // when a contradiction guard fires.
    #[serde(default)]
    pub status: MemoryStatus,
    /// Id of the entry that superseded this one, if `status == Superseded`.
    // TODO(memory-supersede): populate when the contradiction guard
    // supersedes an entry.
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// Set by the review queue's `pin` action: a human explicitly asked
    /// this entry to be protected from the eviction sweep, regardless of
    /// its `source`/`confidence`/usage score. Mirrors the existing
    /// `MemorySource::Manual` eviction exemption (see
    /// `ao_engine_tools_engine::memory::eviction::select_eviction_candidate`)
    /// without requiring the entry to actually be user-authored — an
    /// agent-authored entry a human has reviewed and pinned is exempt too.
    /// `#[serde(default)]` so every earlier row reads as unpinned.
    #[serde(default)]
    pub pinned: bool,
    /// Slow-moving relevance score maintained by the periodic decay
    /// sweep (`ao_engine_tools_engine::memory::decay::decay_sweep`) — the one
    /// field on this struct that deliberately mutates outside a read, rather
    /// than living in the `.usage.json` sidecar
    /// (`ao_engine_tools_core::memory_usage`). It changes at most once per
    /// sweep run, not once per surface-and-use, so persisting it inline
    /// never turns into a per-read rewrite of the JSONL. Distinct from
    /// `confidence` (a static trust rating set at write time) and from the
    /// sidecar's raw `use_count`/`last_used` counters (which the sweep reads
    /// to compute a boost but never stores here directly). Read by the
    /// eviction scorer (`eviction::eviction_score`); `#[serde(default)]` so
    /// every earlier row reads as full strength.
    #[serde(default = "default_decay_score")]
    pub decay_score: f32,
}
