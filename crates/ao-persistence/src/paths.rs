use std::path::PathBuf;

use ao_protocol::error::AoError;

/// Root directory for all Launchpad Studio data.
/// Resolves from `LAUNCHPAD_STUDIO_DATA_DIR` env var or defaults to `~/.launchpad_studio`.
#[derive(Debug, Clone)]
pub struct DataRoot {
    root: PathBuf,
}

/// Top-level directory names required for [`DataRoot::looks_like_data_root`]
/// to consider a path an existing, adoptable Launchpad data root.
///
/// Deliberately a SUBSET of the directories [`DataRoot::ensure_directories`]
/// creates, not the full list: `agents/` and `messages/` are the two
/// directories that have existed since the very first version of this
/// struct (the "Implement DataRoot paths" commit) and every store built on
/// top of them depends on their presence unconditionally. Every
/// other directory `ensure_directories` creates today (`memory/`,
/// `projects/`, `agent_homes/`, ...) was added in a later release. Requiring the
/// full current list here would refuse to adopt a perfectly good data root
/// created by an older build that predates one of those additions — this
/// constant exists specifically so that doesn't happen. Extra top-level
/// entries beyond this subset are tolerated; only these two are required.
pub const CORE_DATA_ROOT_DIRS: &[&str] = &["agents", "messages"];

impl DataRoot {
    /// Create a DataRoot from an explicit path (useful for tests).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve the data root path from environment or default.
    pub fn resolve() -> Result<Self, AoError> {
        Ok(Self {
            root: ao_protocol::data_root::resolve_data_root()?,
        })
    }

    /// Read-only check for whether this path already looks like a valid
    /// Launchpad data root, as opposed to some other non-empty directory a
    /// user might point workspace creation at. Requires every entry in
    /// [`CORE_DATA_ROOT_DIRS`] to exist as a directory; tolerates anything
    /// else being present or absent. Never creates, deletes, or modifies
    /// anything on disk.
    pub async fn looks_like_data_root(&self) -> bool {
        for name in CORE_DATA_ROOT_DIRS {
            match tokio::fs::metadata(self.root.join(name)).await {
                Ok(meta) if meta.is_dir() => {}
                _ => return false,
            }
        }
        true
    }

    /// `{root}/.workspace.lock` — records the pid (and, if known, the
    /// AO_PORT) of the ao-server process currently running against this
    /// data root. Written at server startup and removed on graceful
    /// shutdown; a stale entry (dead pid) is never treated as a conflict.
    /// Not part of the workspace's application data — it exists solely so
    /// `POST /workspaces/{id}/activate` can refuse to hand a live root to a
    /// second process. See `ao_server::workspace_lock` for the read/write/
    /// liveness logic; this crate only owns the path, same as every other
    /// per-root file this struct resolves.
    pub fn workspace_lock_path(&self) -> PathBuf {
        self.root.join(".workspace.lock")
    }

    /// Create all required subdirectories.
    pub async fn ensure_directories(&self) -> Result<(), AoError> {
        let dirs = [
            self.agents_dir(),
            self.messages_metadata_dir(),
            self.messages_data_dir(),
            self.messages_data_dir().join("tasks"),
            self.threads_data_dir(),
            self.memory_dir(),
            self.memory_agents_dir(),
            self.memory_projects_dir(),
            self.memory_threads_dir(),
            self.bookmarks_dir(),
            self.assignment_scratchpads_dir(),
            self.channel_cursors_dir(),
            self.channel_leases_dir(),
            self.linked_senders_dir(),
            self.slack_connections_dir(),
            self.slack_conversations_dir(),
            self.conversation_registry_dir(),
            self.projects_dir(),
            self.agent_homes_dir(),
            self.assignment_runs_dir(),
        ];
        for dir in &dirs {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    pub fn messages_metadata_dir(&self) -> PathBuf {
        self.root.join("messages").join("metadata")
    }

    pub fn messages_data_dir(&self) -> PathBuf {
        self.root.join("messages").join("data")
    }

    pub fn agent_transcript_path(&self, agent_id: &str) -> PathBuf {
        self.messages_data_dir().join(format!("{}.jsonl", agent_id))
    }

    /// Per-turn outcome-record file for an agent, sitting next to
    /// [`Self::agent_transcript_path`] in the same directory.
    /// `{root}/messages/data/{agent_id}.outcomes.jsonl`.
    pub fn agent_outcome_path(&self, agent_id: &str) -> PathBuf {
        self.messages_data_dir()
            .join(format!("{}.outcomes.jsonl", agent_id))
    }

    /// Staged reflection-candidate file for an agent,
    /// sitting next to [`Self::agent_transcript_path`] in the same directory.
    /// `{root}/messages/data/{agent_id}.reflection_candidates.jsonl`.
    pub fn agent_reflection_staging_path(&self, agent_id: &str) -> PathBuf {
        self.messages_data_dir()
            .join(format!("{}.reflection_candidates.jsonl", agent_id))
    }

    /// Directory holding per-thread transcripts for non-default threads
    /// (kind `Fresh` or `Branch`). Default threads instead live at
    /// [`Self::agent_transcript_path`], which is the path an agent's
    /// transcript already used before threads existed — so materializing a
    /// default thread row aliases that file rather than moving any messages
    /// on disk.
    pub fn threads_data_dir(&self) -> PathBuf {
        self.messages_data_dir().join("threads")
    }

    /// Transcript file for a non-default thread.
    /// `{root}/messages/data/threads/{thread_id}.jsonl`.
    pub fn thread_transcript_path(&self, thread_id: &str) -> PathBuf {
        self.threads_data_dir().join(format!("{}.jsonl", thread_id))
    }

    /// Directory holding per-artifact chat mini-thread transcripts. A
    /// purpose-built sibling of [`Self::threads_data_dir`] rather than a
    /// `ThreadStore`-backed thread: the per-artifact chat panel only ever has
    /// two roles (user/assistant) and none of the archive/branch/distillation
    /// semantics `ThreadScope` carries.
    pub fn artifact_threads_data_dir(&self) -> PathBuf {
        self.messages_data_dir().join("artifact_threads")
    }

    /// Transcript file for one artifact's chat mini-thread.
    /// `{root}/messages/data/artifact_threads/{artifact_id}.jsonl`.
    pub fn artifact_thread_path(&self, artifact_id: &str) -> PathBuf {
        self.artifact_threads_data_dir()
            .join(format!("{}.jsonl", artifact_id))
    }

    /// On-disk store for [`crate::thread_store::ThreadStore`].
    /// `{root}/threads.json`.
    pub fn threads_path(&self) -> PathBuf {
        self.root.join("threads.json")
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.messages_metadata_dir().join("snapshot.json")
    }

    // --- New memory layout: memory/ ---

    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    pub fn memory_agents_dir(&self) -> PathBuf {
        self.memory_dir().join("agents")
    }

    pub fn memory_projects_dir(&self) -> PathBuf {
        self.memory_dir().join("projects")
    }

    pub fn memory_agent_path(&self, agent_id: &str) -> PathBuf {
        self.memory_agents_dir().join(format!("{}.jsonl", agent_id))
    }

    pub fn memory_global_path(&self) -> PathBuf {
        self.memory_dir().join("global.jsonl")
    }

    pub fn memory_project_path(&self, hash: &str) -> PathBuf {
        self.memory_projects_dir().join(format!("{}.jsonl", hash))
    }

    pub fn memory_projects_index_path(&self) -> PathBuf {
        self.memory_projects_dir().join("index.json")
    }

    /// Directory holding one JSONL file per thread for the ephemeral
    /// thread-scope memory tier — a sibling of [`Self::memory_agents_dir`]
    /// and [`Self::memory_projects_dir`], not nested inside either, since a
    /// thread id is neither an agent id nor a project hash.
    pub fn memory_threads_dir(&self) -> PathBuf {
        self.memory_dir().join("threads")
    }

    /// Thread-scope memory file: `{root}/memory/threads/{thread_id}.jsonl`.
    pub fn memory_thread_path(&self, thread_id: &str) -> PathBuf {
        self.memory_threads_dir().join(format!("{}.jsonl", thread_id))
    }

    /// SQLite FTS5 full-text index shared by the memory store and the skill
    /// registry (`ao_search_index::SearchIndex`). One file per data root, so
    /// it moves with `LAUNCHPAD_STUDIO_DATA_DIR` the same as everything else.
    pub fn search_index_path(&self) -> PathBuf {
        self.root.join(ao_search_index::SEARCH_INDEX_FILENAME)
    }

    pub fn bookmarks_dir(&self) -> PathBuf {
        self.root.join("bookmarks")
    }

    pub fn agent_bookmark_path(&self, agent_id: &str) -> PathBuf {
        self.bookmarks_dir().join(format!("{}.jsonl", agent_id))
    }

    /// Directory holding one JSON file per channel binding's durable dedup
    /// cursor (see `ao_protocol::channel_cursor::ChannelCursor`).
    pub fn channel_cursors_dir(&self) -> PathBuf {
        self.root.join("channel_cursors")
    }

    /// Cursor file for one `(agent_id, binding_id)` channel binding.
    /// `{root}/channel_cursors/{agent_id}__{binding_id}.json`.
    pub fn channel_cursor_path(&self, agent_id: &str, binding_id: &str) -> PathBuf {
        self.channel_cursors_dir()
            .join(agent_binding_file_name(agent_id, binding_id))
    }

    /// Directory holding one JSON file per assignment's durable dedup
    /// scratchpad (see `ao_protocol::assignment_scratchpad::AssignmentScratchpad`).
    pub fn assignment_scratchpads_dir(&self) -> PathBuf {
        self.root.join("assignment_scratchpads")
    }

    /// Scratchpad file for one assignment.
    /// `{root}/assignment_scratchpads/{assignment_id}.json`.
    pub fn assignment_scratchpad_path(&self, assignment_id: &str) -> PathBuf {
        self.assignment_scratchpads_dir()
            .join(format!("{}.json", assignment_id))
    }

    /// Directory holding one JSON file per channel binding's durable sender
    /// allow-list (see `ao_protocol::linked_sender_list::LinkedSenderList`).
    pub fn linked_senders_dir(&self) -> PathBuf {
        self.root.join("linked_senders")
    }

    /// Sender allow-list file for one `(agent_id, binding_id)` channel
    /// binding. `{root}/linked_senders/{agent_id}__{binding_id}.json`.
    pub fn linked_sender_path(&self, agent_id: &str, binding_id: &str) -> PathBuf {
        self.linked_senders_dir()
            .join(agent_binding_file_name(agent_id, binding_id))
    }

    /// Directory holding one JSON file per channel binding's durable
    /// single-writer lease (see `ao_protocol::channel_lease::ChannelLease`).
    pub fn channel_leases_dir(&self) -> PathBuf {
        self.root.join("channel_leases")
    }

    /// Lease file for one `(agent_id, binding_id)` channel binding.
    /// `{root}/channel_leases/{agent_id}__{binding_id}.json`.
    pub fn channel_lease_path(&self, agent_id: &str, binding_id: &str) -> PathBuf {
        self.channel_leases_dir()
            .join(agent_binding_file_name(agent_id, binding_id))
    }

    /// Directory holding one JSON file per Slack workspace connection record
    /// (see `ao_protocol::slack_connection::SlackConnection`).
    pub fn slack_connections_dir(&self) -> PathBuf {
        self.root.join("slack_connections")
    }

    /// Connection record file, keyed by the opaque `connection_id` a Slack
    /// binding's `ChannelKindConfig::Slack::connection_id` references.
    /// `{root}/slack_connections/{connection_id}.json`.
    pub fn slack_connection_path(&self, connection_id: &str) -> PathBuf {
        self.slack_connections_dir()
            .join(format!("{}.json", connection_id))
    }

    /// Directory holding one JSON file per Slack workspace's conversation→
    /// thread registry (see
    /// `ao_protocol::slack_conversation_registry::SlackConversationRow`).
    pub fn slack_conversations_dir(&self) -> PathBuf {
        self.root.join("slack_conversations")
    }

    /// Registry file for one Slack workspace's conversations, keyed by
    /// `team_id` — every lookup a Slack event triggers already carries its
    /// `team_id`, so this is the natural sharding key, and it keeps one
    /// noisy workspace's rewrite churn from touching any other workspace's
    /// file. `{root}/slack_conversations/{team_id}.json`.
    pub fn slack_conversation_registry_path(&self, team_id: &str) -> PathBuf {
        self.slack_conversations_dir().join(format!("{}.json", team_id))
    }

    /// Directory holding one JSON file per `(agent_id, binding_id)` channel
    /// binding's generic conversation→thread registry (see
    /// `ao_protocol::conversation_registry::ConversationRow`). Shared by
    /// Discord/Telegram/Email; Slack keeps its own separately-sharded
    /// registry at [`Self::slack_conversations_dir`].
    pub fn conversation_registry_dir(&self) -> PathBuf {
        self.root.join("conversation_registry")
    }

    /// Registry file for one `(agent_id, binding_id)` channel binding's
    /// conversations. `binding_id` alone is not a safe sharding key: it is a
    /// fixed constant per channel kind (e.g. every Telegram binding is
    /// `"telegram"`), so two different agents with the same channel kind
    /// would otherwise collide on one file and steal each other's inbound
    /// conversations. Sharding by `(agent_id, binding_id)` matches
    /// [`Self::channel_cursor_path`], [`Self::linked_sender_path`], and
    /// [`Self::channel_lease_path`], every other per-agent-binding artifact.
    /// `{root}/conversation_registry/{agent_id}__{binding_id}.json`.
    pub fn conversation_registry_path(&self, agent_id: &str, binding_id: &str) -> PathBuf {
        self.conversation_registry_dir()
            .join(agent_binding_file_name(agent_id, binding_id))
    }

    /// Base directory for all agent home directories.
    pub fn agent_homes_dir(&self) -> PathBuf {
        self.root.join("agent_homes")
    }

    /// Home directory for a specific agent.
    pub fn agent_home_dir(&self, agent_id: &str) -> PathBuf {
        self.agent_homes_dir().join(agent_id)
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn project_path(&self, project_id: &str) -> PathBuf {
        self.projects_dir().join(format!("{}.yaml", project_id))
    }

    /// Legacy read-only subtree. Team-scoped tasklists are no longer
    /// creatable — `TasklistService::create` rejects `TasklistOwner::Team`
    /// outright — but the variant is still deserializable, so installs that
    /// predate the removal keep working. Deliberately NOT in
    /// [`Self::ensure_directories`]: a fresh data root should not sprout a
    /// directory for a feature it can never use. Every remaining reader
    /// guards on `try_exists` and yields an empty result when it is absent.
    pub fn teams_dir(&self) -> PathBuf {
        self.root.join("teams")
    }

    pub fn team_transcript_path(&self, team_id: &str) -> PathBuf {
        self.messages_data_dir().join(format!("team_{}.jsonl", team_id))
    }

    pub fn team_agent_transcript_path(&self, team_id: &str, agent_id: &str) -> PathBuf {
        self.messages_data_dir().join(format!("team_{}_{}.jsonl", team_id, agent_id))
    }

    /// Directory holding all tasklists for a given team:
    /// `{root}/teams/{team_id}/tasklists/`.
    pub fn team_tasklists_dir(&self, team_id: &str) -> PathBuf {
        self.teams_dir().join(team_id).join("tasklists")
    }

    /// Per-tasklist root directory.
    pub fn tasklist_dir(&self, team_id: &str, tasklist_id: &str) -> PathBuf {
        self.team_tasklists_dir(team_id).join(tasklist_id)
    }

    /// Per-tasklist metadata file (`tasklist.json`).
    pub fn tasklist_meta_path(&self, team_id: &str, tasklist_id: &str) -> PathBuf {
        self.tasklist_dir(team_id, tasklist_id).join("tasklist.json")
    }

    /// Per-tasklist shared workspace directory.
    pub fn tasklist_workspace_dir(&self, team_id: &str, tasklist_id: &str) -> PathBuf {
        self.tasklist_dir(team_id, tasklist_id).join("workspace")
    }

    /// Hidden append-only changelog of `<task-item-notification>` payloads
    /// emitted by CLI agents working tasks in this tasklist. Lives inside
    /// the workspace directory so it travels with other tasklist outputs,
    /// but is named with a leading underscore so the outputs widget can
    /// skip it.
    pub fn tasklist_changelog_path(&self, team_id: &str, tasklist_id: &str) -> PathBuf {
        self.tasklist_workspace_dir(team_id, tasklist_id)
            .join("_changelog.jsonl")
    }

    /// Per-tasklist transcripts directory.
    pub fn tasklist_transcripts_dir(&self, team_id: &str, tasklist_id: &str) -> PathBuf {
        self.tasklist_dir(team_id, tasklist_id).join("transcripts")
    }

    /// Per-tasklist, per-agent transcript file path.
    /// `{root}/teams/{team_id}/tasklists/{tasklist_id}/transcripts/{agent_id}.jsonl`.
    pub fn tasklist_agent_transcript_path(
        &self,
        team_id: &str,
        tasklist_id: &str,
        agent_id: &str,
    ) -> PathBuf {
        self.tasklist_transcripts_dir(team_id, tasklist_id)
            .join(format!("{}.jsonl", agent_id))
    }

    /// Root directory for scheduled workflow task records (`{root}/tasks/`).
    /// Holds the flat per-task JSON files written by `TaskStore` as well as
    /// the `agents/` subtree (see `tasks_agents_dir`).
    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    /// Parent of every per-agent directory under `tasks/agents/`.
    /// Used by the watchdog to enumerate all agents that have tasklists.
    pub fn tasks_agents_dir(&self) -> PathBuf {
        self.tasks_dir().join("agents")
    }

    /// Base directory for all agent-owned tasklists.
    /// `{root}/tasks/agents/{agent_id}/tasklists/`
    pub fn agent_tasklists_dir(&self, agent_id: &str) -> PathBuf {
        self.tasks_agents_dir().join(agent_id).join("tasklists")
    }

    /// Per-tasklist root directory for agent-owned tasklists.
    pub fn agent_tasklist_dir(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklists_dir(agent_id).join(tasklist_id)
    }

    /// Per-tasklist metadata file for agent-owned tasklists.
    pub fn agent_tasklist_meta_path(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklist_dir(agent_id, tasklist_id)
            .join("tasklist.json")
    }

    /// Per-tasklist workspace directory for agent-owned tasklists.
    pub fn agent_tasklist_workspace_dir(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklist_dir(agent_id, tasklist_id).join("workspace")
    }

    /// Changelog for an agent-owned tasklist — the agent-tree counterpart of
    /// [`Self::tasklist_changelog_path`]. Same filename and same
    /// inside-the-workspace placement, so both ownership flavours keep the
    /// changelog next to the outputs it describes.
    pub fn agent_tasklist_changelog_path(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklist_workspace_dir(agent_id, tasklist_id)
            .join("_changelog.jsonl")
    }

    /// Per-tasklist transcripts directory for agent-owned tasklists.
    pub fn agent_tasklist_transcripts_dir(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklist_dir(agent_id, tasklist_id)
            .join("transcripts")
    }

    /// Per-task transcript file path for agent-owned tasklists.
    /// `{root}/tasks/agents/{agent_id}/tasklists/{tasklist_id}/transcripts/{task_id}.jsonl`
    pub fn agent_tasklist_transcript_path(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> PathBuf {
        self.agent_tasklist_transcripts_dir(agent_id, tasklist_id)
            .join(format!("{}.jsonl", task_id))
    }

    /// Progress log for an agent-owned tasklist run.
    /// `{workspace}/progress.jsonl`
    pub fn agent_tasklist_progress_log(&self, agent_id: &str, tasklist_id: &str) -> PathBuf {
        self.agent_tasklist_workspace_dir(agent_id, tasklist_id)
            .join("progress.jsonl")
    }

    /// Per-task output directory inside an agent-owned tasklist workspace.
    /// `{workspace}/tasks/{task_id}/`
    pub fn agent_tasklist_task_dir(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> PathBuf {
        self.agent_tasklist_workspace_dir(agent_id, tasklist_id)
            .join("tasks")
            .join(task_id)
    }

    /// Per-task text output file inside an agent-owned tasklist workspace.
    /// `{workspace}/tasks/{task_id}/output.txt`
    pub fn agent_tasklist_task_output_path(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> PathBuf {
        self.agent_tasklist_task_dir(agent_id, tasklist_id, task_id)
            .join("output.txt")
    }

    /// Per-task directory for colocated task content.
    /// `{workspace}/tasks/{task_id}/`
    ///
    /// Equivalent to `agent_tasklist_task_dir` — provides the canonical name
    /// used by the task classifier, boot sweep, and transcript pruner.
    pub fn task_dir(&self, parent_agent_id: &str, tasklist_id: &str, task_id: &str) -> PathBuf {
        self.agent_tasklist_workspace_dir(parent_agent_id, tasklist_id)
            .join("tasks")
            .join(task_id)
    }

    /// Per-task JSONL transcript at the new path: `{task_dir}/transcript.jsonl`
    pub fn task_transcript_path(
        &self,
        parent_agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> PathBuf {
        self.task_dir(parent_agent_id, tasklist_id, task_id)
            .join("transcript.jsonl")
    }

    /// Per-task metadata sidecar: `{task_dir}/meta.json`
    pub fn task_meta_path(
        &self,
        parent_agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> PathBuf {
        self.task_dir(parent_agent_id, tasklist_id, task_id)
            .join("meta.json")
    }

    pub fn user_preferences_path(&self) -> PathBuf {
        self.root.join("user_preferences.yaml")
    }

    /// `{root}/assignments.json` — the Assignment metadata store.
    pub fn assignments_path(&self) -> PathBuf {
        self.root.join("assignments.json")
    }

    /// `{root}/assignment_runs/` — directory holding per-assignment run JSONL
    /// files.
    pub fn assignment_runs_dir(&self) -> PathBuf {
        self.root.join("assignment_runs")
    }

    /// `{root}/assignment_runs/{assignment_id}.jsonl` — append-only run log
    /// for one assignment.
    pub fn assignment_runs_path(&self, assignment_id: &str) -> PathBuf {
        self.assignment_runs_dir()
            .join(format!("{}.jsonl", assignment_id))
    }

    /// Base directory for all agent asset directories.
    pub fn assets_base_dir(&self) -> PathBuf {
        self.root.join("messages").join("assets")
    }

    pub fn assets_dir(&self, agent_id: &str) -> PathBuf {
        self.root
            .join("messages")
            .join("assets")
            .join(agent_id)
            .join("files")
    }

    pub fn asset_registry_path(&self, agent_id: &str) -> PathBuf {
        self.root
            .join("messages")
            .join("assets")
            .join(agent_id)
            .join("registry.json")
    }

    /// Parent of every agent's artifact directory, for cross-agent walks
    /// (e.g. aggregating pinned artifacts). `{root}/artifacts/`
    pub fn artifacts_root_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// Base directory for one agent's artifact records.
    /// `{root}/artifacts/{agent_id}/`
    pub fn artifact_dir(&self, agent_id: &str) -> PathBuf {
        self.artifacts_root_dir().join(agent_id)
    }

    /// Directory holding an agent's artifact payload blobs.
    /// `{root}/artifacts/{agent_id}/blobs/`
    pub fn artifact_blobs_dir(&self, agent_id: &str) -> PathBuf {
        self.artifact_dir(agent_id).join("blobs")
    }

    /// Registry file for an agent's artifact records.
    /// `{root}/artifacts/{agent_id}/registry.json`
    pub fn artifact_registry_path(&self, agent_id: &str) -> PathBuf {
        self.artifact_dir(agent_id).join("registry.json")
    }

    /// Directory holding an agent's undo-history snapshot blobs — a sibling
    /// of `blobs/`, not nested inside it, so a directory walk over one never
    /// has to filter out the other. `{root}/artifacts/{agent_id}/history/`
    pub fn artifact_history_dir(&self, agent_id: &str) -> PathBuf {
        self.artifact_dir(agent_id).join("history")
    }

    /// Registry file for user-defined artifact groups (the Assets sidebar's
    /// collapsible sections). Global, not per-agent — mirrors the cross-agent
    /// scope of the pinned-artifacts view. `{root}/artifact_groups.json`
    pub fn artifact_groups_path(&self) -> PathBuf {
        self.root.join("artifact_groups.json")
    }
}

/// Filename for a per-`(agent_id, binding_id)` artifact, joining the two ids
/// with `__`.
///
/// The pair only round-trips unambiguously while `binding_id` itself contains
/// no `__`: given that, splitting at the *last* `__` always recovers the pair
/// no matter what `agent_id` holds. Without it, `("a", "b__c")` and
/// `("a__b", "c")` both name `a__b__c.json` — two distinct bindings aliased
/// onto one file, which is exactly the collision class this sharding scheme
/// exists to prevent.
///
/// Every `binding_id` is a compile-time channel-kind constant (`"telegram"`,
/// `"email-default"`, `"discord-default"`, `"slack"`), so the invariant holds
/// today; nothing in the type system enforces it. The assert makes a future
/// constant that breaks it fail loudly in debug and test builds instead of
/// silently corrupting routing at runtime.
fn agent_binding_file_name(agent_id: &str, binding_id: &str) -> String {
    debug_assert!(
        !binding_id.contains("__"),
        "binding_id must not contain \"__\" (got {:?}): it is the delimiter in \
         {{agent_id}}__{{binding_id}} artifact filenames, so a binding_id \
         containing it can alias two distinct (agent_id, binding_id) pairs onto \
         the same file",
        binding_id
    );
    format!("{}__{}.json", agent_id, binding_id)
}

#[cfg(test)]
mod paths_tests {
    use super::*;

    fn fixture_root() -> DataRoot {
        DataRoot::new("/data")
    }

    #[test]
    fn task_meta_path_resolves_under_task_dir() {
        let dr = fixture_root();
        let meta = dr.task_meta_path("agent-1", "tl-42", "task-7");
        let expected = dr.task_dir("agent-1", "tl-42", "task-7").join("meta.json");
        assert_eq!(meta, expected);
    }

    #[test]
    fn task_transcript_path_resolves_under_task_dir() {
        let dr = fixture_root();
        let tr = dr.task_transcript_path("agent-1", "tl-42", "task-7");
        let expected = dr.task_dir("agent-1", "tl-42", "task-7").join("transcript.jsonl");
        assert_eq!(tr, expected);
    }

    #[test]
    fn task_dir_matches_agent_tasklist_task_dir() {
        let dr = fixture_root();
        assert_eq!(
            dr.task_dir("a", "tl", "t"),
            dr.agent_tasklist_task_dir("a", "tl", "t")
        );
    }

    #[test]
    fn paths_progress_log_resolves_under_workspace() {
        let dr = fixture_root();
        let path = dr.agent_tasklist_progress_log("agent-1", "tl-42");
        let workspace = dr.agent_tasklist_workspace_dir("agent-1", "tl-42");
        assert_eq!(path, workspace.join("progress.jsonl"));
    }

    #[test]
    fn paths_task_dir_resolves_under_workspace() {
        let dr = fixture_root();
        let path = dr.agent_tasklist_task_dir("agent-1", "tl-42", "task-7");
        let workspace = dr.agent_tasklist_workspace_dir("agent-1", "tl-42");
        assert_eq!(path, workspace.join("tasks").join("task-7"));
    }

    #[test]
    fn paths_task_output_composes_with_task_dir() {
        let dr = fixture_root();
        let task_dir = dr.agent_tasklist_task_dir("agent-1", "tl-42", "task-7");
        let output = dr.agent_tasklist_task_output_path("agent-1", "tl-42", "task-7");
        assert_eq!(output, task_dir.join("output.txt"));
    }

    #[test]
    fn channel_cursor_path_resolves_under_channel_cursors_dir() {
        let dr = fixture_root();
        let path = dr.channel_cursor_path("agent-1", "discord");
        assert_eq!(path, dr.channel_cursors_dir().join("agent-1__discord.json"));
    }

    #[test]
    fn channel_lease_path_resolves_under_channel_leases_dir() {
        let dr = fixture_root();
        let path = dr.channel_lease_path("agent-1", "discord");
        assert_eq!(path, dr.channel_leases_dir().join("agent-1__discord.json"));
    }

    #[test]
    fn linked_sender_path_resolves_under_linked_senders_dir() {
        let dr = fixture_root();
        let path = dr.linked_sender_path("agent-1", "discord");
        assert_eq!(path, dr.linked_senders_dir().join("agent-1__discord.json"));
    }

    #[test]
    fn slack_connection_path_resolves_under_slack_connections_dir() {
        let dr = fixture_root();
        let path = dr.slack_connection_path("conn-1");
        assert_eq!(path, dr.slack_connections_dir().join("conn-1.json"));
    }

    #[test]
    fn slack_conversation_registry_path_resolves_under_slack_conversations_dir() {
        let dr = fixture_root();
        let path = dr.slack_conversation_registry_path("T0123ABCD");
        assert_eq!(path, dr.slack_conversations_dir().join("T0123ABCD.json"));
    }

    #[test]
    fn conversation_registry_path_resolves_under_conversation_registry_dir() {
        let dr = fixture_root();
        let path = dr.conversation_registry_path("agent-1", "telegram");
        assert_eq!(path, dr.conversation_registry_dir().join("agent-1__telegram.json"));
    }

    #[test]
    fn conversation_registry_path_differs_by_agent_id_for_the_same_binding() {
        let dr = fixture_root();
        let path_a = dr.conversation_registry_path("agent-a", "telegram");
        let path_b = dr.conversation_registry_path("agent-b", "telegram");
        assert_ne!(path_a, path_b, "two agents sharing a binding_id must resolve to distinct registry files");
    }

    /// The `__` delimiter is only unambiguous while `binding_id` is free of
    /// it. Without the guard, `("agent-a", "b__telegram")` and
    /// `("agent-a__b", "telegram")` both resolve to
    /// `agent-a__b__telegram.json` — silently re-creating the two-bindings-one-
    /// file collision that per-agent sharding exists to prevent.
    // `debug_assert!` compiles out under `--release`, where this would fail by
    // not panicking; the test only exists in builds where the guard does.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "binding_id must not contain")]
    fn agent_binding_file_name_rejects_a_binding_id_containing_the_delimiter() {
        let _ = agent_binding_file_name("agent-a", "b__telegram");
    }

    /// Guards the premise of the assert above: every `binding_id` actually
    /// minted in the codebase is a channel-kind constant with no `__`, so the
    /// guard is a tripwire for future constants, not a live constraint on any
    /// existing one.
    #[test]
    fn every_known_binding_id_constant_satisfies_the_delimiter_invariant() {
        for binding_id in ["telegram", "email-default", "discord-default", "slack"] {
            assert!(
                !binding_id.contains("__"),
                "binding_id constant {:?} would trip the delimiter guard",
                binding_id
            );
        }
    }

    #[test]
    fn paths_assignment_helpers_resolve_under_root() {
        let dr = fixture_root();
        let root = dr.root().clone();
        assert_eq!(dr.assignments_path(), root.join("assignments.json"));
        assert_eq!(dr.assignment_runs_dir(), root.join("assignment_runs"));
        assert_eq!(
            dr.assignment_runs_path("assign-7"),
            dr.assignment_runs_dir().join("assign-7.jsonl")
        );
    }

    #[test]
    fn workspace_lock_path_is_dot_prefixed_under_root() {
        let dr = fixture_root();
        assert_eq!(dr.workspace_lock_path(), dr.root().join(".workspace.lock"));
    }

    #[tokio::test]
    async fn looks_like_data_root_false_for_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(!DataRoot::new(missing).looks_like_data_root().await);
    }

    #[tokio::test]
    async fn looks_like_data_root_false_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!DataRoot::new(tmp.path()).looks_like_data_root().await);
    }

    #[tokio::test]
    async fn looks_like_data_root_false_when_core_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Only one of the two CORE_DATA_ROOT_DIRS present.
        std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
        assert!(!DataRoot::new(tmp.path()).looks_like_data_root().await);
    }

    #[tokio::test]
    async fn looks_like_data_root_true_when_core_dirs_present() {
        let tmp = tempfile::tempdir().unwrap();
        for name in CORE_DATA_ROOT_DIRS {
            std::fs::create_dir_all(tmp.path().join(name)).unwrap();
        }
        assert!(DataRoot::new(tmp.path()).looks_like_data_root().await);
    }

    #[tokio::test]
    async fn looks_like_data_root_true_with_extra_entries_from_a_newer_build() {
        let tmp = tempfile::tempdir().unwrap();
        for name in CORE_DATA_ROOT_DIRS {
            std::fs::create_dir_all(tmp.path().join(name)).unwrap();
        }
        // Directories a newer ensure_directories() would also create — must
        // not be required for an older root to be recognized as valid.
        std::fs::create_dir_all(tmp.path().join("memory")).unwrap();
        std::fs::create_dir_all(tmp.path().join("projects")).unwrap();
        assert!(DataRoot::new(tmp.path()).looks_like_data_root().await);
    }

    /// Team-scoped tasklists cannot be created any more, so a brand-new data
    /// root must not contain a `teams/` directory. A reader opening the data
    /// dir should not find a folder for a feature the app does not have.
    #[tokio::test]
    async fn fresh_data_root_has_no_teams_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(tmp.path());
        root.ensure_directories().await.unwrap();

        assert!(
            !tokio::fs::try_exists(root.teams_dir()).await.unwrap(),
            "ensure_directories created a teams/ directory on a fresh root"
        );
        // The directories that replaced it are still created.
        assert!(tokio::fs::try_exists(root.projects_dir()).await.unwrap());
        assert!(tokio::fs::try_exists(root.agents_dir()).await.unwrap());
        // And the root is still recognizable as one.
        assert!(root.looks_like_data_root().await);
    }

    /// Every remaining reader of the legacy subtree guards on its absence, so
    /// a fresh root that never had a `teams/` directory must read as empty
    /// rather than erroring.
    #[tokio::test]
    async fn tasklist_reads_tolerate_a_missing_teams_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(tmp.path());
        root.ensure_directories().await.unwrap();

        let store = crate::tasklist_store::TasklistStore::new(root.clone());
        assert!(store.list_active_across_teams().await.unwrap().is_empty());
        assert!(store.list_all_across_teams().await.unwrap().is_empty());
        assert!(store
            .find_by_copilot_agent_id("copilot-x")
            .await
            .unwrap()
            .is_none());
    }
}
