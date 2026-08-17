pub mod artifact_group_store;
pub mod artifact_store;
pub mod assets;
pub mod assignment_run_store;
pub mod assignment_scratchpad_store;
pub mod assignment_store;
pub mod bookmarks;
pub mod changelog;
pub mod channel_cursor_store;
pub mod channel_lease_store;
pub mod conversation_registry_store;
pub mod cron_util;
pub mod linked_sender_store;
pub mod memory;
pub mod outcome;
pub mod paths;
pub mod preferences;
pub mod profiles;
pub mod progress_log;
pub mod project_key;
pub mod projects;
pub mod reflection_staging;
pub mod slack_connection_store;
pub mod slack_conversation_registry_store;
pub mod snapshot;
pub mod task_meta;
pub mod tasklist_store;
pub mod thread_store;
pub mod transcript;
pub mod workflow_store;

pub use project_key::{hash_project_key, resolve_project_key, update_projects_index};
pub use memory::MemoryOpResult;

use std::sync::Arc;

use ao_protocol::agent::ChannelKind;
use ao_protocol::assignment::AssignmentThreadPolicy;
use ao_protocol::error::AoError;
use ao_protocol::thread::{default_thread_id, AssignmentBridgeOrigin, ChannelBridgeOrigin};

use crate::artifact_group_store::ArtifactGroupStore;
use crate::artifact_store::ArtifactStore;
use crate::assets::AssetStore;
use crate::assignment_run_store::AssignmentRunStore;
use crate::assignment_scratchpad_store::AssignmentScratchpadStore;
use crate::assignment_store::AssignmentStore;
use crate::bookmarks::BookmarkStore;
use crate::changelog::ChangelogStore;
use crate::channel_cursor_store::ChannelCursorStore;
use crate::channel_lease_store::ChannelLeaseStore;
use crate::conversation_registry_store::ConversationRegistryStore;
use crate::linked_sender_store::LinkedSenderStore;
use crate::memory::MemoryStore;
use crate::outcome::OutcomeStore;
use crate::paths::DataRoot;
use crate::preferences::UserPreferencesStore;
use crate::profiles::AgentProfileStore;
use crate::reflection_staging::ReflectionStagingStore;
use crate::slack_connection_store::SlackConnectionStore;
use crate::slack_conversation_registry_store::SlackConversationRegistryStore;
use crate::snapshot::SnapshotStore;
use crate::projects::ProjectStore;
use crate::tasklist_store::TasklistStore;
use crate::thread_store::ThreadStore;
use crate::transcript::TranscriptStore;

/// Aggregates all persistence services.
pub struct PersistenceLayer {
    pub agents: AgentProfileStore,
    pub projects: ProjectStore,
    pub tasklists: TasklistStore,
    pub transcripts: TranscriptStore,
    pub changelogs: ChangelogStore,
    pub snapshots: Arc<SnapshotStore>,
    pub memory: Arc<MemoryStore>,
    pub bookmarks: BookmarkStore,
    /// Durable per-binding dedup cursor store (Telegram offset, Discord
    /// seen-ids + session) — see [`ChannelCursorStore`].
    pub channel_cursors: ChannelCursorStore,
    /// Single-writer lease store on `(agent_id, binding_id)` — see
    /// [`ChannelLeaseStore`]. `ChannelBridge::reconcile` checks this before
    /// starting or continuing to run any channel binding.
    pub channel_leases: ChannelLeaseStore,
    /// Workspace-level Slack connection record store — see
    /// [`SlackConnectionStore`]. A `ChannelKindConfig::Slack` binding's
    /// `connection_id` references a row here rather than carrying identity
    /// (or credentials) itself.
    pub slack_connections: SlackConnectionStore,
    /// Conversation→thread registry — see
    /// [`SlackConversationRegistryStore`]. Keyed workspace-wide by
    /// `(team_id, channel_id, thread_ts)`, not by binding, so a second agent
    /// bound into the same Slack channel is a second row, not a redesign.
    pub slack_conversations: SlackConversationRegistryStore,
    /// Channel-agnostic conversation→thread registry, shared by
    /// Discord/Telegram/Email — see [`ConversationRegistryStore`]. Keyed by
    /// `(binding_id, ConversationKey)`, where each channel composes its own
    /// key from whichever fields already separate one sender/room from
    /// another. Slack keeps its own separately-sharded registry above.
    pub conversation_registry: ConversationRegistryStore,
    /// Server-authoritative per-binding sender allow-list — see
    /// [`LinkedSenderStore`] and `ChannelBinding::allowed_senders`'s doc for
    /// why this replaces that inline field as the thing enforcement reads.
    pub linked_senders: LinkedSenderStore,
    pub preferences: UserPreferencesStore,
    pub assets: AssetStore,
    pub artifacts: Arc<ArtifactStore>,
    pub artifact_groups: Arc<ArtifactGroupStore>,
    pub threads: Arc<ThreadStore>,
    pub assignments: Arc<AssignmentStore>,
    pub assignment_runs: Arc<AssignmentRunStore>,
    /// Durable per-assignment dedup scratchpad for the agent-driven watch
    /// detection tier — see [`AssignmentScratchpadStore`].
    pub assignment_scratchpads: AssignmentScratchpadStore,
    /// Staged output of the reflection pass — see
    /// [`ReflectionStagingStore`].
    pub reflection_staging: Arc<ReflectionStagingStore>,
    /// Per-turn feedback signal store — see
    /// [`OutcomeStore`]. Also the instrumentation target for the
    /// acceptance-rate promotion budget's human keep/forget events
    /// (`ao_engine_tools_engine::memory::promotion_budget`).
    pub outcome: OutcomeStore,
    /// Local SQLite FTS5 full-text index shared by the memory store and the
    /// skill registry (local retrieval, no vector DB, offline by
    /// default). Cheap to clone; `memory` above already holds a copy so its
    /// writes stay incrementally consistent with this index.
    pub search_index: ao_search_index::SearchIndex,
    pub data_root: DataRoot,
}

impl PersistenceLayer {
    /// Initialize the persistence layer: resolve data root, create directories, load snapshot.
    pub async fn init() -> Result<Self, AoError> {
        let data_root = DataRoot::resolve()?;
        Self::init_with_root(data_root).await
    }

    /// Initialize with an explicit data root (useful for tests).
    pub async fn init_with_root(data_root: DataRoot) -> Result<Self, AoError> {
        data_root.ensure_directories().await?;
        let snapshots = Arc::new(SnapshotStore::load(data_root.clone()).await?);
        let threads = Arc::new(ThreadStore::load(data_root.clone()).await?);

        // Assignment stores start empty and are populated on first API call.
        // No migration is needed — the run-log directory is created by
        // `ensure_directories`.
        let assignments = Arc::new(AssignmentStore::load(data_root.clone()).await?);
        let assignment_runs = Arc::new(AssignmentRunStore::new(data_root.clone()));

        // Opening SQLite (and creating the FTS5 schema) is a blocking call;
        // run it off the async runtime's worker threads like every other
        // blocking `ao_search_index::SearchIndex` operation.
        let search_index_path = data_root.search_index_path();
        let search_index = tokio::task::spawn_blocking(move || {
            ao_search_index::SearchIndex::open(&search_index_path)
        })
        .await
        .map_err(|e| AoError::Internal(format!("search index init task panicked: {e}")))??;

        let agents_store = AgentProfileStore::new(data_root.clone());
        let agent_profiles = agents_store.list().await.unwrap_or_default();

        // One-time backfill: stamp `Thread::channel_origin` on every Slack
        // per-conversation bridge thread created before that field existed
        // (see `ChannelBridgeOrigin`'s docstring — Slack never had a single
        // `bridge_thread_id` to reverse-look-up from, so its bridge threads
        // were invisible to both the composer-gating hint and the backend's
        // `is_channel_bridge_thread` tool-admission gate until they're
        // stamped here). Best-effort: a row whose agent or Slack binding no
        // longer exists is skipped, and a per-thread failure is logged
        // rather than failing startup — a missed backfill only means one
        // thread stays un-gated until the next boot, not data loss.
        let slack_conversations = SlackConversationRegistryStore::new(data_root.clone());
        if let Ok(rows) = slack_conversations.list_all().await {
            for row in rows {
                let Some(binding_id) = agent_profiles
                    .iter()
                    .find(|p| p.id == row.agent_id)
                    .and_then(|p| p.channels.iter().find(|b| b.kind == ChannelKind::Slack))
                    .map(|b| b.binding_id.clone())
                else {
                    continue;
                };
                let origin = ChannelBridgeOrigin { kind: ChannelKind::Slack, binding_id };
                if let Err(e) = threads.backfill_channel_origin(&row.thread_id, origin).await {
                    tracing::warn!(
                        thread_id = %row.thread_id,
                        error = %e,
                        "channel_origin backfill failed for a Slack bridge thread"
                    );
                }
            }
        }

        // One-time backfill: stamp `Thread::assignment_origin` on every
        // thread a `Fresh`- or `Dedicated`-policy assignment run resolved to
        // before that field existed. `Main`-policy assignments are skipped
        // entirely — their runs land on the agent's ordinary default thread,
        // which must never be marked as assignment-owned. Best-effort, like
        // the Slack backfill above: a per-thread failure is logged rather
        // than failing startup.
        for assignment in assignments.list_all().await {
            // Gated on the assignment's *current* thread_policy, so a run
            // fired while the assignment was still `Main` — recorded with
            // the agent's own default thread id (see `fire_assignment`'s
            // `display_thread_id`) — must never get backfilled just because
            // the assignment was later switched to `Fresh`/`Dedicated`. This
            // check is what keeps that guarantee even after a policy change.
            let default_id = default_thread_id(&assignment.agent_id);
            match assignment.thread_policy {
                AssignmentThreadPolicy::Fresh => {
                    let Ok(runs) = assignment_runs.list_for_assignment(&assignment.id).await
                    else {
                        continue;
                    };
                    for run in runs {
                        let Some(thread_id) = &run.thread_id else {
                            continue;
                        };
                        if *thread_id == default_id {
                            continue;
                        }
                        let origin = AssignmentBridgeOrigin {
                            assignment_id: assignment.id.clone(),
                            run_id: Some(run.id.clone()),
                        };
                        if let Err(e) =
                            threads.backfill_assignment_origin(thread_id, origin).await
                        {
                            tracing::warn!(
                                thread_id = %thread_id,
                                assignment_id = %assignment.id,
                                error = %e,
                                "assignment_origin backfill failed for a Fresh-policy run thread"
                            );
                        }
                    }
                }
                AssignmentThreadPolicy::Dedicated => {
                    let Some(thread_id) = &assignment.dedicated_thread_id else {
                        continue;
                    };
                    let origin =
                        AssignmentBridgeOrigin { assignment_id: assignment.id.clone(), run_id: None };
                    if let Err(e) = threads.backfill_assignment_origin(thread_id, origin).await {
                        tracing::warn!(
                            thread_id = %thread_id,
                            assignment_id = %assignment.id,
                            error = %e,
                            "assignment_origin backfill failed for a Dedicated assignment thread"
                        );
                    }
                }
                AssignmentThreadPolicy::Main => {}
            }
        }

        Ok(Self {
            agents: agents_store,
            projects: ProjectStore::new(data_root.clone()),
            tasklists: TasklistStore::new(data_root.clone()),
            transcripts: TranscriptStore::new(data_root.clone()),
            changelogs: ChangelogStore::new(data_root.clone()),
            snapshots,
            memory: Arc::new(MemoryStore::new(data_root.clone()).with_index(search_index.clone())),
            bookmarks: BookmarkStore::new(data_root.clone()),
            channel_cursors: ChannelCursorStore::new(data_root.clone()),
            channel_leases: ChannelLeaseStore::new(data_root.clone()),
            slack_connections: SlackConnectionStore::new(data_root.clone()),
            slack_conversations,
            conversation_registry: ConversationRegistryStore::new(data_root.clone()),
            linked_senders: LinkedSenderStore::new(data_root.clone()),
            preferences: UserPreferencesStore::new(data_root.clone()),
            assets: AssetStore::new(data_root.clone()),
            artifacts: Arc::new(ArtifactStore::new(data_root.clone())),
            artifact_groups: Arc::new(ArtifactGroupStore::new(data_root.clone())),
            threads,
            assignments,
            assignment_runs,
            assignment_scratchpads: AssignmentScratchpadStore::new(data_root.clone()),
            reflection_staging: Arc::new(ReflectionStagingStore::new(data_root.clone())),
            outcome: OutcomeStore::new(data_root.clone()),
            search_index,
            data_root,
        })
    }
}

#[cfg(test)]
mod tests;
