// Behavioural tests for argv construction live in src/lib.rs::tests; see also tests.rs.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;
use tracing;
use uuid::Uuid;

use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::PersistenceLayer;
use ao_process::supervisor::{ProcessSupervisor, SpawnInput, TerminationReason};
use ao_protocol::agent::{
    AgentProfile, InputMode, ProviderConfig, ThinkingDisplay, ThinkingMode, WorkflowBinding,
};
#[allow(unused_imports)]
use ao_protocol::attachment::{Attachment, AttachmentType, FileCapability, ImageMode};
use ao_protocol::error::AoError;
use ao_protocol::event::{AgentEventPayload, RunEndReason};
#[allow(unused_imports)]
use ao_protocol::memory::MemoryEntry;
use ao_protocol::reflection_trigger::{NoopReflectionSubscriber, ReflectionTriggerSubscriber};
#[allow(unused_imports)]
use ao_protocol::workflow::{PhaseStatus, TaskStatus, WorkflowDefinition, WorkflowSummary};

use crate::command_queue::CommandQueue;
use crate::context::{
    build_prompt_with_context, ContextConfig,
};
use crate::mcp_session::McpSessionStore;
use crate::history::anchor::WindowAnchorRegistry;
use crate::context_cache::{CachedContext, ContextCache, ContextCacheKey};
use crate::plugin_cache::{filter_for_agent as filter_plugins_for_agent, PluginCache};
use crate::event_bus::EventBus;
use crate::instance_registry::{InstanceRegistry, InstanceRegistryGuard};
#[allow(unused_imports)]
use crate::memory_instructions::MEMORY_SAVE_INSTRUCTION;
use crate::tag_stream_scanner::{ScannerEvent, TagStreamScanner};
use crate::task_feeder::TaskFeeder;
use crate::tasklist_extraction::{
    extract_task_actions, extract_task_item_notification, extract_tasklist_actions,
    format_task_item_notification, format_tasklist_parse_error, strip_task_item_notification,
    NotificationParseResult, TaskItemNotification, TaskTagAction, TasklistTagAction,
};
use crate::workflow_queue_manager::WorkflowQueueHandle;
use crate::workflow_registry::WorkflowRegistry;
use crate::workflow_runner::WorkflowRunner;
use crate::prompt_sections::COPILOT_PROFILE_ID;
use ao_protocol::tasklist::TasklistScope;

use crate::agent_runner::timeline_adapter::TimelineAdapter;
use crate::agent_runner::shared::augment_prompt_with_attachments;
use ao_protocol::agent::AgentRunnerMode;
use super::{AgentRunner, AgentRunRequest, RunHandle, RunningAgents, RunningAgentsGuard};

use ao_engine_tools_core::Registry;

/// Convert a scanner-produced lifecycle event into the wire-level payload the
/// frontend listens for on SSE.
fn scanner_event_to_payload(event: ScannerEvent) -> AgentEventPayload {
    match event {
        ScannerEvent::ActionStarted {
            action_id,
            kind,
            summary,
        } => AgentEventPayload::AgentActionStarted {
            action_id,
            kind,
            summary,
        },
        ScannerEvent::ActionCompleted { action_id } => {
            AgentEventPayload::AgentActionCompleted { action_id }
        }
    }
}

/// Run a streamed payload through the per-turn tag scanner. For `TextDelta`
/// this strips recognized action tags from the outbound text and yields their
/// lifecycle events. For `TextComplete` it drains any orphan open tags and
/// resets the scanner so the next turn starts clean. Other payload kinds are
/// left untouched.
fn apply_tag_scanner(
    scanner: &mut TagStreamScanner,
    payload: &mut AgentEventPayload,
) -> Vec<AgentEventPayload> {
    let mut events = Vec::new();
    match payload {
        AgentEventPayload::TextDelta { text } => {
            let (stripped, scanner_events) = scanner.feed(text);
            *text = stripped;
            events.extend(scanner_events.into_iter().map(scanner_event_to_payload));
        }
        AgentEventPayload::TextComplete { .. } => {
            let (_flushed, scanner_events) = scanner.drain();
            events.extend(scanner_events.into_iter().map(scanner_event_to_payload));
            *scanner = TagStreamScanner::new();
        }
        _ => {}
    }
    events
}


/// RAII guard that deregisters an MCP session entry and removes its per-spawn mcp.json file
/// when it goes out of scope. Drop fires on every exit path — normal return and panic unwind —
/// so a subprocess crash never leaks a session entry or temp config file.
struct McpSessionGuard {
    sessions: Arc<McpSessionStore>,
    session_id: String,
    mcp_json_path: std::path::PathBuf,
}

impl Drop for McpSessionGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.session_id);
        let _ = std::fs::remove_file(&self.mcp_json_path);
        // Also remove the resurrection sidecar written alongside the config
        // file — its presence is the signal that the spawn is still alive,
        // so it must not outlive the subprocess.
        let _ = std::fs::remove_file(self.mcp_json_path.with_extension("meta.json"));
    }
}

/// Result sent via the completion channel when an agent run finishes.
/// Contains the run_id and the authoritative accumulated output text,
/// eliminating the need for collectors to reconstruct text from broadcast events.
/// A follow-up message to queue for the agent after a workflow action.
#[derive(Debug, Clone)]
pub struct WorkflowFollowup {
    pub context: String,
    /// Optional system transcript entry to persist before queuing the followup.
    /// Rendered as a centered system bubble in the chat UI.
    pub system_transcript: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunComplete {
    pub run_id: String,
    pub output_text: String,
    /// Follow-up messages to queue for the agent (e.g., next phase context).
    pub workflow_followups: Vec<WorkflowFollowup>,
    /// How the run actually terminated. Lets consumers (e.g. the queue
    /// manager's `AssignmentRun` completion branch) tell a clean finish
    /// apart from a spawn/process failure that still surfaced as `Ok(..)`
    /// rather than `Err(..)` — the CLI runner's continuation loop always
    /// breaks into the completion send, even on `RunEndReason::Error`.
    pub end_reason: RunEndReason,
}

/// Scope for an agent run — standalone (default), team-scoped, or tasklist-scoped.
#[derive(Debug, Clone)]
pub enum RunScope {
    /// Default standalone run. Events and registry use the agent's own ID.
    Standalone,
    /// Team-scoped run. Events use `team:{team_id}` as agent_id,
    /// registry uses `team:{team_id}:{agent_id}` as key,
    /// personal transcript is NOT read/written, and agent snapshot is NOT updated.
    Team {
        team_id: String,
        /// Optional context string to use instead of reading the agent's personal transcript.
        context_override: Option<String>,
    },
    /// Tasklist-scoped run. Used by the TaskFeeder when dispatching a task to
    /// an agent. Personal transcript is NOT touched; reads/writes go to the
    /// per-tasklist transcripts directory. The agent's system prompt is
    /// augmented with a tasklist-mode preamble (see [`crate::tasklist_runtime`]).
    Tasklist {
        scope: TasklistScope,
        tasklist_id: String,
        task_id: String,
    },
    /// Project channel run. Events emit on `project:{project_id}`, transcript
    /// key is `project_{project_id}`, and the personal agent transcript is
    /// NOT touched — keeping project conversations isolated from personal chat.
    Project {
        project_id: String,
    },
}

impl RunScope {
    /// The agent_id to use for event emission.
    /// Coordinator runs (context_override=None) emit on `team:{team_id}` so the
    /// SSE stream picks them up. Child/delegate runs (context_override=Some)
    /// emit on a hidden channel so their raw events don't leak to the frontend;
    /// the child collector selectively re-emits relevant
    /// events (e.g. TextDelta with delegation metadata) on the team channel.
    fn event_agent_id(&self, agent_id: &str) -> String {
        match self {
            RunScope::Standalone => agent_id.to_string(),
            RunScope::Team { team_id, context_override } => {
                if context_override.is_some() {
                    // Child agent: use a hidden agent_id so events don't leak to SSE
                    format!("team:{}:child:{}", team_id, agent_id)
                } else {
                    format!("team:{}", team_id)
                }
            }
            // Agent-owned tasklist runs emit on a dedicated `tasklist:{id}` channel
            // so subagent stdout never leaks into the parent chat stream.
            // Team-owned tasklist runs continue to emit on `team:{team_id}`.
            RunScope::Tasklist { scope, tasklist_id, .. } => match scope {
                TasklistScope::Team(team_id) => format!("team:{}", team_id),
                TasklistScope::Agent(_) => format!("tasklist:{}", tasklist_id),
            },
            RunScope::Project { project_id } => format!("project:{}", project_id),
        }
    }

    /// The key to use for instance registry (register/unregister).
    fn registry_key(&self, agent_id: &str) -> String {
        match self {
            RunScope::Standalone => agent_id.to_string(),
            RunScope::Team { team_id, .. } => format!("team:{}:{}", team_id, agent_id),
            RunScope::Tasklist { tasklist_id, .. } => {
                format!("tasklist:{}:{}", tasklist_id, agent_id)
            }
            RunScope::Project { project_id } => format!("project:{}:{}", project_id, agent_id),
        }
    }

    /// Whether this is a team-scoped run (skip personal transcript and snapshot).
    fn is_team(&self) -> bool {
        matches!(self, RunScope::Team { .. })
    }

    /// Whether this is a tasklist-scoped run.
    fn is_tasklist(&self) -> bool {
        matches!(self, RunScope::Tasklist { .. })
    }

    /// Whether this is a project-scoped run (skip personal transcript and snapshot).
    fn is_project(&self) -> bool {
        matches!(self, RunScope::Project { .. })
    }

    /// Whether a run in this scope must NOT update the agent's visible
    /// chat-list snapshot (last_message / last_activity_at /
    /// last_agent_activity_at / message_count) in `TimelineAdapter::persist_pending`.
    ///
    /// True for team/tasklist/project scope (per the doc comments above —
    /// their content never surfaces in the agent's own chat thread) and for
    /// an isolated delegate child (`isolate_history`), regardless of scope.
    /// False for `Standalone`, including a `Standalone` run scoped to a
    /// secondary (non-default) thread: that content IS the agent's own
    /// visible chat, on a thread other than the default one, so the preview
    /// must still track it. Deliberately NOT derived from whether the run
    /// happens to write its transcript through an override path — a
    /// secondary thread also does that, for unrelated file-routing reasons
    /// (see `bg_transcript_override` / `thread_transcript_override`).
    pub(crate) fn suppresses_visible_snapshot(&self, isolate_history: bool) -> bool {
        self.is_team() || self.is_tasklist() || self.is_project() || isolate_history
    }
}

/// Returns true when the given command would simply echo its argv to stdout —
/// `echo` and `printf` being the obvious offenders. We use this in
/// `build_argv` to strip the system prompt from argv when the user has
/// configured one of these as the agent's command, so private instructions
/// can't be exfiltrated by sending the agent any message.
///
/// We compare on basename so `/bin/echo`, `/usr/bin/echo`, etc. are caught.
/// This is intentionally a small list — `cat`, `tee`, etc. take file paths
/// rather than literal text and aren't used as the default test command, so
/// they're not worth the risk of false positives. Add to the list if a new
/// foot-gun shows up.
fn is_leak_prone_command(command: &str) -> bool {
    let basename = std::path::Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    matches!(basename, "echo" | "printf")
}

/// True if the configured CLI command's basename matches `expected`, after
/// stripping any directory prefix. Lets us special-case `claude`-specific
/// flags without breaking when the user has pinned an absolute path to the
/// binary (e.g. `/Users/x/.nvm/versions/node/v22/bin/claude`).
fn matches_command_basename(command: &str, expected: &str) -> bool {
    let basename = std::path::Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    basename == expected
}

/// Build the `-c` overrides that register the launchpad MCP server with Codex.
///
/// Verified against `codex-cli 0.145.0`: `codex mcp add <name> --url <url>`
/// (the CLI's own first-class command for this) writes a flat
/// `mcp_servers.<name>.url = "<url>"` table with no `type`/`transport` field
/// and no separate `enabled` flag, and that same key parses cleanly as a
/// `-c` override with no experimental feature gate required. Other Codex
/// releases have shipped a typed `transport = { type = "streamable_http", ... }`
/// table instead (sometimes gated behind an experimental flag) — if this ever
/// starts getting silently ignored or rejected, re-check `codex mcp add --help`
/// against the running binary's version before assuming this shape still holds.
fn codex_mcp_server_config_overrides(server_name: &str, url: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        format!(r#"mcp_servers.{server_name}.url="{url}""#),
    ]
}

/// Merge the launchpad MCP server entry into `<cwd>/.cursor/mcp.json`.
///
/// Verified against `cursor-agent 2026.03.25`: it has no `--mcp-config` flag
/// (absent from `--help` and from every option table in the installed
/// bundle) and unrecognized flags are rejected by its commander-based parser.
/// Instead it discovers MCP servers implicitly at startup by reading
/// `.cursor/mcp.json` from its workspace directory (which defaults to the
/// spawned process's cwd — `cursor-agent mcp list` reports servers as
/// "expected in .cursor/mcp.json or ~/.cursor/mcp.json") and merging it with
/// the global `~/.cursor/mcp.json`. The per-server shape only needs a `url`
/// key — the CLI infers the "streamableHttp" transport from its presence, so
/// unlike Claude/Codex's config no `type` field is required (one was
/// confirmed accepted-but-unnecessary via a live `cursor-agent mcp list`
/// against a hand-written file). A server registered this way still needs
/// approval past the CLI's allowlist gate; the cursor profile's base args
/// already carry `--approve-mcps` for that.
///
/// Reads and merges rather than overwriting outright, so a project's own
/// `.cursor/mcp.json` (real Cursor IDE servers) survives. Note this file is a
/// real path inside the user's project, not an ephemeral per-session temp
/// file like Claude/Codex get — cursor-agent has no per-invocation config
/// override, so the `launchpad` entry here is shared across concurrent
/// cursor-agent spawns in the same cwd and is left behind (pointing at the
/// most recently spawned session's URL) after a run ends.
fn merge_cursor_mcp_config(cwd: &std::path::Path, mcp_url: &str) -> std::io::Result<()> {
    let cursor_dir = cwd.join(".cursor");
    std::fs::create_dir_all(&cursor_dir)?;
    let config_path = cursor_dir.join("mcp.json");

    let mut root: serde_json::Value = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));

    let servers = root
        .as_object_mut()
        .expect("root is always an object: constructed or filtered above")
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }
    servers
        .as_object_mut()
        .expect("just normalized to an object above")
        .insert("launchpad".to_string(), serde_json::json!({ "url": mcp_url }));

    let json_str = serde_json::to_string_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&config_path, json_str)
}

/// `agy` (Google Antigravity) has no per-project or workspace MCP config
/// override at all — it reads exactly one file, for every session on the
/// machine, at `~/.gemini/config/mcp_config.json`.
/// `~/.gemini/antigravity/mcp_config.json` is a symlink to this same path;
/// resolving off `home_dir` and writing through the canonical location keeps
/// both aliases in sync without ever touching the symlink itself.
fn agy_global_mcp_config_path(home_dir: &std::path::Path) -> std::path::PathBuf {
    home_dir.join(".gemini").join("config").join("mcp_config.json")
}

/// Merge the launchpad MCP server entry into agy's global MCP config file.
///
/// `agy` has no `--mcp-config` flag and, unlike cursor-agent, no per-project
/// file either — the single global file this writes to is shared by every
/// agy session on the machine, so this must read-merge-write rather than
/// overwrite: any of the user's own `mcpServers` entries have to survive.
///
/// Schema note: agy infers a server's transport from which keys are present
/// rather than from an explicit `type` field, and its key for a remote HTTP
/// server is `url`. Using the wrong key leaves the entry silently unrecognized
/// — agy makes zero outbound connection attempts for it.
///
/// Thin wrapper around [`merge_agy_mcp_config_at`] bound to the real home
/// directory; the split lets tests point the merge at a temp directory
/// instead of the caller's real `~/.gemini`.
fn merge_agy_mcp_config(mcp_url: &str) -> std::io::Result<()> {
    let home_dir = dirs::home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine home directory for agy MCP config",
        )
    })?;
    merge_agy_mcp_config_at(&home_dir, mcp_url)
}

fn merge_agy_mcp_config_at(home_dir: &std::path::Path, mcp_url: &str) -> std::io::Result<()> {
    let config_path = agy_global_mcp_config_path(home_dir);
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut root: serde_json::Value = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));

    let servers = root
        .as_object_mut()
        .expect("root is always an object: constructed or filtered above")
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }
    servers
        .as_object_mut()
        .expect("just normalized to an object above")
        .insert(
            "launchpad".to_string(),
            serde_json::json!({ "url": mcp_url }),
        );

    let json_str = serde_json::to_string_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&config_path, json_str)
}



/// Orchestrates a single agent run: process spawning, output normalization,
/// event emission, and transcript persistence.
pub struct CliAgentRunner {
    pub process_supervisor: Arc<dyn ProcessSupervisor>,
    pub normalizer_registry: Arc<NormalizerRegistry>,
    pub event_bus: Arc<EventBus>,
    pub persistence: Arc<PersistenceLayer>,
    pub command_queue: Arc<CommandQueue>,
    pub instance_registry: Arc<InstanceRegistry>,
    pub workflow_runner: Option<Arc<WorkflowRunner>>,
    pub workflow_registry: Option<Arc<RwLock<WorkflowRegistry>>>,
    pub workflow_queue: Option<WorkflowQueueHandle>,
    pub context_cache: Option<Arc<ContextCache>>,
    /// Shared cache of global plugin skills/rules. Parsed once at startup and
    /// on every install/uninstall/refresh; filtered per-agent via the agent's
    /// `enabled_plugins` map on each message turn.
    pub plugin_cache: Option<Arc<PluginCache>>,
    /// Late-bound TaskFeeder so the agent_runner can notify it when a tasklist
    /// task tag (`<task action="complete|fail">`) terminates the assigned task.
    /// Late-bound because TaskFeeder depends on QueueManagerRegistry, which
    /// itself depends on AgentRunner — see [`set_task_feeder`].
    task_feeder: Arc<OnceLock<Arc<TaskFeeder>>>,
    /// Late-bound notification dispatcher so the parse-success path
    /// (`<task action="complete|fail">` + `<task-item-notification>`) can
    /// route the formatted notification XML to a task's `remind_me` agent
    /// via the existing mailbox pipeline. Stored as a trait object (rather
    /// than the concrete `Arc<QueueManagerRegistry>`) so the
    /// `AgentRunner ↔ QueueManagerRegistry` reference cycle doesn't deadlock
    /// Rust's `Send` auto-inference — same trick `TaskFeeder.dispatcher`
    /// uses with `Arc<dyn TaskDispatcher>`. Set post-construction via
    /// [`set_notification_dispatcher`] in `state.rs::AppState`.
    notification_dispatcher:
        Arc<OnceLock<Arc<dyn crate::queue_manager::NotificationDispatcher>>>,
    /// Stores cancel senders keyed by run_id, enabling external cancellation of active runs.
    cancel_senders: Arc<RwLock<HashMap<String, oneshot::Sender<TerminationReason>>>>,
    /// Shared in-flight run registry. Updated on every run entry/exit so the
    /// cancel HTTP route can fire the right token regardless of runner mode.
    running_agents: Arc<RunningAgents>,
    /// IoTool/EngineTool registry used to assemble the XML catalog appended to
    /// CLI-mode system prompts. The same shared Arc<Registry> that NativeAgentRunner uses.
    pub tools_registry: Arc<Registry>,
    /// Runtime anchor registry for cache-floor stability across CLI turns.
    pub anchor_registry: Arc<WindowAnchorRegistry>,
    /// Reflection-trigger subscriber for the OBSERVE pass. Defaults to a no-op; late-bound to a real subscriber via
    /// [`Self::with_reflection_subscriber`] once the reflection pass exists.
    pub reflection_subscriber: Arc<dyn ReflectionTriggerSubscriber>,
    /// Per-agent live context map shared with the MCP HTTP route handler.
    pub mcp_sessions: Arc<McpSessionStore>,
}

impl CliAgentRunner {
    pub fn new(
        process_supervisor: Arc<dyn ProcessSupervisor>,
        normalizer_registry: Arc<NormalizerRegistry>,
        event_bus: Arc<EventBus>,
        persistence: Arc<PersistenceLayer>,
        command_queue: Arc<CommandQueue>,
        instance_registry: Arc<InstanceRegistry>,
        running_agents: Arc<RunningAgents>,
        tools_registry: Arc<Registry>,
    ) -> Self {
        Self {
            process_supervisor,
            normalizer_registry,
            event_bus,
            persistence,
            command_queue,
            instance_registry,
            workflow_runner: None,
            workflow_registry: None,
            workflow_queue: None,
            context_cache: None,
            plugin_cache: None,
            task_feeder: Arc::new(OnceLock::new()),
            notification_dispatcher: Arc::new(OnceLock::new()),
            cancel_senders: Arc::new(RwLock::new(HashMap::new())),
            running_agents,
            tools_registry,
            anchor_registry: Arc::new(WindowAnchorRegistry::new()),
            reflection_subscriber: Arc::new(NoopReflectionSubscriber),
            mcp_sessions: Arc::new(McpSessionStore::new()),
        }
    }

    /// Replace the anchor registry (used by AppState to share one registry across runners).
    pub fn with_anchor_registry(mut self, registry: Arc<WindowAnchorRegistry>) -> Self {
        self.anchor_registry = registry;
        self
    }

    /// Supply a reflection-trigger subscriber (used by AppState once the
    /// reflection pass exists to receive `select`'s anchor-rotation and
    /// idle-timeout triggers).
    pub fn with_reflection_subscriber(
        mut self,
        subscriber: Arc<dyn ReflectionTriggerSubscriber>,
    ) -> Self {
        self.reflection_subscriber = subscriber;
        self
    }

    /// Share the McpSessionStore with this runner (used by AppState to share one store across
    /// the runner and the MCP HTTP route handler).
    pub fn with_mcp_sessions(mut self, sessions: Arc<McpSessionStore>) -> Self {
        self.mcp_sessions = sessions;
        self
    }

    /// Late-bind the TaskFeeder so this runner can call `on_task_terminal`
    /// when an agent emits `<task action="complete|fail">`. Idempotent: a
    /// second call after the first is silently ignored.
    pub fn set_task_feeder(&self, feeder: Arc<TaskFeeder>) {
        let _ = self.task_feeder.set(feeder);
    }

    /// Late-bind the [`crate::queue_manager::NotificationDispatcher`] so the
    /// parse-success path can submit a formatted `<task-item-notification>`
    /// QueuedMessage to a task's `remind_me` agent. Idempotent: a second call
    /// is silently ignored. If never bound, the parse-success path skips the
    /// dispatch with a warning (the changelog append still runs).
    pub fn set_notification_dispatcher(
        &self,
        dispatcher: Arc<dyn crate::queue_manager::NotificationDispatcher>,
    ) {
        let _ = self.notification_dispatcher.set(dispatcher);
    }

    pub fn with_workflow_runner(mut self, workflow_runner: Arc<WorkflowRunner>) -> Self {
        self.workflow_runner = Some(workflow_runner);
        self
    }

    pub fn with_workflow_registry(mut self, registry: Arc<RwLock<WorkflowRegistry>>) -> Self {
        self.workflow_registry = Some(registry);
        self
    }

    pub fn with_workflow_queue(mut self, queue: WorkflowQueueHandle) -> Self {
        self.workflow_queue = Some(queue);
        self
    }

    pub fn with_context_cache(mut self, cache: Arc<ContextCache>) -> Self {
        self.context_cache = Some(cache);
        self
    }

    pub fn with_plugin_cache(mut self, cache: Arc<PluginCache>) -> Self {
        self.plugin_cache = Some(cache);
        self
    }

    /// Register a cancel sender for a run, enabling external cancellation.
    pub async fn register_cancel_sender(
        &self,
        run_id: &str,
        cancel_tx: oneshot::Sender<TerminationReason>,
    ) {
        self.cancel_senders
            .write()
            .await
            .insert(run_id.to_string(), cancel_tx);
    }

    /// Remove a cancel sender (called when run completes naturally).
    pub async fn unregister_cancel_sender(&self, run_id: &str) {
        self.cancel_senders.write().await.remove(run_id);
    }

    /// Cancel an active run by sending TerminationReason::Cancelled via its cancel sender.
    /// Returns true if the cancel signal was sent, false if no active run was found.
    pub async fn cancel_run(&self, run_id: &str) -> bool {
        if let Some(cancel_tx) = self.cancel_senders.write().await.remove(run_id) {
            let _ = cancel_tx.send(TerminationReason::Cancelled);
            true
        } else {
            false
        }
    }

    /// Mint a new session_id (UUID), write a per-spawn
    /// `{data_root}/agents/{agent_id}/mcp-{session_id}.json` with the session_id embedded
    /// in the MCP URL, and register a fresh McpAgentSession keyed by that session_id.
    /// Each invocation gets an isolated file and session entry so concurrent spawns of the
    /// same profile no longer share state or clobber each other's config file.
    /// The caller must deregister the session via `McpSessionGuard` when the subprocess exits.
    pub async fn prepare_mcp_session(
        &self,
        agent_id: &str,
        cwd: std::path::PathBuf,
        floor_ts: chrono::DateTime<chrono::Utc>,
    ) -> Result<String, AoError> {
        let (session_id, _mcp_url) = self
            .prepare_mcp_session_with_chains(agent_id, cwd, floor_ts, vec![], vec![], None, None)
            .await?;
        Ok(session_id)
    }

    /// Like [`prepare_mcp_session`] but stores delegation chain metadata in
    /// the session so the MCP route handler can propagate it into every
    /// `RunnerContext` it builds for tool calls within this session.
    ///
    /// Returns `(session_id, mcp_url)` — the URL is handed back so callers
    /// that can't take a `--mcp-config <path>` flag (e.g. Codex) can inject
    /// the same server via their own config surface instead.
    async fn prepare_mcp_session_with_chains(
        &self,
        agent_id: &str,
        cwd: std::path::PathBuf,
        floor_ts: chrono::DateTime<chrono::Utc>,
        delegate_chain: Vec<String>,
        spawn_chain: Vec<String>,
        project_id: Option<String>,
        thread_id: Option<String>,
    ) -> Result<(String, String), AoError> {
        let session_id = Uuid::new_v4().to_string();

        let mcp_json_path = self
            .persistence
            .data_root
            .agents_dir()
            .join(agent_id)
            .join(format!("mcp-{}.json", session_id));

        // Write a per-session config file so concurrent invocations of the same agent
        // profile each point their Claude CLI at their own isolated session URL.
        let port: u16 = std::env::var("AO_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3001);
        let mcp_url = format!("http://localhost:{}/mcp/{}/{}", port, agent_id, session_id);
        let content = serde_json::json!({
            "mcpServers": {
                "launchpad": {
                    "type": "http",
                    "url": &mcp_url
                }
            }
        });
        let json_str = serde_json::to_string_pretty(&content)
            .map_err(|e| AoError::Json(e.to_string()))?;
        if let Some(parent) = mcp_json_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&mcp_json_path, json_str).await?;

        // Sidecar metadata so the server can resurrect this session if a
        // restart wipes the in-memory session store while the spawned CLI
        // subprocess (and any suspended tool call in it) is still alive.
        // The config file above is consumed verbatim by the CLI as its MCP
        // config, so resurrection state must live in a separate file. Both
        // files are deleted together by `McpSessionGuard` on subprocess exit.
        // Write failure is non-fatal: resurrection degrades, the spawn works.
        let meta_path = mcp_json_path.with_extension("meta.json");
        let meta = serde_json::json!({
            "agentId": agent_id,
            "cwd": cwd.to_string_lossy(),
            "delegateChain": &delegate_chain,
            "spawnChain": &spawn_chain,
            "projectId": &project_id,
            "threadId": &thread_id,
            "floorTs": floor_ts.to_rfc3339(),
        });
        if let Err(e) = tokio::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        )
        .await
        {
            tracing::warn!(
                agent_id = %agent_id,
                session_id = %session_id,
                "failed to write MCP session metadata sidecar: {e}"
            );
        }

        self.mcp_sessions
            .register_session_with_chains(
                session_id.clone(),
                agent_id.to_string(),
                cwd,
                None,
                delegate_chain,
                spawn_chain,
                project_id,
                thread_id,
            )
            .map_err(|_| AoError::Internal("failed to register MCP session".to_string()))?;
        self.mcp_sessions.update_floor(&session_id, floor_ts).await;

        Ok((session_id, mcp_url))
    }

    /// Build the CLI argv from an AgentProfile and user prompt.
    ///
    /// Note: also enforces a small security guard — see `is_leak_prone_command`
    /// below. Commands whose basename is `echo` or `printf` will simply print
    /// their args to stdout, which becomes the agent's reply; passing the
    /// system prompt to such a command would expose it to the user.
    pub fn build_argv(
        agent: &AgentProfile,
        prompt: &str,
        mcp_config_path: Option<&std::path::Path>,
        mcp_server_url: Option<&str>,
    ) -> Vec<String> {
        let ProviderConfig::Cli(ref cli) = agent.provider;

        let mut argv = vec![cli.command.clone()];

        // Append base args
        argv.extend(cli.args.iter().cloned());

        // Add model flag if model_arg and model are present
        if let (Some(ref model_arg), Some(ref model)) = (&cli.model_arg, &agent.model) {
            // Resolve model aliases
            let resolved_model = cli
                .model_aliases
                .get(model)
                .cloned()
                .unwrap_or_else(|| model.clone());
            tracing::info!(
                agent_id = %agent.id,
                model_key = %model,
                resolved_model = %resolved_model,
                model_arg = %model_arg,
                "Resolved model alias for CLI argv"
            );
            argv.push(model_arg.clone());
            argv.push(resolved_model);
        }

        // Reasoning channel flags. Only the Claude CLI accepts these as
        // bare command-line flags today; other providers express the same
        // concept via API parameters and will need their own mapping when
        // they grow a CLI-runner-backed path. Keying off the basename of
        // the configured command keeps the mapping localized — adding e.g.
        // `gemini` support is a single match-arm here, not a refactor of
        // the profile schema.
        if let Some(ref thinking) = agent.thinking {
            if matches_command_basename(&cli.command, "claude") {
                let mode_flag = match thinking.mode {
                    ThinkingMode::Adaptive => Some("adaptive"),
                    // Mode::Disabled = caller wants thinking off entirely.
                    // The Claude CLI doesn't expose an explicit "off" value
                    // for the `--thinking` flag — its absence is the off
                    // state. Skip the flag set entirely in that case so we
                    // don't ship a no-op argument to the binary.
                    ThinkingMode::Disabled => None,
                };
                if let Some(mode_value) = mode_flag {
                    argv.push("--thinking".to_string());
                    argv.push(mode_value.to_string());
                    argv.push("--thinking-display".to_string());
                    argv.push(
                        match thinking.display {
                            ThinkingDisplay::Summarized => "summarized",
                            ThinkingDisplay::Raw => "raw",
                            ThinkingDisplay::Omitted => "omitted",
                        }
                        .to_string(),
                    );
                    if let Some(budget) = thinking.budget_tokens {
                        argv.push("--max-thinking-tokens".to_string());
                        argv.push(budget.to_string());
                    }
                }
            }
        }

        // Advertise MCP server config so the CLI discovers custom tools via MCP.
        // Codex has no `--mcp-config` flag (it hard-errors on unrecognized
        // arguments), so it needs the launchpad server injected through its own
        // `-c` config-override surface instead of the JSON file path other
        // CLIs take. cursor-agent and agy likewise take no MCP-related argv at
        // all here — each reads its own workspace-implicit JSON file instead
        // (`.cursor/mcp.json` / `mcp_config.json`), written by the caller via
        // `merge_cursor_mcp_config` / `merge_agy_mcp_config` once it knows the
        // spawn's cwd (see `run_with_scope_inner`).
        if matches_command_basename(&cli.command, "codex") {
            if let Some(url) = mcp_server_url {
                argv.extend(codex_mcp_server_config_overrides("launchpad", url));
                // Codex has no TTY in this headless spawn, so any approval
                // gate on MCP tool calls has no one to answer it and each
                // call is auto-denied before it reaches the server —
                // discovery (tools/list) still succeeds, but every
                // tools/call silently fails. `-c approval_policy="never"`
                // is Codex's own config key for disabling that gate
                // entirely; it composes cleanly with the base `--sandbox
                // workspace-write` arg above, which governs local shell/file
                // access and is orthogonal to MCP call approval.
                argv.push("-c".to_string());
                argv.push(r#"approval_policy="never""#.to_string());
            }
        } else if matches_command_basename(&cli.command, "cursor-agent")
            || matches_command_basename(&cli.command, "agy")
        {
            // No argv flag — see comment above.
        } else if let Some(mcp_path) = mcp_config_path {
            argv.push("--mcp-config".to_string());
            argv.push(mcp_path.to_string_lossy().into_owned());
        }

        // SECURITY: If the configured command is a generic "spit-out" utility
        // like `echo` or `printf`, anything we pass as args (including the
        // agent's system prompt) gets reflected straight back as the agent's
        // reply, exposing private instructions to the user. Detect by basename
        // and strip the system prompt from argv in that case. The default
        // template uses `echo` for testing with no system prompt set, so this
        // only triggers when a user has both (a) entered a leak-prone command
        // in Advanced settings and (b) configured a system prompt.
        let leak_prone = is_leak_prone_command(&cli.command);
        if leak_prone && agent.system_prompt.is_some() {
            tracing::warn!(
                agent_id = %agent.id,
                command = %cli.command,
                "Stripping system prompt from argv: command is a leak-prone utility (echo/printf) that would echo it back to the user. Change the command in Advanced settings to a real CLI to enable system prompts."
            );
        }

        // Add system prompt flag if present (skipped when command is leak-prone)
        if !leak_prone {
            if let (Some(ref sp_arg), Some(ref sp)) = (&cli.system_prompt_arg, &agent.system_prompt)
            {
                argv.push(sp_arg.clone());
                argv.push(sp.clone());
            }
        }

        // Add prompt as final arg if InputMode::Arg
        if cli.input_mode == InputMode::Arg {
            // agy takes its prompt as `-p <value>` rather than a bare
            // trailing positional (confirmed invocation:
            // `agy --dangerously-skip-permissions --model "<model>" -p "<prompt>"`).
            // Standard getopt-style parsers only require a flag's value to
            // immediately follow it — not that the flag itself be the very
            // last token before it — so pushing `-p` here and letting
            // `--dangerously-skip-permissions`/`--model` sit earlier in argv
            // is equivalent to the confirmed sample's literal order.
            if matches_command_basename(&cli.command, "agy") {
                argv.push("-p".to_string());
            }
            // If there's a system prompt but no system_prompt_arg flag,
            // prepend it to the user message so the CLI still receives it.
            // (Also skipped when command is leak-prone — see comment above.)
            if !leak_prone && cli.system_prompt_arg.is_none() {
                if let Some(ref sp) = agent.system_prompt {
                    argv.push(format!(
                        "[System Instructions]\n{}\n[End System Instructions]\n\n{}",
                        sp, prompt
                    ));
                } else {
                    argv.push(prompt.to_string());
                }
            } else {
                argv.push(prompt.to_string());
            }
        }

        argv
    }

    /// Execute an agent run. Returns the run_id immediately; execution happens
    /// in a spawned background task.
    ///
    /// `run_complete_tx` is notified with the run_id and accumulated output text when the run finishes.
    pub async fn run(
        self: &Arc<Self>,
        agent: &AgentProfile,
        prompt: &str,
        attachments: &[Attachment],
        run_complete_tx: mpsc::Sender<RunComplete>,
        focus_path: Option<&str>,
    ) -> Result<String, AoError> {
        self.run_with_scope(agent, prompt, attachments, run_complete_tx, RunScope::Standalone, focus_path)
            .await
    }

    /// Execute an agent run with a specified scope. Team-scoped runs use different
    /// registry keys and event agent IDs, skip personal transcript read/write,
    /// and skip agent snapshot updates.
    pub async fn run_with_scope(
        self: &Arc<Self>,
        agent: &AgentProfile,
        prompt: &str,
        attachments: &[Attachment],
        run_complete_tx: mpsc::Sender<RunComplete>,
        scope: RunScope,
        focus_path: Option<&str>,
    ) -> Result<String, AoError> {
        self.run_with_scope_inner(agent, prompt, attachments, run_complete_tx, scope, focus_path, None, None, vec![], vec![], false, None, None, false)
            .await
    }

    /// Internal variant of [`run_with_scope`] that accepts an optional
    /// pre-allocated `run_id` and delegation chain metadata. When
    /// `pre_registered_run_id` is supplied, the runner adopts it as-is
    /// and assumes the caller has already booked the slot in
    /// [`InstanceRegistry`] (the slot is bookkeeping, not a lock — see
    /// `AgentRunRequest::pre_registered_run_id` for why). When `None`, a
    /// fresh UUID is minted and the runner registers it itself.
    ///
    /// `delegate_chain` and `spawn_chain` are stored in the per-session
    /// `McpAgentSession` so the MCP route handler can propagate them into
    /// every `RunnerContext` it builds for tool calls in this session.
    ///
    /// `transcript_override` and `event_channel` carry the delegated-child
    /// isolation contract (see `AgentRunRequest`): when set, transcript
    /// writes land in the override file and live bus events ride the given
    /// channel instead of the agent's own — critical for clone-parent
    /// delegates, whose agent_id IS the parent's.
    pub(crate) async fn run_with_scope_inner(
        self: &Arc<Self>,
        agent: &AgentProfile,
        prompt: &str,
        attachments: &[Attachment],
        run_complete_tx: mpsc::Sender<RunComplete>,
        scope: RunScope,
        focus_path: Option<&str>,
        pre_registered_run_id: Option<String>,
        thread_id: Option<String>,
        delegate_chain: Vec<String>,
        spawn_chain: Vec<String>,
        isolate_history: bool,
        transcript_override: Option<std::path::PathBuf>,
        event_channel: Option<String>,
        bypass_instance_cap: bool,
    ) -> Result<String, AoError> {
        // Adopt the caller's pre-allocated run_id if provided, else mint
        // a fresh one. The pre-allocated path is taken by the per-agent
        // queue manager to close a TOCTOU window in its `can_spawn`
        // check — see `AgentRunRequest::pre_registered_run_id`.
        let caller_pre_registered = pre_registered_run_id.is_some();
        let run_id = pre_registered_run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let agent_id = agent.id.clone();
        // Delegated children supply a dedicated channel (e.g.
        // `delegate:<delegation_id>`) so their streaming output never renders
        // in an agent's live chat feed; otherwise the scope decides.
        let event_agent_id = event_channel
            .clone()
            .unwrap_or_else(|| scope.event_agent_id(&agent_id));
        let registry_key = scope.registry_key(&agent_id);
        let is_team_scope = scope.is_team();
        let is_tasklist_scope = scope.is_tasklist();
        let is_project_scope = scope.is_project();

        // Resolve the chat thread this turn targets, once, up front. `None`
        // and default-kind rows keep the back-compat agent-keyed transcript
        // path so single-thread agents stay byte-equivalent; fresh/branch
        // threads carry their own `transcript_path`, consulted below for
        // both the history read AND the transcript write override. Cheap to
        // resolve unconditionally — `thread_id` is only ever `Some` for
        // standalone AgentChat-scoped runs, so tasklist/team/project scopes
        // hit the `None` arm without a persistence lookup.
        let thread_metadata: Option<ao_protocol::thread::Thread> = match thread_id.as_deref() {
            Some(id) => self
                .persistence
                .threads
                .get(id)
                .await
                .ok()
                .flatten()
                .filter(|t| t.kind != ao_protocol::thread::ThreadKind::Default),
            None => None,
        };
        let thread_transcript_override: Option<std::path::PathBuf> = thread_metadata
            .as_ref()
            .map(|t| std::path::PathBuf::from(&t.transcript_path));

        // For a branch thread, the first turn should graft pre-fork context
        // from the SOURCE thread's transcript — the branch's own file holds
        // only post-floor turns. `None` for non-branch (fresh/default)
        // threads, in which case `history::select` just reads the thread's
        // own file. Mirrors `native.rs`'s `thread_recall_override`
        // resolution so the CLI runner performs the same TRUE FORK graft
        // instead of relying on RecallHistory, which has no signal to fire
        // on a cold, empty branch window.
        let thread_branch_source_path: Option<std::path::PathBuf> = match thread_metadata
            .as_ref()
            .and_then(|t| t.branch_source.as_ref())
        {
            Some(bs) => self
                .persistence
                .threads
                .get(&bs.source_thread_id)
                .await
                .ok()
                .flatten()
                .map(|src| std::path::PathBuf::from(src.transcript_path)),
            None => None,
        };

        // Resolve where this run's transcript writes go when they must NOT
        // touch the agent's personal transcript file:
        // - Delegated children pass an explicit override (the delegate
        //   sidechain file) via the run request — it takes precedence.
        // - Tasklist runs resolve a per-tasklist/per-task file from scope.
        // - Standalone runs on a fresh/branch thread write to that thread's
        //   own file (mirrors the read-side `thread_metadata` resolution
        //   above) so the agent's reply lands in the same thread the user's
        //   message was sent to, instead of always falling back to the
        //   agent-keyed personal transcript.
        // All transcript reads/writes inside this run are then routed via
        // `*_for_run` helpers, so the personal transcript is never touched.
        let transcript_path_override: Option<std::path::PathBuf> = transcript_override
            .or(match &scope {
                RunScope::Tasklist {
                    scope: ref tl_scope,
                    tasklist_id,
                    task_id,
                } => match tl_scope {
                    TasklistScope::Team(team_id) => Some(
                        self.persistence
                            .data_root
                            .tasklist_agent_transcript_path(team_id, tasklist_id, &agent_id),
                    ),
                    // Per-task transcript at {workspace}/tasks/{task_id}/transcript.jsonl
                    // Writes always land here. Reads fall back to the legacy path when the new file is absent.
                    TasklistScope::Agent(owner_agent_id) => Some(
                        self.persistence
                            .data_root
                            .task_transcript_path(owner_agent_id, tasklist_id, task_id),
                    ),
                },
                _ => None,
            })
            .or_else(|| thread_transcript_override.clone());

        // Back-compat read path: if the new per-task transcript file doesn't exist yet
        // (task created under a pre-Loop-L build), resolve the legacy path for the
        // HistorySource read so context from old runs is preserved. Writes always
        // go to the new path set above; only the initial history read may use the old path.
        let transcript_read_path_override: Option<std::path::PathBuf> =
            if let (RunScope::Tasklist { scope: TasklistScope::Agent(owner_agent_id), tasklist_id, task_id: _ }, Some(ref new_path)) =
                (&scope, &transcript_path_override)
            {
                if !new_path.exists() {
                    let legacy = self
                        .persistence
                        .data_root
                        .agent_tasklist_transcript_path(owner_agent_id, tasklist_id, &agent_id);
                    if legacy.exists() {
                        Some(legacy)
                    } else {
                        transcript_path_override.clone()
                    }
                } else {
                    transcript_path_override.clone()
                }
            } else {
                transcript_path_override.clone()
            };

        // Load tasklist + task + team name for the system-prompt preamble.
        // Done up here so we fail fast (and miss the run) if the tasklist or
        // assigned task no longer exists by the time the agent boots.
        let tasklist_run_ctx: Option<(
            ao_protocol::tasklist::Tasklist,
            ao_protocol::tasklist::Task,
            String,
        )> = if let RunScope::Tasklist {
            scope: ref tl_scope,
            ref tasklist_id,
            ref task_id,
        } = scope
        {
            let (tl, team_name_for_preamble) = match tl_scope {
                TasklistScope::Team(team_id) => {
                    let tl = self
                        .persistence
                        .tasklists
                        .get(team_id, tasklist_id)
                        .await?
                        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
                    // Previously resolved the team's display name for the
                    // preamble; with teams removed there is no profile to
                    // read, so the id is the only label available. This was
                    // already the fallback when the profile was missing.
                    (tl, team_id.clone())
                }
                TasklistScope::Agent(owner_agent_id) => {
                    let tl = self
                        .persistence
                        .tasklists
                        .get_for_agent(owner_agent_id, tasklist_id)
                        .await?
                        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
                    (tl, owner_agent_id.clone())
                }
            };
            let task = tl
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .find(|t| t.id == *task_id)
                .cloned()
                .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;
            Some((tl, task, team_name_for_preamble))
        } else {
            None
        };

        // ─────────────────────────────────────────────────────────────────────
        // CommandQueue per-agent serialization gate — currently DISABLED.
        //
        // Historical purpose
        // ------------------
        // This gate was a per-`agent_id` semaphore (capacity hard-coded to 1)
        // that guaranteed only one vendor CLI process (`claude --print`,
        // `codex`, `cursor`, …) was alive for a given agent identity at a time.
        // It is *separate* from `max_instances` / `PersonalQueueManager`, which
        // gate how many user-message dispatches may overlap. This lower-level
        // gate existed to protect resources that the vendor CLIs assume are
        // single-driver per session — specifically:
        //
        //   The `--resume <session-id>` transcript chain. The Claude CLI (and
        //   peers) append turn-by-turn to
        //   `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Two
        //   concurrent `claude --print --resume <same-sid>` processes would
        //   race on that file and corrupt the agent's own conversation
        //   history.
        //
        // Other resources sometimes cited (vendor's `__store.db`, MCP
        // registrations, settings reads) are either SQLite-WAL safe or
        // per-process isolated — they did NOT actually need this gate.
        //
        // Why it is disabled now
        // ----------------------
        // We do not invoke vendor CLIs with `--resume`. Conversation history
        // is sourced from our own per-scope JSONL transcripts (see
        // `crate::history::select` and the `AnchorKey` mapping below) and
        // injected into each CLI invocation as an augmented prompt. The
        // `.jsonl` file the gate was protecting is therefore never read or
        // appended-to by us in a way that races.
        //
        // The deadlock this caused
        // ------------------------
        // Because the lane key was `agent_id` only, a parent invocation that
        // called `TodoCreate` synchronously would hold the single permit and
        // block its own tasklist subtasks (which run under the *same*
        // `agent_id` but in a tasklist-scoped CWD with their own transcript).
        // Parent waited on subtask completion; subtask waited on the parent's
        // permit. The vendor CLI emitted no stdout during the wait, so the
        // process supervisor's `no_output_timeout_ms` watchdog eventually
        // killed the parent, releasing the permit and letting subtasks
        // finally run — which looked like "the tasklist only finished after
        // the main agent timed out." Teams hit the analogous shape when a
        // coordinator dispatched to itself.
        //
        // If we need to bring this back
        // -----------------------------
        // Re-introduce ONLY if we start using vendor `--resume` continuity (or
        // any other per-session, append-only resource that genuinely cannot
        // tolerate two concurrent drivers). When re-enabling, do NOT key the
        // lane on `agent_id` alone — make it scope-aware so that tasklist
        // subtasks, team coordinator runs, and team child runs each get their
        // own lane. Sketch:
        //
        //     let lane_key = match &scope {
        //         RunScope::Tasklist { tasklist_id, task_id, .. } =>
        //             format!("{agent_id}::tl::{tasklist_id}::{task_id}"),
        //         RunScope::Team { team_id, .. } =>
        //             format!("{agent_id}::team::{team_id}"),
        //         _ => agent_id.clone(),
        //     };
        //
        // Also: pass capacity from agent config (today's `1` is hard-coded in
        // `CommandQueue::acquire`, and `CommandQueue` caches the cap on first
        // insert — so reviving this should fix that too).
        //
        // To re-enable verbatim (NOT recommended without the scope fix above):
        //
        //     let permit = if agent.serialize {
        //         Some(self.command_queue.acquire(&agent_id, 1).await)
        //     } else {
        //         None
        //     };
        //
        // The `agent.serialize` field is kept on the profile struct so a
        // future revival doesn't require a schema/migration change.
        // ─────────────────────────────────────────────────────────────────────
        let permit: Option<tokio::sync::OwnedSemaphorePermit> = None;
        let _ = &self.command_queue; // suppress unused-field lint while gate is disabled
        let _ = agent.serialize; // suppress unused-field lint while gate is disabled

        // Register run in InstanceRegistry (use team-scoped key if applicable).
        //
        // Skipped when the caller pre-registered the slot before the spawn
        // boundary (see `AgentRunRequest::pre_registered_run_id`). The
        // queue manager takes that path to make its `can_spawn` check
        // race-free under `max_instances = 1`; here we honor it and leave
        // cleanup to the caller's RAII guard. The `InstanceRegistryGuard`
        // inside the runner's inner spawn (further down) is idempotent —
        // even if it fires, the second `unregister_run` is a no-op on an
        // already-cleared entry.
        //
        // Also skipped when `bypass_instance_cap` is set (see
        // `AgentRunRequest::bypass_instance_cap`): this run never occupies
        // the agent's slot at all, so it can't contend with — or be
        // mistaken in the UI for — that agent's own live turn. The later
        // `wrap_existing` guard still runs unconditionally; unregistering a
        // key that was never registered is a harmless no-op.
        if !caller_pre_registered && !bypass_instance_cap {
            self.instance_registry
                .register_run_with_thread(&registry_key, &run_id, thread_id.clone())
                .await;
        }

        // Note: user message is already persisted by the route handler (send_message)
        // before it reaches the queue. No need to write it again here.

        // Augment prompt with attachment references (FileReference strategy).
        // The original message is already in the transcript; this augmentation
        // is only for the CLI invocation.
        let file_caps = match &agent.provider {
            ProviderConfig::Cli(cli) => cli.file_capabilities.as_ref(),
        };
        let prompt = &augment_prompt_with_attachments(prompt, attachments, file_caps);

        // Emit RunStarted eagerly — typing indicator shows immediately.
        self.event_bus
            .emit(&run_id, &event_agent_id, thread_id.clone(), AgentEventPayload::RunStarted)
            .await;

        // Mark this task's run as observed-alive for the dispatch watchdog.
        // Now that the run is registered in the InstanceRegistry above, the
        // watchdog can tell a run that started then vanished (genuine drop →
        // recover at once) apart from one that simply has not started yet
        // (cold start → keep honoring the dispatch grace window). Keyed by the
        // exact task, so a lingering sibling run can never mark a still-starting
        // task as observed.
        if let RunScope::Tasklist {
            tasklist_id, task_id, ..
        } = &scope
        {
            if let Some(feeder) = self.task_feeder.get() {
                feeder.mark_run_observed(tasklist_id, task_id).await;
            }
        }

        // Load conversation history and build augmented prompt via shared history::select.
        // RunScope → AnchorKey mapping:
        //   Tasklist              → TasklistPath(path)
        //   Team coordinator      → TeamShared(team_id)
        //   Team child/delegate   → TeamPerAgent(team_id, agent_id)
        //   Standalone            → Personal(agent_id)
        let augmented_prompt = if is_tasklist_scope {
            // Use the back-compat read path here: for new tasks this equals the write path;
            // for tasks created before Loop L, this falls back to the legacy transcript location.
            let path = transcript_read_path_override
                .as_deref()
                .expect("tasklist scope sets transcript_read_path_override");
            let (entries, anchor_signal) = crate::history::select(
                &self.persistence,
                crate::history::HistorySelectInput {
                    source: crate::history::HistorySource::TasklistPath {
                        path: path.to_path_buf(),
                    },
                    current_message_already_persisted: false,
                    now: Utc::now(),
                    config: ContextConfig::default(),
                    anchor_registry: Some(Arc::clone(&self.anchor_registry)),
                    reflection_subscriber: Some(Arc::clone(&self.reflection_subscriber)),
                },
            )
            .await;
            if let Some(signal) = anchor_signal {
                tracing::debug!(scope = "tasklist", signal = ?signal, "history::select anchor signal");
            }
            build_prompt_with_context(&entries, prompt, &ContextConfig::default())
        } else if is_team_scope {
            // Build base prompt from context_override (coordinator = bare prompt; child = ctx prepended).
            let base_prompt =
                if let RunScope::Team { context_override: Some(ref ctx), .. } = scope {
                    if ctx.is_empty() {
                        prompt.to_string()
                    } else {
                        format!("{}\n\n{}", ctx, prompt)
                    }
                } else {
                    prompt.to_string()
                };

            if let RunScope::Team { ref team_id, ref context_override, .. } = scope {
                // Coordinator reads shared team transcript; child reads per-agent transcript.
                // current_message_already_persisted=true for both (fixes duplicate-last-entry bug).
                let source = if context_override.is_none() {
                    crate::history::HistorySource::TeamShared {
                        team_id: team_id.clone(),
                    }
                } else {
                    crate::history::HistorySource::TeamPerAgent {
                        team_id: team_id.clone(),
                        agent_id: agent_id.clone(),
                    }
                };
                let (entries, anchor_signal) = crate::history::select(
                    &self.persistence,
                    crate::history::HistorySelectInput {
                        source,
                        current_message_already_persisted: true,
                        now: Utc::now(),
                        config: ContextConfig::default(),
                        anchor_registry: Some(Arc::clone(&self.anchor_registry)),
                        reflection_subscriber: Some(Arc::clone(&self.reflection_subscriber)),
                    },
                )
                .await;
                if let Some(signal) = anchor_signal {
                    tracing::debug!(scope = "team", signal = ?signal, "history::select anchor signal");
                }
                build_prompt_with_context(&entries, &base_prompt, &ContextConfig::default())
            } else {
                base_prompt
            }
        } else if is_project_scope {
            if let RunScope::Project { ref project_id } = scope {
                let (entries, anchor_signal) = crate::history::select(
                    &self.persistence,
                    crate::history::HistorySelectInput {
                        source: crate::history::HistorySource::Project {
                            project_id: project_id.clone(),
                        },
                        current_message_already_persisted: true,
                        now: Utc::now(),
                        config: ContextConfig::default(),
                        anchor_registry: Some(Arc::clone(&self.anchor_registry)),
                        reflection_subscriber: Some(Arc::clone(&self.reflection_subscriber)),
                    },
                )
                .await;
                if let Some(signal) = anchor_signal {
                    tracing::debug!(scope = "project", signal = ?signal, "history::select anchor signal");
                }
                build_prompt_with_context(&entries, prompt, &ContextConfig::default())
            } else {
                prompt.to_string()
            }
        } else if isolate_history {
            // History-isolated runs (delegate children) receive the directive as their
            // full context — no personal history is loaded.
            prompt.to_string()
        } else {
            // `thread_metadata` was already resolved up front (alongside
            // `transcript_path_override`); reuse it here rather than
            // re-querying persistence. `None` and default threads stay on
            // the agent-keyed path; fresh/branch threads route through their
            // own JSONL file, and branch threads carry a floor that keeps
            // inherited pre-branch turns out of the live window.
            let history_select_source = match thread_metadata.as_ref() {
                Some(t) => crate::history::HistorySource::PersonalThread {
                    agent_id: agent_id.clone(),
                    transcript_path: std::path::PathBuf::from(&t.transcript_path),
                    // Branch threads graft pre-fork context from the source
                    // thread's transcript (resolved above), matching
                    // native.rs's TRUE FORK behavior for the CLI runner.
                    branch_source_path: thread_branch_source_path.clone(),
                    history_floor_ts: t.history_floor_ts,
                },
                None => crate::history::HistorySource::Personal {
                    agent_id: agent_id.clone(),
                },
            };
            let (entries, anchor_signal) = crate::history::select(
                &self.persistence,
                crate::history::HistorySelectInput {
                    source: history_select_source,
                    current_message_already_persisted: true,
                    now: Utc::now(),
                    config: ContextConfig::default(),
                    anchor_registry: Some(Arc::clone(&self.anchor_registry)),
                    reflection_subscriber: Some(Arc::clone(&self.reflection_subscriber)),
                },
            )
            .await;
            if let Some(signal) = anchor_signal {
                tracing::debug!(scope = "standalone", signal = ?signal, "history::select anchor signal");
            }
            build_prompt_with_context(&entries, prompt, &ContextConfig::default())
        };

        // Load agent and global memories
        // TODO(memory-usage): this is the real per-turn "surfaced" memory set
        // (see the matching note in native.rs's compose_system_prompt
        // inputs). Wire `ao_engine_tools_core::memory_usage::increment` here
        // once a batched per-scope read-modify-write lands — a naive
        // per-entry loop over every scope, every turn, is a needless sidecar
        // rewrite per entry on the message hot path.
        //
        // TODO(outcome-signal): this runner does not route through
        // `ao_engine_tools_runner::query_loop::run_session`, so the
        // `OutcomeRecord` capture wired there (native.rs's sibling call
        // site) does not cover this path yet — no artifact-usage recording
        // happens here today.
        let agent_memories = self
            .persistence
            .memory
            .list(&agent_id)
            .await
            .unwrap_or_default();
        let global_memories = self
            .persistence
            .memory
            .list_global()
            .await
            .unwrap_or_default();
        // Project memories are loaded after effective_cwd is resolved below

        // Load user preferences for system prompt placeholders
        let user_prefs = self
            .persistence
            .preferences
            .get()
            .await
            .unwrap_or(None)
            .unwrap_or_default();

        // Resolve effective_cwd early: focus_path > agent.working_dir > home dir
        // (needed for workspace context loading and later for process spawning)
        let effective_cwd = focus_path
            .map(|p| p.to_string())
            .or_else(|| agent.working_dir.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".to_string())
            });

        // Load project-scoped memories for the effective working directory.
        let (project_memories, resolved_project_key) = {
            let cwd_path = std::path::Path::new(&effective_cwd);
            match ao_persistence::project_key::resolve_project_key(cwd_path).await {
                Ok(canonical_key) => {
                    let hash = ao_persistence::project_key::hash_project_key(&canonical_key);
                    let _ = ao_persistence::project_key::update_projects_index(
                        &self.persistence.data_root,
                        &hash,
                        &canonical_key,
                    ).await;
                    let memories = self.persistence.memory.list_project(&hash).await.unwrap_or_default();
                    (memories, Some(canonical_key))
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        cwd = %effective_cwd,
                        "Failed to resolve project key for memory loading: {}",
                        e
                    );
                    (vec![], None)
                }
            }
        };

        // Load agent home + workspace context using canonical composer loader,
        // with an in-memory cache to avoid re-reading files on every message turn.
        let agent_home = agent.home_dir.as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.persistence.data_root.agent_home_dir(&agent_id));
        let cache_key = ContextCacheKey {
            agent_id: agent_id.clone(),
            effective_cwd: std::path::PathBuf::from(&effective_cwd),
            agent_home: agent_home.clone(),
        };

        // Stat the agent profile file so the cache can detect competency changes
        // (skill toggles, etc.) without waiting for the TTL to expire.
        let profile_path = self.persistence.data_root.agents_dir()
            .join(format!("{}.yaml", agent_id));
        let profile_mtime = tokio::fs::metadata(&profile_path)
            .await
            .and_then(|m| m.modified())
            .ok();

        let cached = if let Some(ref cache) = self.context_cache {
            cache.get(&cache_key, profile_mtime).await
        } else {
            None
        };

        let (mut agent_home_ctx, workspace_ctx) = if let Some(ctx) = cached {
            // Cache hit — skip file I/O
            tracing::info!(
                agent_id = %agent_id,
                agent_home = %agent_home.display(),
                effective_cwd = %effective_cwd,
                cached_skill_count = ctx.agent_home_context.skills.len(),
                cached_rule_count = ctx.agent_home_context.rules.len(),
                "Context cache hit for message turn"
            );
            (ctx.agent_home_context, ctx.workspace_context)
        } else {
            // Cache miss — load from disk via canonical composer loader
            tracing::info!(
                agent_id = %agent_id,
                agent_home = %agent_home.display(),
                effective_cwd = %effective_cwd,
                "Context cache miss — loading from disk"
            );
            if let Err(e) = ao_protocol::agent_home::ensure_agent_home(&agent_home).await {
                tracing::warn!("Failed to scaffold agent home for {}: {}", agent_id, e);
            }
            let (workspace_c, agent_home_c) = tokio::join!(
                crate::system_prompt_composer::loader::load_workspace_context(
                    std::path::Path::new(&effective_cwd)
                ),
                crate::system_prompt_composer::loader::load_agent_home_context(&agent_home),
            );

            // Store in cache alongside profile mtime so the next turn is a hit.
            if let Some(ref cache) = self.context_cache {
                cache.set(cache_key, CachedContext {
                    agent_home_context: agent_home_c.clone(),
                    workspace_context: workspace_c.clone(),
                }, profile_mtime).await;
            }

            (agent_home_c, workspace_c)
        };

        // Merge plugin rules into agent home context.
        // Plugin skills surface via SkillRegistry at dispatch time (not in the system prompt).
        if let Some(ref plugin_cache) = self.plugin_cache {
            let snapshot = plugin_cache.snapshot().await;
            let (_plugin_skills, plugin_rules) = filter_plugins_for_agent(&snapshot, agent);
            let enabled_plugin_keys: Vec<&str> =
                agent.enabled_plugins.keys().map(String::as_str).collect();
            tracing::info!(
                agent_id = %agent_id,
                plugin_rule_count = plugin_rules.len(),
                enabled_plugins = ?enabled_plugin_keys,
                "Merged plugin rules into context"
            );
            for plugin_rule in plugin_rules {
                agent_home_ctx.rules.push(plugin_rule.content.clone());
            }
        }

        // Render the unified "# Studio Skills" listing from the same pools that
        // serve dispatch. In CLI mode `RunSkill` is resolved by the MCP HTTP
        // route (which builds its own registry per request), but the *listing*
        // the model reads comes from this composed prompt — so it must be filled
        // here or the model is never told its enabled pool/plugin skills
        // exist. The CliAgentRunner holds no MCP manager, so
        // the listing covers the user pool + enabled plugins; MCP-prompt-sourced
        // skills remain resolvable via the dispatch route. Done after the cache
        // read so the registry-derived block is recomputed every turn rather than
        // frozen into the context cache. `cli_precedence = true` appends the
        // directive to prefer Studio skills over the host binary's native
        // `Skill` ecosystem on a name collision.
        let skill_registry = crate::agent_context::build_skill_registry(
            self.persistence.data_root.root(),
            agent,
            None,
        );
        agent_home_ctx.skills_block =
            crate::agent_context::render_studio_skills_block(&skill_registry, true);

        // Build workflow summaries for compose_system_prompt (id + name only).
        let workflow_summaries: Vec<WorkflowSummary> = if let Some(ref registry) = self.workflow_registry {
            let reg = registry.read().await;
            match &agent.workflows {
                Some(WorkflowBinding::All) => reg.list_summaries().into_iter().cloned().collect(),
                Some(WorkflowBinding::List(ids)) => ids
                    .iter()
                    .filter_map(|id| reg.get_summary(id))
                    .cloned()
                    .collect(),
                None | Some(WorkflowBinding::None) => vec![],
            }
        } else {
            vec![]
        };

        // Compose the canonical system prompt from pure-data inputs.
        let date_str = Utc::now().format("%Y-%m-%d").to_string();
        let canonical_prompt = crate::system_prompt_composer::compose_system_prompt(
            agent,
            &user_prefs,
            &workspace_ctx,
            &agent_home_ctx,
            &agent_memories,
            &project_memories,
            &global_memories,
            &workflow_summaries,
            &agent.delegates_to,
            &date_str,
            resolved_project_key.as_deref(),
        );

        let mut modified = agent.clone();
        modified.system_prompt = Some(canonical_prompt);

        // Inject the tasklist-mode preamble last so it sits at the very end of
        // the system prompt. Agent profile (skills/memory/workflows) is loaded
        // identically in tasklist mode — only this block is added.
        if let Some((ref tl, ref task, ref team_name)) = tasklist_run_ctx {
            let preamble = crate::tasklist_runtime::build_tasklist_preamble(
                team_name, tl, task,
            );
            let teaches_nested = preamble.contains(&format!(
                r#"<task action="complete" task_id="{}">"#,
                task.id
            )) && preamble.contains("<task-item-notification>");
            let teaches_selfclosing_complete = preamble.contains(&format!(
                r#"<task action="complete" task_id="{}" />"#,
                task.id
            ));
            tracing::debug!(
                target: "ao_engine::tasklist_runtime",
                tasklist_id = %tl.id,
                task_id = %task.id,
                preamble_len = preamble.len(),
                teaches_nested_form = teaches_nested,
                teaches_selfclosing_complete = teaches_selfclosing_complete,
                "tasklist preamble injected into worker system prompt"
            );
            if teaches_selfclosing_complete || !teaches_nested {
                tracing::warn!(
                    target: "ao_engine::tasklist_runtime",
                    tasklist_id = %tl.id,
                    task_id = %task.id,
                    teaches_nested_form = teaches_nested,
                    teaches_selfclosing_complete = teaches_selfclosing_complete,
                    "tasklist preamble does not match expected nested-form contract — \
                     build_tasklist_preamble may be stale or the schema regressed"
                );
            }
            modified.system_prompt = Some(match &modified.system_prompt {
                Some(sp) => format!("{}\n\n{}", sp, preamble),
                None => preamble,
            });
        }

        // Project-scoped runs append the project context block (goal/spec plus
        // the status-dependent role section) after the composed prompt,
        // mirroring the tasklist preamble above. This must happen post-compose:
        // the composer rebuilds the system prompt from the profile's
        // persona/special_instructions fields and discards the legacy
        // `system_prompt` field, so callers cannot inject per-run context by
        // mutating the profile — it would be silently dropped.
        if let RunScope::Project { ref project_id } = scope {
            modified.system_prompt = crate::project_context::append_project_context(
                &self.persistence.projects,
                project_id,
                modified.system_prompt.take(),
            )
            .await;
        }

        let run_agent = modified;

        // Observability: prompt-size log mirrors the NativeAgentRunner site
        // (target `ao_engine::request`). Lets a single `tail | grep` show how
        // big the system prompt is across both runners. The CLI runner doesn't
        // assemble a `messages[]` array here — prior history is rendered into
        // the system prompt by build_prompt_with_context — so the field is
        // omitted; `system_prompt_chars` reflects total request-prefix size.
        let system_prompt_chars = run_agent.system_prompt.as_deref().map(str::len).unwrap_or(0);
        tracing::info!(
            target: "ao_engine::request",
            agent_id = %agent_id,
            run_id = %run_id,
            provider = "cli",
            system_prompt_chars = system_prompt_chars,
            "request prepared",
        );

        // Compute env map once (reused for all continuation iterations)
        let ProviderConfig::Cli(ref cli_config) = agent.provider;
        let mut env_map = agent.env.clone();
        let transcript_env_path = transcript_path_override
            .clone()
            .unwrap_or_else(|| self.persistence.data_root.agent_transcript_path(&agent_id));
        env_map.insert(
            "AGENT_TRANSCRIPT_PATH".to_string(),
            transcript_env_path.to_string_lossy().into_owned(),
        );

        // agy authenticates via the ANTIGRAVITY_API_KEY env var, read from
        // the shared SecretVault — never from a plaintext file, matching how
        // the anthropic/openai/gemini API-mode keys are already vault-only
        // (see `ao_engine_tools_provider_config::SecretVault`). An explicit
        // key already present on the agent's own `env` map (e.g. a user
        // override in Advanced settings) takes priority and is left as-is.
        if matches_command_basename(&cli_config.command, "agy")
            && !env_map.contains_key("ANTIGRAVITY_API_KEY")
        {
            match ao_engine_tools_provider_config::SecretVault::open()
                .and_then(|vault| vault.get_provider("antigravity"))
            {
                Ok(Some(api_key)) => {
                    env_map.insert("ANTIGRAVITY_API_KEY".to_string(), api_key);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "Failed to read antigravity API key from SecretVault: {}",
                        e
                    );
                }
            }
        }

        let bg_env = Some(env_map);

        // Capture CWD and CLI config fields for the continuation loop inside the spawn block
        let bg_cwd = if let Some(override_dir) = focus_path {
            override_dir.to_string()
        } else {
            agent.working_dir.clone().unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".to_string())
            })
        };
        let bg_input_mode = cli_config.input_mode.clone();
        let bg_timeout_ms = agent.timeout_seconds * 1000;
        let bg_no_output_timeout_ms = cli_config.no_output_timeout_ms;
        let bg_cli_config = cli_config.clone();
        // Initial continuation input = augmented user prompt; subsequent iterations
        // receive the formatted <tool_result> blocks from the previous step.
        let bg_initial_prompt = augmented_prompt;

        // Spawn background task to handle stdout, normalization, events, and persistence
        let runner = Arc::clone(self);
        let bg_agent_id = agent_id.clone();
        let bg_event_agent_id = event_agent_id.clone();
        let bg_registry_key = registry_key.clone();
        let _bg_agent_working_dir = agent.working_dir.clone();
        let _bg_focus_path = focus_path.map(|s| s.to_string());
        let bg_is_team = is_team_scope || is_project_scope;
        let bg_is_delegated = isolate_history;
        let bg_is_copilot = agent.template.as_deref() == Some(COPILOT_PROFILE_ID);
        let bg_suppress_visible_snapshot = scope.suppresses_visible_snapshot(isolate_history);
        let bg_transcript_override = transcript_path_override.clone();
        // Threaded into every event this run emits so the frontend can gate
        // live-streaming renders (typing indicator, text deltas, tool-call
        // chips) to the thread the user is actually viewing, instead of the
        // agent's default transcript — mirrors `native.rs`'s `thread_id`
        // capture, which this runner previously lacked (see
        // `run_with_scope_inner`'s `thread_id` param doc).
        let bg_thread_id = thread_id.clone();
        // Request-supplied delegate channel (None for scope-derived channels).
        // Threaded into the TimelineAdapter so its own bus emissions (e.g.
        // HiddenTranscriptEntry) ride the hidden channel too.
        let bg_event_channel = event_channel;
        let bg_run_id = run_id.clone();
        let _bg_agent_home = agent_home.clone();
        // Capture the assigned-task context (team_id, tasklist_id, task_id) so
        // the spawn closure can drive `<task action="...">` state transitions
        // without re-reading scope. Cloned from `tasklist_run_ctx` (which
        // already failed-fast at scope resolve time if either was missing).
        let bg_tasklist_assigned: Option<(ao_protocol::tasklist::TasklistOwner, String, String)> = tasklist_run_ctx
            .as_ref()
            .map(|(tl, task, _)| (tl.owner.clone(), tl.id.clone(), task.id.clone()));
        let bg_delegate_chain = delegate_chain;
        let bg_spawn_chain = spawn_chain;
        let bg_project_id: Option<String> = match &scope {
            RunScope::Project { project_id } => Some(project_id.clone()),
            _ => None,
        };

        // For agent-owned tasklist runs resolve the per-task output.txt path
        // before the spawn move so text chunks can be teed there alongside the
        // existing JSONL transcript without introducing a second event chain.
        let bg_task_output_path: Option<std::path::PathBuf> = match &scope {
            RunScope::Tasklist {
                scope: TasklistScope::Agent(owner_agent_id),
                tasklist_id,
                task_id,
            } => Some(
                self.persistence
                    .data_root
                    .agent_tasklist_task_output_path(owner_agent_id, tasklist_id, task_id),
            ),
            _ => None,
        };

        // Watcher inputs captured before the move so a panic inside the
        // spawned task can still surface an `Error` + `RunEnded(Error)` pair
        // on the event bus. Without this, the queue manager logged a generic
        // "Run completed without result" but the frontend never received
        // `run_ended` — the in-flight bubble lingered until refresh and the
        // stop button stayed wired to a dead run.
        let watcher_event_bus = Arc::clone(&self.event_bus);
        let watcher_run_id = run_id.clone();
        let watcher_agent_id = event_agent_id.clone();
        let watcher_thread_id = thread_id.clone();
        let watcher_registry = Arc::clone(&self.instance_registry);
        let watcher_registry_key = registry_key.clone();

        let inner_handle = tokio::spawn(async move {
            // RAII handle for the InstanceRegistry overlay we registered
            // above (line 846). The register had to land synchronously to
            // avoid a window where SSE re-connect would see no active runs.
            // The Drop guard owns the cleanup so a panic anywhere in this
            // spawned task still unwedges the sidebar overlay.
            let _instance_guard = InstanceRegistryGuard::wrap_existing(
                Arc::clone(&runner.instance_registry),
                bg_registry_key.clone(),
                bg_run_id.clone(),
            );

            // Hold the permit for the duration of the run (released on drop)
            let _permit = permit;

            // Accumulate all emitted TextComplete text for the authoritative output
            let mut final_output = String::new();

            // Accumulate token usage for phase agents
            let mut total_input_tokens: u64 = 0;
            let mut total_output_tokens: u64 = 0;

            // Accumulate workflow follow-ups (next phase contexts to queue)
            let mut workflow_followups: Vec<WorkflowFollowup> = Vec::new();

            // Set when the agent emits a terminal `<task>` action (Complete
            // or Fail) for its assigned task during this run. Suppresses the
            // RunEnded stale-run reprompt since both Complete (via
            // validate_and_complete) and Fail (via on_task_terminal) already
            // drive the next step themselves.
            let mut bg_terminal_task_action_dispatched = false;

            let continuation_input = bg_initial_prompt.clone();
            // Deliberately uninitialized: every `break 'continuation` below must
            // assign it first, and the compiler enforces that. A default here
            // would let a future exit path report a failed run as `Completed`.
            let end_reason;

            // TimelineAdapter accumulates transcript entries across the full
            // chain and flushes them in one persist_pending() call before
            // RunEnded.
            // Team runs skip personal transcripts (no persistence attached).
            // Delegated children must never write into the profile owner's
            // personal transcript — for clone-parent delegates the agent_id IS
            // the parent's. When the caller supplied a sidechain transcript
            // path, route writes there (rich turn-by-turn record); with no
            // path, drop persistence for this run entirely (the spawner's
            // sidechain persister still records the terminal event).
            let timeline_adapter = {
                let base = TimelineAdapter::new(
                    bg_run_id.clone(),
                    bg_agent_id.clone(),
                    bg_thread_id.clone(),
                    Arc::clone(&runner.event_bus),
                )
                .with_event_channel(bg_event_channel.clone());
                if bg_is_team || (bg_is_delegated && bg_transcript_override.is_none()) {
                    base
                } else {
                    let with_persistence = base.with_persistence(
                        Arc::clone(&runner.persistence),
                        bg_transcript_override.clone(),
                    );
                    if bg_suppress_visible_snapshot {
                        with_persistence.suppress_visible_snapshot()
                    } else {
                        with_persistence
                    }
                }
            };

            'continuation: loop {
            // Mint session_id, write per-spawn mcp config, register session.
            // The McpSessionGuard deregisters and cleans up the config file on every exit path
            // (normal return, error break, and panic unwind).
            let step_floor_ts = chrono::Utc::now();
            let (step_session_id, step_mcp_url) = match runner.prepare_mcp_session_with_chains(
                &bg_agent_id,
                std::path::PathBuf::from(&bg_cwd),
                step_floor_ts,
                bg_delegate_chain.clone(),
                bg_spawn_chain.clone(),
                bg_project_id.clone(),
                bg_thread_id.clone(),
            ).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(agent_id = %bg_agent_id, "Failed to prepare MCP session: {}", e);
                    end_reason = RunEndReason::Error;
                    break 'continuation;
                }
            };
            let step_mcp_config = runner
                .persistence
                .data_root
                .agents_dir()
                .join(&bg_agent_id)
                .join(format!("mcp-{}.json", &step_session_id));
            let _mcp_session_guard = McpSessionGuard {
                sessions: Arc::clone(&runner.mcp_sessions),
                // Cloned (not moved) — `step_session_id` is looked up again
                // below to thread this step's `form_suspended` counter into
                // the spawn.
                session_id: step_session_id.clone(),
                mcp_json_path: step_mcp_config.clone(),
            };

            // cursor-agent and agy have no per-invocation MCP config flag (see
            // `merge_cursor_mcp_config` / `merge_agy_mcp_config`); deliver it
            // by writing each one's implicit config file here instead —
            // cursor-agent's is workspace-scoped (the spawn's actual cwd is
            // known at this point), agy's is a single global file with no
            // per-project override. Failure degrades to a tool-less run
            // rather than aborting the spawn — matches how a missing
            // mcp_config_path degrades other providers.
            let ProviderConfig::Cli(ref run_agent_cli) = run_agent.provider;
            if matches_command_basename(&run_agent_cli.command, "cursor-agent") {
                if let Err(e) =
                    merge_cursor_mcp_config(std::path::Path::new(&bg_cwd), &step_mcp_url)
                {
                    tracing::warn!(
                        agent_id = %bg_agent_id,
                        cwd = %bg_cwd,
                        error = %e,
                        "Failed to write cursor-agent MCP config; run will proceed without Launchpad tools"
                    );
                }
            } else if matches_command_basename(&run_agent_cli.command, "agy") {
                if let Err(e) = merge_agy_mcp_config(&step_mcp_url) {
                    tracing::warn!(
                        agent_id = %bg_agent_id,
                        cwd = %bg_cwd,
                        error = %e,
                        "Failed to write agy MCP config; run will proceed without Launchpad tools"
                    );
                }
            }

            // Build argv and spawn the binary for this continuation step.
            let step_argv = CliAgentRunner::build_argv(
                &run_agent,
                &continuation_input,
                Some(&step_mcp_config),
                Some(&step_mcp_url),
            );
            let step_stdin = if bg_input_mode == InputMode::Stdin {
                Some(continuation_input.clone())
            } else {
                None
            };
            let step_tools_in_flight = Arc::new(AtomicUsize::new(0));
            // Same session this step's MCP config just pointed the CLI at
            // (`step_mcp_config`/`step_mcp_url` above) — its `form_suspended`
            // counter is the one `LiveFormBridge::ask_form` increments (via
            // the MCP route handler's per-request bridge, see
            // `routes::mcp::handle_mcp_request`) when this step's subprocess
            // suspends on a synchronous `AskUserQuestionWithForm` answer.
            // Sharing the same Arc here lets the overall wall-clock deadline
            // below observe it. `None` only if the session lookup races the
            // registration above and misses — degrades to the pre-existing
            // always-consuming behavior rather than failing the spawn.
            let step_form_suspended = runner
                .mcp_sessions
                .get_by_session_id(&step_session_id)
                .map(|session| Arc::clone(&session.form_suspended));
            let step_spawn_input = SpawnInput {
                run_id: Some(bg_run_id.clone()),
                backend_id: bg_agent_id.clone(),
                scope_key: Some(bg_agent_id.clone()),
                argv: step_argv,
                cwd: Some(bg_cwd.clone()),
                env: bg_env.clone(),
                stdin_data: step_stdin,
                timeout_ms: Some(bg_timeout_ms),
                no_output_timeout_ms: Some(bg_no_output_timeout_ms),
                tools_in_flight: Some(step_tools_in_flight.clone()),
                form_suspended: step_form_suspended,
            };
            let managed_run = match runner.process_supervisor.spawn(step_spawn_input).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        agent_id = %bg_agent_id,
                        run_id = %bg_run_id,
                        "Failed to spawn continuation step: {}",
                        e
                    );
                    end_reason = RunEndReason::Error;
                    break 'continuation;
                }
            };
            let mut normalizer = runner
                .normalizer_registry
                .create(&bg_cli_config.command, &bg_cli_config);
            normalizer.set_tools_in_flight_counter(step_tools_in_flight);

            // Destructure managed_run and register cancel_tx for external cancellation
            let ao_process::supervisor::ManagedRun {
                run_id: _mr_run_id,
                pid: _mr_pid,
                started_at: _mr_started,
                mut stdout_rx,
                mut stderr_rx,
                wait_handle,
                cancel_tx,
            } = managed_run;
            runner.register_cancel_sender(&bg_run_id, cancel_tx).await;

            // Collect stderr in background
            let stderr_handle = tokio::spawn(async move {
                let mut lines = Vec::new();
                while let Some(line) = stderr_rx.recv().await {
                    lines.push(line);
                }
                lines
            });

            // Record transcript file size before streaming so we can roll back on cancellation
            let pre_run_transcript_size = runner
                .persistence
                .transcripts
                .file_size_for_run(&bg_agent_id, bg_transcript_override.as_deref())
                .await
                .unwrap_or(0);

            // Per-step tag scanner (reset each continuation step).
            let mut tag_scanner = TagStreamScanner::new();

            // Buffer TextComplete events so intermediate steps' TextComplete
            // is suppressed; only the terminal step emits TextComplete to the
            // event bus.
            let mut step_buffered_text_complete: Option<AgentEventPayload> = None;

            // Process stdout chunks through normalizer
            while let Some(chunk) = stdout_rx.recv().await {
                let payloads = normalizer.process_chunk(&chunk);
                for mut payload in payloads {
                    // Strip action tags from TextDelta payloads and capture any
                    // lifecycle events to emit immediately before the payload.
                    let action_events = apply_tag_scanner(&mut tag_scanner, &mut payload);
                    for ev in action_events {
                        runner
                            .event_bus
                            .emit(&bg_run_id, &bg_event_agent_id, bg_thread_id.clone(), ev)
                            .await;
                    }

                    let mut suppress = false;
                    // Set below when this step is tasklist-scoped, so the
                    // "Suppress empty responses" branch can still persist a
                    // hidden record of the raw (pre-strip) output.
                    let mut raw_output_for_suppressed_persist: Option<String> = None;

                    if let AgentEventPayload::TextComplete { ref mut text } = payload {
                        // Extract and process <task action="..."> tags. Only
                        // run in tasklist scope — outside that scope (chat
                        // view, individual agent runs) prose mentions of
                        // `<task>` are not parsed, so they don't surface as
                        // spurious "[<task> parse error]" system bubbles or
                        // get stripped from the output. Parse errors that DO
                        // arise inside tasklist scope still surface as
                        // structured follow-up notes (does not crash run).
                        if let Some((tl_owner, tasklist_id, assigned_task_id)) =
                            bg_tasklist_assigned.as_ref()
                        {
                            // Snapshot the producing agent's raw output BEFORE
                            // any stripping so the auto-reprompt routed back
                            // to the producing agent can quote it back
                            // verbatim.
                            let original_output_snapshot: String = text.clone();
                            raw_output_for_suppressed_persist = Some(original_output_snapshot.clone());

                            // Look for the `<task-item-notification>` block
                            // before the task-tag extractor strips the
                            // surrounding text. The notification block doesn't
                            // contain a `<task ...>` opener, so it survives
                            // `extract_task_actions` untouched — but we strip
                            // it ourselves below so the raw XML doesn't leak
                            // into the user-visible response.
                            let notification_result =
                                extract_task_item_notification(text);

                            let (task_cleaned, task_actions, task_errors) =
                                extract_task_actions(text);
                            if !task_actions.is_empty() || !task_errors.is_empty() {
                                *text = task_cleaned;
                            }
                            if !matches!(
                                notification_result,
                                NotificationParseResult::Missing
                            ) {
                                *text = strip_task_item_notification(text);
                            }
                            for err in &task_errors {
                                tracing::warn!(
                                    agent_id = %bg_agent_id,
                                    "<task> tag parse error: {}",
                                    err.message,
                                );
                                let msg = format!("[<task> parse error: {}]", err.message);
                                workflow_followups.push(WorkflowFollowup {
                                    context: msg.clone(),
                                    system_transcript: Some(msg),
                                });
                            }
                            for action in &task_actions {
                                let is_terminal_action = matches!(
                                    action,
                                    TaskTagAction::Complete { .. }
                                        | TaskTagAction::Fail { .. }
                                );
                                // Gate the terminal task action when the
                                // companion `<task-item-notification>` block
                                // is missing or malformed. Skip
                                // `process_task_tag_action` (so the task is
                                // NOT marked complete and NO changelog is
                                // written) and instead route a structured
                                // reprompt directly to the producing agent.
                                if is_terminal_action {
                                    let parse_failure_reason = match &notification_result {
                                        NotificationParseResult::Missing => Some(
                                            "the block was not present in the message"
                                                .to_string(),
                                        ),
                                        NotificationParseResult::Malformed(why) => {
                                            Some(why.clone())
                                        }
                                        NotificationParseResult::Parsed(_) => None,
                                    };
                                    if let Some(reason) = parse_failure_reason {
                                        // status string for the synthesized
                                        // fallback ChangelogEntry if this turn
                                        // happens to exhaust the retry budget
                                        // — mirrors the
                                        // <task-item-notification> wire format
                                        // used by the parse-success path.
                                        let completion_status = match action {
                                            TaskTagAction::Complete { .. } => "complete",
                                            TaskTagAction::Fail { .. } => "failed",
                                            _ => "unknown",
                                        };
                                        handle_task_item_notification_parse_failure(
                                            &runner,
                                            tl_owner,
                                            tasklist_id,
                                            assigned_task_id,
                                            &bg_agent_id,
                                            &original_output_snapshot,
                                            &reason,
                                            completion_status,
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                // parse-success path — persist the
                                // ChangelogEntry BEFORE the terminal
                                // transition so the in-stack progress.jsonl /
                                // meta.json writes and completion report can
                                // read this task's summary. The remind_me
                                // dispatch stays AFTER the transition (see
                                // below).
                                if is_terminal_action {
                                    if let NotificationParseResult::Parsed(notif) =
                                        &notification_result
                                    {
                                        record_task_item_changelog(
                                            &runner,
                                            tl_owner,
                                            tasklist_id,
                                            assigned_task_id,
                                            &bg_agent_id,
                                            notif,
                                        )
                                        .await;
                                    }
                                }
                                let result = process_task_tag_action(
                                    &runner,
                                    tl_owner,
                                    tasklist_id,
                                    assigned_task_id,
                                    &bg_agent_id,
                                    action,
                                )
                                .await;
                                match result {
                                    Ok(followup_opt) => {
                                        if let Some(followup) = followup_opt {
                                            workflow_followups.push(followup);
                                        }
                                        if is_terminal_action {
                                            bg_terminal_task_action_dispatched = true;
                                            // Reminder dispatch runs only after
                                            // the transition has been validated.
                                            if let NotificationParseResult::Parsed(
                                                notif,
                                            ) = &notification_result
                                            {
                                                dispatch_task_item_remind_me(
                                                    &runner,
                                                    tl_owner,
                                                    tasklist_id,
                                                    assigned_task_id,
                                                    notif,
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            agent_id = %bg_agent_id,
                                            "Failed to process <task> action: {}",
                                            e,
                                        );
                                        let msg = format!("[<task> action error: {}]", e);
                                        workflow_followups.push(WorkflowFollowup {
                                            context: msg.clone(),
                                            system_transcript: Some(msg),
                                        });
                                    }
                                }
                            }
                        }

                        // Extract and process <tasklist ...> tags.
                        // Runs in team scope (coordinators emit `create` from
                        // team chat) and for tasklist co-pilots (they emit
                        // `append` to add work to the bound tasklist).
                        // Outside both, prose mentions of `<tasklist>` from
                        // unrelated agents are left untouched.
                        if bg_is_team || bg_is_copilot {
                            let (tl_cleaned, tl_actions, tl_errors) =
                                extract_tasklist_actions(text);
                            if !tl_actions.is_empty() || !tl_errors.is_empty() {
                                *text = tl_cleaned;
                            }
                            for err in &tl_errors {
                                tracing::warn!(
                                    agent_id = %bg_agent_id,
                                    "<tasklist> tag parse error: {}",
                                    err.message,
                                );
                                let msg = format_tasklist_parse_error(err);
                                // Co-pilot tasklist tags are an internal mechanism;
                                // keep the parse-retry hint as a private context
                                // followup (agent self-corrects on next turn) and
                                // suppress the user-facing system bubble.
                                let system_transcript =
                                    if bg_is_copilot { None } else { Some(msg.clone()) };
                                workflow_followups.push(WorkflowFollowup {
                                    context: msg,
                                    system_transcript,
                                });
                            }
                            for action in tl_actions {
                                match process_tasklist_tag_action(&runner, &bg_agent_id, action).await
                                {
                                    Ok(Some(followup)) => workflow_followups.push(followup),
                                    Ok(None) => {}
                                    Err(e) => {
                                        tracing::error!(
                                            agent_id = %bg_agent_id,
                                            "Failed to process <tasklist> action: {}",
                                            e,
                                        );
                                        let msg = format!("[<tasklist> action error: {}]", e);
                                        let system_transcript =
                                            if bg_is_copilot { None } else { Some(msg.clone()) };
                                        workflow_followups.push(WorkflowFollowup {
                                            context: msg,
                                            system_transcript,
                                        });
                                    }
                                }
                            }
                        }

                        // Suppress empty responses (e.g. when entire response was workflow tags)
                        if text.trim().is_empty() {
                            suppress = true;
                            tracing::debug!(
                                agent_id = %bg_agent_id,
                                "Suppressing empty agent response after tag stripping"
                            );
                        }

                        // Queue response entry via TimelineAdapter.
                        // Team runs skip personal transcripts (adapter has no persistence).
                        // Tasklist-scoped runs route to per-tasklist transcript via
                        // bg_transcript_override (already wired into the adapter).
                        // Snapshot update happens in persist_pending() at run end.
                        if !suppress {
                            timeline_adapter.record_text_complete(text);
                        } else if let Some(raw) = raw_output_for_suppressed_persist.as_deref() {
                            // The visible text was fully consumed by <task>/
                            // <task-item-notification> stripping, but the agent
                            // still produced a turn. Persist the raw output as a
                            // hidden entry so the per-tasklist transcript stays a
                            // faithful, non-empty record even when every task in
                            // the run completes via a bare completion tag.
                            timeline_adapter.record_suppressed_text_complete(raw);
                        }
                    }

                    if !suppress {
                        if let AgentEventPayload::TextComplete { ref text } = payload {
                            final_output.push_str(text);
                            // Tee cleaned text to per-task output.txt
                            // (agent-owned tasklist runs only). Errors are
                            // logged and swallowed — a write failure must not
                            // abort the run.
                            if let Some(ref out_path) = bg_task_output_path {
                                if let Some(parent) = out_path.parent() {
                                    let _ = tokio::fs::create_dir_all(parent).await;
                                }
                                match tokio::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(out_path)
                                    .await
                                {
                                    Ok(mut f) => {
                                        use tokio::io::AsyncWriteExt;
                                        if let Err(e) = f.write_all(text.as_bytes()).await {
                                            tracing::warn!(
                                                agent_id = %bg_agent_id,
                                                "per-task output.txt write failed: {}",
                                                e,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            agent_id = %bg_agent_id,
                                            "per-task output.txt open failed: {}",
                                            e,
                                        );
                                    }
                                }
                            }
                            // Buffer TextComplete — emit only on terminal step.
                            step_buffered_text_complete = Some(payload);
                            continue;
                        }
                        if let AgentEventPayload::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                            cache_creation_tokens,
                            ..
                        } = &payload
                        {
                            total_input_tokens += input_tokens;
                            total_output_tokens += output_tokens;
                            // Parity with NativeAgentRunner's TimelineAdapter: every Usage
                            // event lands in `ao_engine::cache` with the same field shape.
                            // Lets `grep ao_engine::cache` answer "is caching working?" for
                            // both API and CLI runners (claude, codex, cursor-agent) without
                            // a per-provider branch in the log query.
                            tracing::info!(
                                target: "ao_engine::cache",
                                agent_id = %bg_event_agent_id,
                                run_id = %bg_run_id,
                                input = input_tokens,
                                output = output_tokens,
                                cache_read = cache_read_tokens,
                                cache_creation = cache_creation_tokens,
                                "cache usage",
                            );
                        }
                        // Persist tool_use/tool_result transcript entries for
                        // CLI-mode MCP tool calls (e.g. `ArtifactWrite`) — the
                        // drain loop above only ever broadcast these live, so a
                        // page reload had nothing to replay them from (unlike
                        // `NativeAgentRunner`, whose `TimelineAdapter::emit`
                        // persists both halves of a tool call). Only the
                        // "real" `ToolCallStarted` — the one carrying the fully
                        // parsed input, emitted once `content_block_stop`
                        // closes the block — is persisted; the earlier
                        // no-input announcement shares the same `tool_use_id`
                        // and would otherwise double up the transcript entry.
                        // `record_xml_tool_use`/`record_xml_tool_result` were
                        // already built for this exact CLI-side persist-without-
                        // re-emitting shape (see their doc comments) for a
                        // `<tool_use>` XML-tag transport that real MCP calls
                        // never go through — reused here for the transport CLI
                        // agents actually use, now that the normalizer threads
                        // a real per-call id through instead of dropping it.
                        if let AgentEventPayload::ToolCallStarted {
                            tool_use_id: Some(id),
                            tool_name,
                            tool_input: Some(input),
                            ..
                        } = &payload
                        {
                            timeline_adapter.record_xml_tool_use(id, tool_name, input.clone());
                        }
                        if let AgentEventPayload::ToolCallCompleted {
                            tool_use_id: Some(id),
                            output,
                            is_error,
                            ..
                        } = &payload
                        {
                            timeline_adapter.record_xml_tool_result(
                                id,
                                output.as_deref().unwrap_or(""),
                                *is_error,
                            );
                        }

                        runner
                            .event_bus
                            .emit(&bg_run_id, &bg_event_agent_id, bg_thread_id.clone(), payload)
                            .await;
                    }
                }
            }

            // Wait for process to exit
            let run_exit = wait_handle.await.unwrap_or_else(|e| {
                tracing::error!("wait_handle join error: {}", e);
                ao_process::supervisor::RunExit {
                    reason: ao_process::supervisor::TerminationReason::Error,
                    exit_code: None,
                    duration_ms: 0,
                    timed_out: false,
                    no_output_timed_out: false,
                }
            });

            // Collect stderr
            let stderr_lines = stderr_handle.await.unwrap_or_default();
            let stderr_str = stderr_lines.join("\n");

            if !stderr_str.is_empty() {
                tracing::warn!(
                    agent_id = %bg_agent_id,
                    run_id = %bg_run_id,
                    "Process stderr output: {}",
                    stderr_str
                );
            }

            // On cancellation, roll back any partial agent transcript entries written during streaming
            let was_cancelled = matches!(
                run_exit.reason,
                ao_process::supervisor::TerminationReason::Cancelled
            );
            if was_cancelled && !bg_is_team && !bg_is_delegated {
                if let Err(e) = runner
                    .persistence
                    .transcripts
                    .truncate_to_size_for_run(
                        &bg_agent_id,
                        bg_transcript_override.as_deref(),
                        pre_run_transcript_size,
                    )
                    .await
                {
                    tracing::error!(
                        agent_id = %bg_agent_id,
                        run_id = %bg_run_id,
                        "Failed to truncate transcript on cancellation: {}",
                        e
                    );
                } else {
                    tracing::info!(
                        agent_id = %bg_agent_id,
                        run_id = %bg_run_id,
                        "Rolled back transcript to pre-run size ({} bytes) after cancellation",
                        pre_run_transcript_size
                    );
                }
            }
            // Cancelled steps exit the continuation loop immediately.
            if was_cancelled {
                end_reason = RunEndReason::Cancelled;
                break 'continuation;
            }

            // Finalize normalizer
            let final_payloads = normalizer.finalize(run_exit.exit_code, &stderr_str);
            for mut payload in final_payloads {
                // Flush any in-flight action tags from the primary stream
                // through the same scanner, so orphan-open tags at process
                // exit cleanly Complete on the final TextComplete.
                let action_events = apply_tag_scanner(&mut tag_scanner, &mut payload);
                for ev in action_events {
                    runner
                        .event_bus
                        .emit(&bg_run_id, &bg_event_agent_id, bg_thread_id.clone(), ev)
                        .await;
                }

                let mut suppress = false;
                // Set below when this step is tasklist-scoped, so the
                // "Suppress empty responses" branch can still persist a
                // hidden record of the raw (pre-strip) output.
                let mut raw_output_for_suppressed_persist: Option<String> = None;

                if let AgentEventPayload::TextComplete { ref mut text } = payload {
                    // Extract and process <task action="..."> tags (finalize).
                    // Mirrors the streaming-loop branch so non-streaming
                    // normalizers (e.g. GenericNormalizer, where TextComplete
                    // only fires here) also drive task transitions. Scope-
                    // gated to tasklist runs — chat-view runs leave prose
                    // mentions of `<task>` untouched.
                    if let Some((tl_owner, tasklist_id, assigned_task_id)) =
                        bg_tasklist_assigned.as_ref()
                    {
                        // snapshot the raw text BEFORE any stripping so the
                        // auto-reprompt followup can quote the producing
                        // agent's original output verbatim.
                        let original_output_snapshot: String = text.clone();
                        raw_output_for_suppressed_persist = Some(original_output_snapshot.clone());

                        // mirror the streaming-loop branch's notification
                        // handling (see comment there).
                        let notification_result =
                            extract_task_item_notification(text);

                        let (task_cleaned, task_actions, task_errors) =
                            extract_task_actions(text);
                        if !task_actions.is_empty() || !task_errors.is_empty() {
                            *text = task_cleaned;
                        }
                        if !matches!(
                            notification_result,
                            NotificationParseResult::Missing
                        ) {
                            *text = strip_task_item_notification(text);
                        }
                        for err in &task_errors {
                            tracing::warn!(
                                agent_id = %bg_agent_id,
                                "<task> tag parse error: {}",
                                err.message,
                            );
                            let msg = format!("[<task> parse error: {}]", err.message);
                            workflow_followups.push(WorkflowFollowup {
                                context: msg.clone(),
                                system_transcript: Some(msg),
                            });
                        }
                        for action in &task_actions {
                            let is_terminal_action = matches!(
                                action,
                                TaskTagAction::Complete { .. }
                                    | TaskTagAction::Fail { .. }
                            );
                            // gate terminal action on notification parse
                            // result (see streaming-loop branch).
                            if is_terminal_action {
                                let parse_failure_reason = match &notification_result {
                                    NotificationParseResult::Missing => Some(
                                        "the block was not present in the message"
                                            .to_string(),
                                    ),
                                    NotificationParseResult::Malformed(why) => {
                                        Some(why.clone())
                                    }
                                    NotificationParseResult::Parsed(_) => None,
                                };
                                if let Some(reason) = parse_failure_reason {
                                    // see streaming-loop branch.
                                    let completion_status = match action {
                                        TaskTagAction::Complete { .. } => "complete",
                                        TaskTagAction::Fail { .. } => "failed",
                                        _ => "unknown",
                                    };
                                    handle_task_item_notification_parse_failure(
                                        &runner,
                                        tl_owner,
                                        tasklist_id,
                                        assigned_task_id,
                                        &bg_agent_id,
                                        &original_output_snapshot,
                                        &reason,
                                        completion_status,
                                    )
                                    .await;
                                    continue;
                                }
                            }
                            // Persist the ChangelogEntry BEFORE the terminal
                            // transition (see streaming branch above for why);
                            // remind_me dispatch stays after.
                            if is_terminal_action {
                                if let NotificationParseResult::Parsed(notif) =
                                    &notification_result
                                {
                                    record_task_item_changelog(
                                        &runner,
                                        tl_owner,
                                        tasklist_id,
                                        assigned_task_id,
                                        &bg_agent_id,
                                        notif,
                                    )
                                    .await;
                                }
                            }
                            let result = process_task_tag_action(
                                &runner,
                                tl_owner,
                                tasklist_id,
                                assigned_task_id,
                                &bg_agent_id,
                                action,
                            )
                            .await;
                            match result {
                                Ok(followup_opt) => {
                                    if let Some(followup) = followup_opt {
                                        workflow_followups.push(followup);
                                    }
                                    if is_terminal_action {
                                        bg_terminal_task_action_dispatched = true;
                                        if let NotificationParseResult::Parsed(
                                            notif,
                                        ) = &notification_result
                                        {
                                            dispatch_task_item_remind_me(
                                                &runner,
                                                tl_owner,
                                                tasklist_id,
                                                assigned_task_id,
                                                notif,
                                            )
                                            .await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        agent_id = %bg_agent_id,
                                        "Failed to process <task> action (finalize): {}",
                                        e,
                                    );
                                    let msg =
                                        format!("[<task> action error: {}]", e);
                                    workflow_followups.push(WorkflowFollowup {
                                        context: msg.clone(),
                                        system_transcript: Some(msg),
                                    });
                                }
                            }
                        }
                    }

                    // Extract and process <tasklist ...> tags (finalize).
                    // Runs in team scope (coordinator `create`) and for
                    // tasklist co-pilots (`append`). Outside both, prose
                    // mentions of `<tasklist>` are left untouched.
                    if bg_is_team || bg_is_copilot {
                        let (tl_cleaned, tl_actions, tl_errors) =
                            extract_tasklist_actions(text);
                        if !tl_actions.is_empty() || !tl_errors.is_empty() {
                            *text = tl_cleaned;
                        }
                        for err in &tl_errors {
                            tracing::warn!(
                                agent_id = %bg_agent_id,
                                "<tasklist> tag parse error (finalize): {}",
                                err.message,
                            );
                            let msg = format_tasklist_parse_error(err);
                            // See streaming-path comment: silence the user-facing
                            // bubble for co-pilots; the agent still gets the hint.
                            let system_transcript =
                                if bg_is_copilot { None } else { Some(msg.clone()) };
                            workflow_followups.push(WorkflowFollowup {
                                context: msg,
                                system_transcript,
                            });
                        }
                        for action in tl_actions {
                            match process_tasklist_tag_action(&runner, &bg_agent_id, action).await
                            {
                                Ok(Some(followup)) => workflow_followups.push(followup),
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!(
                                        agent_id = %bg_agent_id,
                                        "Failed to process <tasklist> action (finalize): {}",
                                        e,
                                    );
                                    let msg =
                                        format!("[<tasklist> action error: {}]", e);
                                    let system_transcript =
                                        if bg_is_copilot { None } else { Some(msg.clone()) };
                                    workflow_followups.push(WorkflowFollowup {
                                        context: msg,
                                        system_transcript,
                                    });
                                }
                            }
                        }
                    }

                    // Suppress empty responses (e.g. when entire response was workflow tags)
                    if text.trim().is_empty() {
                        suppress = true;
                        tracing::debug!(
                            agent_id = %bg_agent_id,
                            "Suppressing empty finalized response after tag stripping"
                        );
                    }

                    // Queue finalized response entry via TimelineAdapter.
                    // Cancelled runs skip (persist_pending is not called on
                    // cancellation). Team runs and delegated runs without a
                    // sidechain override record into an adapter with no
                    // persistence attached — a no-op. Delegated runs WITH an
                    // override record here so the sidechain file carries the
                    // child's responses.
                    if !was_cancelled {
                        if !suppress {
                            timeline_adapter.record_text_complete(text);
                        } else if let Some(raw) = raw_output_for_suppressed_persist.as_deref() {
                            // The visible text was fully consumed by <task>/
                            // <task-item-notification> stripping, but the agent
                            // still produced a turn. Persist the raw output as a
                            // hidden entry so the per-tasklist transcript stays a
                            // faithful, non-empty record even when every task in
                            // the run completes via a bare completion tag.
                            timeline_adapter.record_suppressed_text_complete(raw);
                        }
                    }
                }

                if !suppress {
                    if let AgentEventPayload::TextComplete { ref text } = payload {
                        final_output.push_str(text);
                        // Tee finalized text to per-task output.txt
                        // (agent-owned tasklist runs only). Mirrors the
                        // streaming-loop branch so non-streaming normalizers
                        // (e.g. GenericNormalizer) are covered.
                        if let Some(ref out_path) = bg_task_output_path {
                            if let Some(parent) = out_path.parent() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                            match tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(out_path)
                                .await
                            {
                                Ok(mut f) => {
                                    use tokio::io::AsyncWriteExt;
                                    if let Err(e) = f.write_all(text.as_bytes()).await {
                                        tracing::warn!(
                                            agent_id = %bg_agent_id,
                                            "per-task output.txt write failed (finalize): {}",
                                            e,
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        agent_id = %bg_agent_id,
                                        "per-task output.txt open failed (finalize): {}",
                                        e,
                                    );
                                }
                            }
                        }
                        // Buffer TextComplete — emit only on terminal step.
                        step_buffered_text_complete = Some(payload);
                        continue;
                    }
                    runner
                        .event_bus
                        .emit(&bg_run_id, &bg_event_agent_id, bg_thread_id.clone(), payload)
                        .await;
                }
            }

            // Terminal step: emit buffered TextComplete and end.
            if let Some(tc) = step_buffered_text_complete.take() {
                runner
                    .event_bus
                    .emit(&bg_run_id, &bg_event_agent_id, bg_thread_id.clone(), tc)
                    .await;
            }
            end_reason = match run_exit.reason {
                ao_process::supervisor::TerminationReason::Natural => RunEndReason::Completed,
                ao_process::supervisor::TerminationReason::Cancelled => RunEndReason::Cancelled,
                ao_process::supervisor::TerminationReason::Timeout => RunEndReason::TimedOut,
                ao_process::supervisor::TerminationReason::NoOutputTimeout => RunEndReason::NoOutputTimeout,
                ao_process::supervisor::TerminationReason::Error => RunEndReason::Error,
            };
            break 'continuation;

            } // end 'continuation loop

            // end_reason is set inside the loop (terminal step, cancellation, or cap trip).
            // Log non-successful terminations.
            match end_reason {
                RunEndReason::Completed => {}
                RunEndReason::TimedOut | RunEndReason::NoOutputTimeout => {
                    tracing::warn!(
                        agent_id = %bg_agent_id,
                        run_id = %bg_run_id,
                        reason = ?end_reason,
                        "Run ended due to timeout"
                    );
                }
                _ => {
                    tracing::error!(
                        agent_id = %bg_agent_id,
                        run_id = %bg_run_id,
                        reason = ?end_reason,
                        "Run ended abnormally"
                    );
                }
            }

            // Flush all queued transcript entries to disk. Skip on
            // cancellation — nothing meaningful was fully committed this turn.
            if end_reason != RunEndReason::Cancelled {
                timeline_adapter.persist_pending().await;
            }

            // Emit RunEnded event (use team-scoped agent_id if applicable)
            runner
                .event_bus
                .emit(
                    &bg_run_id,
                    &bg_event_agent_id,
                    bg_thread_id.clone(),
                    AgentEventPayload::RunEnded { reason: end_reason },
                )
                .await;

            // Tasklist stale-run detection. If the agent was running under a
            // tasklist scope but did not emit a terminal `<task>` action this
            // run, ask the feeder to either reprompt or fail the assigned
            // task. Both `<task complete>` (validate_and_complete) and
            // `<task fail>` (on_task_terminal) already drive the next step
            // on their own, so we skip this branch when the flag is set.
            if !bg_terminal_task_action_dispatched {
                if let Some((tl_owner, tasklist_id, _task_id)) = bg_tasklist_assigned.as_ref() {
                    if let Some(feeder) = runner.task_feeder.get() {
                        if let Err(e) = feeder
                            .on_run_ended(tl_owner, tasklist_id, &bg_agent_id)
                            .await
                        {
                            tracing::error!(
                                agent_id = %bg_agent_id,
                                tasklist_id = %tasklist_id,
                                "TaskFeeder::on_run_ended failed: {}",
                                e,
                            );
                        }
                    }
                }
            }

            // Write accumulated token usage to phase state for phase agents
            if (total_input_tokens > 0 || total_output_tokens > 0)
                && bg_agent_id.starts_with("task:")
            {
                // Parse task_id and phase_id from "task:{task_id}:phase:{phase_id}"
                if let Some(rest) = bg_agent_id.strip_prefix("task:") {
                    if let Some((task_id, phase_id)) = rest.split_once(":phase:") {
                        if let Some(ref wf_runner) = runner.workflow_runner {
                            if let Err(e) = wf_runner
                                .accumulate_phase_tokens(
                                    task_id,
                                    phase_id,
                                    total_input_tokens,
                                    total_output_tokens,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_id = %task_id,
                                    phase_id = %phase_id,
                                    "Failed to write phase token usage: {}",
                                    e
                                );
                            }
                        }
                    }
                }
            }

            // Clean up cancel sender (no-op if already removed by cancel_run)
            runner.unregister_cancel_sender(&bg_run_id).await;

            // Unregister from InstanceRegistry (use team-scoped key if applicable)
            runner
                .instance_registry
                .unregister_run(&bg_registry_key, &bg_run_id)
                .await;

            // `has_active_run` is no longer persisted on the snapshot —
            // routes overlay it from the instance registry, which we
            // unregistered from a few lines above. The previous block here
            // duplicated registry state into the on-disk snapshot, which
            // wedged the sidebar indicator any time this cleanup was
            // skipped (cancellation, panic, etc.).

            // Cleanup event bus seq counter
            runner.event_bus.cleanup_run(&bg_run_id).await;

            // Notify run completion with authoritative accumulated text and workflow follow-ups
            let _ = run_complete_tx.send(RunComplete {
                run_id: bg_run_id,
                output_text: final_output,
                workflow_followups,
                end_reason,
            }).await;
        });

        // Panic watcher. Holds the inner JoinHandle and awaits it in its own
        // task so that if the inner task panics, we synthesise the
        // `Error` + `RunEnded(Error)` events the frontend needs to clear its
        // in-flight bubble and re-enable the chat input. The
        // `InstanceRegistryGuard` Drop above handles the sidebar overlay; this
        // watcher handles the per-agent SSE listener.
        tokio::spawn(async move {
            match inner_handle.await {
                Ok(()) => {}
                Err(join_err) if join_err.is_panic() => {
                    let panic_payload = join_err.into_panic();
                    let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "non-string panic payload".to_string()
                    };
                    let user_msg = format!(
                        "Agent runner crashed mid-run: {}. The run was terminated. Try again, and if this repeats check the server logs for a stack trace.",
                        panic_msg
                    );
                    tracing::error!(
                        agent_id = %watcher_agent_id,
                        run_id = %watcher_run_id,
                        panic = %panic_msg,
                        "CLI runner spawned task panicked"
                    );
                    watcher_event_bus
                        .emit(
                            &watcher_run_id,
                            &watcher_agent_id,
                            watcher_thread_id.clone(),
                            AgentEventPayload::Error {
                                message: user_msg,
                                recoverable: false,
                            },
                        )
                        .await;
                    watcher_event_bus
                        .emit(
                            &watcher_run_id,
                            &watcher_agent_id,
                            watcher_thread_id.clone(),
                            AgentEventPayload::RunEnded {
                                reason: RunEndReason::Error,
                            },
                        )
                        .await;
                    // Belt-and-suspenders cleanup. The InstanceRegistryGuard
                    // inside the panicked task already spawned its own
                    // unregister; this extra call is idempotent on the inner
                    // HashMap and protects against the (extremely unlikely)
                    // case that Drop couldn't acquire a runtime handle.
                    watcher_registry
                        .unregister_run(&watcher_registry_key, &watcher_run_id)
                        .await;
                }
                Err(_) => {
                    // Task was cancelled at the runtime level (rare —
                    // normally cancellation flows through CancellationToken
                    // and produces an Ok). Nothing to surface.
                }
            }
        });

        Ok(run_id)
    }

}

impl Clone for CliAgentRunner {
    fn clone(&self) -> Self {
        Self {
            process_supervisor: Arc::clone(&self.process_supervisor),
            normalizer_registry: Arc::clone(&self.normalizer_registry),
            event_bus: Arc::clone(&self.event_bus),
            persistence: Arc::clone(&self.persistence),
            command_queue: Arc::clone(&self.command_queue),
            instance_registry: Arc::clone(&self.instance_registry),
            workflow_runner: self.workflow_runner.as_ref().map(Arc::clone),
            workflow_registry: self.workflow_registry.as_ref().map(Arc::clone),
            workflow_queue: self.workflow_queue.clone(),
            context_cache: self.context_cache.as_ref().map(Arc::clone),
            plugin_cache: self.plugin_cache.as_ref().map(Arc::clone),
            task_feeder: Arc::clone(&self.task_feeder),
            notification_dispatcher: Arc::clone(&self.notification_dispatcher),
            cancel_senders: Arc::clone(&self.cancel_senders),
            running_agents: Arc::clone(&self.running_agents),
            tools_registry: Arc::clone(&self.tools_registry),
            anchor_registry: Arc::clone(&self.anchor_registry),
            reflection_subscriber: Arc::clone(&self.reflection_subscriber),
            mcp_sessions: Arc::clone(&self.mcp_sessions),
        }
    }
}

#[async_trait]
impl AgentRunner for CliAgentRunner {
    fn mode(&self) -> AgentRunnerMode {
        AgentRunnerMode::Cli
    }

    async fn run(&self, request: AgentRunRequest) -> Result<RunComplete, AoError> {
        let arc_self = Arc::new(self.clone());
        let agent_id = request.agent.id.clone();

        // Register a run handle so the cancel HTTP route can fire the token.
        // The registry hands back a unique per-registration id rather than
        // keying on agent_id — a parent run and a subtask that share the
        // same agent profile each get their own entry, so neither overwrites
        // the other and the cancel route fans out to every active sibling.
        // When a parent token is supplied (delegated run), reuse it so
        // DelegateStop/parent cancel propagates without a separate bridge.
        let cancel_token = request.cancel.clone().unwrap_or_else(CancellationToken::new);
        let handle = RunHandle {
            agent_id: agent_id.clone(),
            thread_id: request.thread_id.clone(),
            cancel: cancel_token.clone(),
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        };
        let reg_id = self.running_agents.insert(handle);
        // RAII guard removes only THIS registration on every exit path,
        // including panics — sibling runs under the same agent_id stay
        // registered and cancellable.
        let _guard = RunningAgentsGuard::new(Arc::clone(&self.running_agents), reg_id);

        let external_tx = request.run_complete_tx;
        let (capture_tx, mut capture_rx) = mpsc::channel::<RunComplete>(1);
        let (fan_tx, mut fan_rx) = mpsc::channel::<RunComplete>(1);

        // Fan-out: forward RunComplete to both the queue manager channel
        // and the internal capture channel for our return value
        tokio::spawn(async move {
            if let Some(rc) = fan_rx.recv().await {
                let _ = external_tx.send(rc.clone()).await;
                let _ = capture_tx.send(rc).await;
            }
        });

        // run_with_scope spawns the background task and returns the run_id.
        // If the caller pre-allocated and pre-registered a run_id (queue
        // manager path), forward it so the runner skips its own
        // `register_run` and adopts the caller's id verbatim — see
        // `AgentRunRequest::pre_registered_run_id`.
        let run_id = arc_self.run_with_scope_inner(
            &request.agent,
            &request.prompt,
            &request.attachments,
            fan_tx,
            request.scope,
            request.focus_path.as_deref(),
            request.pre_registered_run_id,
            request.thread_id,
            request.delegate_chain,
            request.spawn_chain,
            request.isolate_history,
            request.transcript_override,
            request.event_channel,
            request.bypass_instance_cap,
        ).await?;

        // Bridge: when the cancel token fires (via running_agents.cancel),
        // translate that into the existing oneshot-sender cancel path so the
        // spawned CLI process is terminated.
        {
            let bridge_self = arc_self.clone();
            let bridge_run_id = run_id.clone();
            tokio::spawn(async move {
                cancel_token.cancelled().await;
                bridge_self.cancel_run(&bridge_run_id).await;
            });
        }

        capture_rx.recv().await
            .ok_or_else(|| AoError::Internal("Run completed without result".to_string()))
        // _guard drops here, removing from running_agents
    }
}

/// Apply a `<task action="...">` tag emitted by an agent during a tasklist run.
/// The agent must be the owner of `assigned_task_id` AND the tag's `task_id`
/// must match — otherwise the action is rejected (an agent cannot transition
/// a task assigned to a peer). On Complete/Fail this writes the new task
/// status and notifies the bound TaskFeeder so the next group dispatches.
async fn process_task_tag_action(
    runner: &Arc<CliAgentRunner>,
    owner: &ao_protocol::tasklist::TasklistOwner,
    tasklist_id: &str,
    assigned_task_id: &str,
    agent_id: &str,
    action: &TaskTagAction,
) -> Result<Option<WorkflowFollowup>, AoError> {
    use ao_protocol::tasklist::{TasklistOwner, TaskStatus};

    enum Disposition {
        Complete,
        Fail(String),
    }

    let (target_task_id, disposition) = match action {
        TaskTagAction::Complete { task_id } => (task_id.as_str(), Disposition::Complete),
        TaskTagAction::Fail { task_id, reason } => {
            (task_id.as_str(), Disposition::Fail(reason.clone()))
        }
        TaskTagAction::RequestClarification { task_id, question } => {
            tracing::info!(
                agent_id = %agent_id,
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "Task clarification requested: {}",
                question,
            );
            let summary = if question.is_empty() {
                format!("[Clarification logged for task {task_id}]")
            } else {
                format!("[Clarification logged for task {task_id}: {question}]")
            };
            return Ok(Some(WorkflowFollowup {
                context: summary.clone(),
                system_transcript: Some(summary),
            }));
        }
    };

    if target_task_id != assigned_task_id {
        return Err(AoError::ValidationError(format!(
            "agent '{agent_id}' attempted to transition task '{target_task_id}' but is assigned to '{assigned_task_id}'"
        )));
    }

    let tasklist_owned = tasklist_id.to_string();
    let task_owned = target_task_id.to_string();

    match disposition {
        Disposition::Complete => {
            // Output validation + reprompt-on-missing flows through the feeder
            // so the agent's self-report is not trusted. The feeder may
            // transition to Completed, reprompt via the dispatcher, or
            // transition to Failed after `max_attempts` validation failures.
            if let Some(feeder) = runner.task_feeder.get() {
                feeder
                    .validate_and_complete(owner, &tasklist_owned, &task_owned)
                    .await?;
            } else {
                tracing::warn!(
                    "TaskFeeder not bound; falling back to direct Completed transition without output validation"
                );
                // Fallback: only team-scope path is supported without a feeder.
                if let TasklistOwner::Team { team_id } = owner {
                    runner
                        .persistence
                        .tasklists
                        .set_task_status(
                            team_id,
                            &tasklist_owned,
                            &task_owned,
                            TaskStatus::Completed,
                        )
                        .await?;
                }
            }
        }
        Disposition::Fail(reason) => {
            if !reason.is_empty() {
                let reason_owned = reason.clone();
                let task_for_log = task_owned.clone();
                runner
                    .persistence
                    .tasklists
                    .mutate_by_owner(owner, &tasklist_owned, move |tl| {
                        for group in &mut tl.groups {
                            for t in &mut group.tasks {
                                if t.id == task_for_log {
                                    t.error_log.push(reason_owned.clone());
                                }
                            }
                        }
                        Ok(())
                    })
                    .await?;
            }
            runner
                .persistence
                .tasklists
                .set_task_status_by_owner(
                    owner,
                    &tasklist_owned,
                    &task_owned,
                    TaskStatus::Failed,
                )
                .await?;
            if let Some(feeder) = runner.task_feeder.get() {
                feeder
                    .emit_task_updated(owner, &tasklist_owned, &task_owned)
                    .await;
                if let Err(e) = feeder
                    .on_task_terminal(owner, &tasklist_owned, &task_owned)
                    .await
                {
                    tracing::error!(
                        tasklist_id = %tasklist_id,
                        task_id = %target_task_id,
                        "Failed to notify TaskFeeder of task terminal: {}",
                        e,
                    );
                }
            } else {
                tracing::warn!(
                    "TaskFeeder not bound; <task> fail recorded but feeder not notified"
                );
            }
        }
    }

    Ok(None)
}

/// Persist the producing agent's `<task-item-notification>` to the tasklist's
/// hidden `_changelog.jsonl` so downstream consumers (co-pilot context
/// injection, retro tools) can see what the agent reported.
///
/// IMPORTANT — call ordering: this runs *before* the terminal `<task action>`
/// transition is processed, not after. The terminal transition is what writes
/// `progress.jsonl`, rewrites the task `meta.json`, and builds the completion
/// report — and all three source each task's summary by reading this changelog.
/// Appending here first makes the changelog the single source of truth that is
/// already on disk when those in-stack writes fire. Appending afterwards (the
/// previous ordering) left the just-completed task's summary missing from its
/// own progress block, meta, and the completion report.
///
/// The `status` recorded is the agent's self-reported wording
/// (`complete | failed | needs_clarification`), which the changelog is
/// documented to round-trip verbatim — so recording it ahead of output
/// validation is consistent with its purpose even when a later validation
/// reprompt supersedes it (the retry appends a fresh entry; readers take the
/// last entry per task).
///
/// Best-effort: a write failure is logged and swallowed so it can never block
/// the terminal transition.
pub(crate) async fn record_task_item_changelog(
    runner: &Arc<CliAgentRunner>,
    owner: &ao_protocol::tasklist::TasklistOwner,
    tasklist_id: &str,
    task_id: &str,
    agent_id: &str,
    notification: &TaskItemNotification,
) {
    use ao_protocol::changelog::ChangelogEntry;

    let entry = ChangelogEntry {
        task_id: task_id.to_string(),
        tasklist_id: tasklist_id.to_string(),
        agent_id: agent_id.to_string(),
        status: notification.status.clone(),
        summary: notification.summary.clone(),
        details: notification.details.clone(),
        ts: Utc::now(),
    };
    if let Err(e) = runner
        .persistence
        .changelogs
        .append(owner, tasklist_id, &entry)
        .await
    {
        tracing::warn!(
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            agent_id = %agent_id,
            "Failed to append changelog entry: {}",
            e
        );
    }
}

/// If the task's `remind_me` field is `Some(agent_id)`, format the
/// notification back to `<task-item-notification>` XML and submit it as a
/// [`QueuedMessage`] to that agent's mailbox via the existing
/// [`crate::queue_manager::QueueManagerRegistry`] pipeline.
///
/// Call ordering: this runs *after* the terminal `<task action>` transition has
/// been processed. Unlike the changelog record (which must precede the
/// transition), the reminder must follow it — dispatching a "task done"
/// notification before output validation could complete would falsely notify
/// the reminded agent for a task that validation then reprompts.
///
/// Best-effort: a missing dispatcher or submit failure is logged and swallowed.
pub(crate) async fn dispatch_task_item_remind_me(
    runner: &Arc<CliAgentRunner>,
    owner: &ao_protocol::tasklist::TasklistOwner,
    tasklist_id: &str,
    task_id: &str,
    notification: &TaskItemNotification,
) {
    use ao_protocol::message::QueuedMessage;

    let remind_me = match runner
        .persistence
        .tasklists
        .get_by_owner(owner, tasklist_id)
        .await
    {
        Ok(Some(tl)) => tl
            .groups
            .into_iter()
            .flat_map(|g| g.tasks.into_iter())
            .find(|t| t.id == task_id)
            .and_then(|t| t.remind_me),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "remind_me lookup: failed to load tasklist: {}",
                e
            );
            None
        }
    };

    let Some(target_agent_id) = remind_me else {
        return;
    };

    let Some(dispatcher) = runner.notification_dispatcher.get() else {
        tracing::warn!(
            target_agent_id = %target_agent_id,
            "NotificationDispatcher not bound; <task-item-notification> dispatch to remind_me skipped"
        );
        return;
    };

    let xml = format_task_item_notification(notification);
    let message = QueuedMessage {
        message_id: Uuid::new_v4().to_string(),
        content: xml,
        queued_at: Utc::now(),
        attachments: Vec::new(),
        source: None,
        focus_path: None,
        thread_id: None,
        };
    if let Err(e) = dispatcher
        .submit_to_agent(&target_agent_id, message)
        .await
    {
        tracing::warn!(
            target_agent_id = %target_agent_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            "Failed to submit <task-item-notification> to remind_me agent: {}",
            e
        );
    }
}

/// Cap on the auto-reprompt loop. After this many parse failures for the same
/// task, the system stops dispatching producer re-prompts (and `remind_me`
/// notifications), synthesizes a fallback `ChangelogEntry` so downstream state
/// still reflects the completion the agent claimed, and flips
/// `Task.parse_failed` so a misbehaving agent can't loop forever.
pub(crate) const MAX_NOTIFICATION_PARSE_RETRIES: u32 = 3;

/// Auto-reprompt routing for a missing or malformed `<task-item-notification>`
/// block.
///
/// Called instead of the terminal-task path (NOT alongside it) when the
/// producing agent emitted `<task action="complete|fail">` but the required
/// nested notification block was either absent or unparseable. The contract
/// is that the notification XML must be the body of the wrapping `<task>`
/// tag (see `prompts/sections/task_notification_format.md`); a self-closing
/// `<task ... />` for `complete`/`fail` is treated as a parse failure for
/// this purpose. Three side effects:
///
/// 1. Bumps the persisted `Task.notification_parse_retry_count` so a server
///    restart doesn't reset the budget capped by [`MAX_NOTIFICATION_PARSE_RETRIES`].
/// 2. Builds a re-prompt `QueuedMessage` addressed to the **producing
///    agent** (the worker that emitted the bad message), explaining in
///    prose what was wrong and including a worked nested XML example so
///    the next attempt can re-emit a valid `<task>…<task-item-notification>…
///    </task-item-notification></task>` element. Earlier versions routed
///    this to the team coordinator, but the coordinator routinely parroted
///    the structured key/value body back as a fake "answer" — sending the
///    instruction directly to the worker fixes the miscommunication.
/// 3. Submits the message to the producing agent's mailbox via the bound
///    NotificationDispatcher.
///
/// Once the bumped retry counter reaches [`MAX_NOTIFICATION_PARSE_RETRIES`],
/// the helper switches to the graceful
/// exhaustion path: it appends a synthesized `ChangelogEntry` (status from
/// `completion_status`), the `Task.parse_failed` flag is flipped inside the
/// same atomic write as the counter bump, and the worker re-prompt +
/// `remind_me` notification are BOTH suppressed. A `tracing::warn!` line
/// names the producing agent so persistently misbehaving agents can be
/// spotted in logs.
///
/// The terminal `<task action="…">` action is intentionally NOT processed
/// when this helper runs — the task remains in its prior status (typically
/// `InProgress`). All errors are logged and swallowed so a mailbox dispatch
/// failure can't crash the producing agent's run.
pub(crate) async fn handle_task_item_notification_parse_failure(
    runner: &Arc<CliAgentRunner>,
    owner: &ao_protocol::tasklist::TasklistOwner,
    tasklist_id: &str,
    task_id: &str,
    producing_agent_id: &str,
    original_output: &str,
    reason: &str,
    completion_status: &str,
) {
    use ao_protocol::changelog::ChangelogEntry;
    use ao_protocol::message::QueuedMessage;

    // Bump the persisted retry counter and, if this bump tips us into the
    // cap, also flip `parse_failed = true` in the same atomic temp+rename
    // write so a server restart can never observe one-without-the-other.
    let task_for_bump = task_id.to_string();
    let bump_result = runner
        .persistence
        .tasklists
        .mutate_by_owner(owner, tasklist_id, move |tl| {
            for group in &mut tl.groups {
                for t in &mut group.tasks {
                    if t.id == task_for_bump {
                        t.notification_parse_retry_count =
                            t.notification_parse_retry_count.saturating_add(1);
                        if t.notification_parse_retry_count
                            >= MAX_NOTIFICATION_PARSE_RETRIES
                        {
                            t.parse_failed = true;
                        }
                    }
                }
            }
            Ok(())
        })
        .await;

    // Read the post-bump count off the returned tasklist so we can branch
    // into the exhaustion path without a second persistence round-trip.
    // On bump failure we conservatively skip the exhaustion check (we have
    // no reliable count) and fall through to the followup dispatch path.
    let post_bump_count: Option<u32> = match &bump_result {
        Ok(tl) => tl
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .map(|t| t.notification_parse_retry_count),
        Err(e) => {
            tracing::warn!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "Failed to bump notification_parse_retry_count: {}",
                e
            );
            None
        }
    };

    if matches!(post_bump_count, Some(n) if n >= MAX_NOTIFICATION_PARSE_RETRIES) {
        // retry-exhaustion path: synthesize a minimal ChangelogEntry
        // (status mirrors the completion tag the producing agent emitted)
        // and DO NOT dispatch the producer re-prompt or any remind_me
        // notification. `parse_failed` was already flipped inside the
        // mutate closure above. The clear log line names the producing
        // agent so misbehaving agents can be spotted in observability.
        let preview_len = original_output.len().min(500);
        let original_preview: String = original_output.chars().take(preview_len).collect();
        tracing::warn!(
            producing_agent_id = %producing_agent_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            reason = %reason,
            completion_status = %completion_status,
            original_output_len = original_output.len(),
            original_output_preview = %original_preview,
            "<task-item-notification> auto-reprompt exhausted after {} parse failures — synthesizing fallback changelog entry and flipping parse_failed",
            MAX_NOTIFICATION_PARSE_RETRIES
        );
        let synthesized = ChangelogEntry {
            task_id: task_id.to_string(),
            tasklist_id: tasklist_id.to_string(),
            agent_id: producing_agent_id.to_string(),
            status: completion_status.to_string(),
            summary: "auto-synthesized after parse failure".to_string(),
            details: Some(format!(
                "the producing agent's <task-item-notification> block was missing or invalid for {} consecutive parse attempts (last reason: {}); parse_failed has been set to true.",
                MAX_NOTIFICATION_PARSE_RETRIES, reason
            )),
            ts: Utc::now(),
        };
        if let Err(e) = runner
            .persistence
            .changelogs
            .append(owner, tasklist_id, &synthesized)
            .await
        {
            tracing::warn!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "Failed to append synthesized ChangelogEntry on retry exhaustion: {}",
                e
            );
        }
        return;
    }

    let Some(dispatcher) = runner.notification_dispatcher.get() else {
        tracing::warn!(
            producing_agent_id = %producing_agent_id,
            "<task-item-notification> auto-reprompt: NotificationDispatcher not bound; followup dropped"
        );
        return;
    };

    let body = format!(
        "Your previous final message did not include a valid `<task-item-notification>` block: {reason}.\n\
         \n\
         The required schema is one nested XML element — the notification MUST be the body of the `<task action=\"…\">` tag, not a sibling. Do NOT emit a self-closing `<task ... />` for `complete`/`fail` and do NOT emit a key/value list with named fields like \"Producing agent:\" or \"Tasklist:\".\n\
         \n\
         Re-emit your final response with this exact nested shape:\n\
         \n\
         <task action=\"{completion_status}\" task_id=\"{task_id}\">\n\
         \x20\x20<task-item-notification>\n\
         \x20\x20\x20\x20<status>{completion_status}</status>\n\
         \x20\x20\x20\x20<summary>One-line summary of what you accomplished or why it failed.</summary>\n\
         \x20\x20\x20\x20<details>Optional longer details — omit this tag entirely if none.</details>\n\
         \x20\x20</task-item-notification>\n\
         </task>\n\
         \n\
         For reference, your previous output was:\n\
         {original_output}",
    );

    let body_len = body.len();
    let message_id = Uuid::new_v4().to_string();
    let message = QueuedMessage {
        message_id: message_id.clone(),
        content: body,
        queued_at: Utc::now(),
        attachments: Vec::new(),
        source: None,
        focus_path: None,
        thread_id: None,
        };
    // Diagnostic snapshot of what the model actually emitted, truncated to keep
    // log lines bounded. When this path keeps firing, this is the only signal
    // we have for *why* — the model's exact output text, not just our parse
    // verdict. Without it, retry-exhaustion loops are opaque in logs.
    let preview_len = original_output.len().min(500);
    let original_preview: String = original_output.chars().take(preview_len).collect();
    tracing::warn!(
        producing_agent_id = %producing_agent_id,
        tasklist_id = %tasklist_id,
        task_id = %task_id,
        retry_count = ?post_bump_count,
        reason = %reason,
        completion_status = %completion_status,
        message_id = %message_id,
        body_len,
        original_output_len = original_output.len(),
        original_output_preview = %original_preview,
        "<task-item-notification> auto-reprompt dispatching to producing agent (nested-form re-prompt)"
    );

    if let Err(e) = dispatcher.submit_to_agent(producing_agent_id, message).await {
        tracing::warn!(
            producing_agent_id = %producing_agent_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            "Failed to submit <task-item-notification> auto-reprompt to producing agent: {}",
            e
        );
    }
}

/// Apply a `<tasklist ...>` tag emitted by an agent.
///
/// - `Create` was team-scoped and is now always rejected with a logged
///   warning and NO state change — teams were removed, so there is nothing
///   for it to create against. The tag still parses; only the outcome
///   changed.
/// - `Append` is co-pilot-only: the bound tasklist is resolved by
///   `find_by_copilot_agent_id(agent_id)`, which walks both the team and the
///   agent tasklist trees, so it resolves project-scoped (agent-owned)
///   tasklists — the kind the live project co-pilot route binds. Each group's
///   `owner_agent_id`s are validated against the agent store; if all pass, the
///   groups are appended, terminal tasklists revive (Completed -> Active or
///   Paused via the same auto-resume window as the HTTP append_task route;
///   Failed/Cancelled -> Paused), and the feeder is poked.
///
///   Appended tasks carry a `Pinned` assignment built from the validated
///   `owner_agent_id`, because agent-owned tasklists dispatch on
///   `task.assignment` and would otherwise hand the co-pilot's explicit choice
///   back to the classifier.
///
///   The wake event that re-enrols the bound co-pilot is emitted for
///   team-owned tasklists only: `TasklistWoke` carries just a team id and the
///   mailbox poller resolves it with a team-keyed `get`. Agent-owned co-pilots
///   re-enrol through wake-on-deliver in `QueueManagerRegistry::submit_message`,
///   which keys off the agent's profile template instead of ownership.
async fn process_tasklist_tag_action(
    runner: &Arc<CliAgentRunner>,
    agent_id: &str,
    action: TasklistTagAction,
) -> Result<Option<WorkflowFollowup>, AoError> {
    use ao_protocol::tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskStatus, TasklistOwner, TasklistStatus,
    };

    match action {
        TasklistTagAction::Create { team, .. } => {
            // Team-scoped tasklist creation was removed along with the team
            // subsystem: nothing can resolve `team` any more, and there is no
            // supported way to create a team-owned tasklist. Reject explicitly
            // rather than silently doing nothing, so an agent that emits the
            // tag gets a usable message back instead of a no-op.
            //
            // The tag grammar still parses `action="create"` (see
            // `tasklist_extraction`), and the only prompt section documenting
            // it (`routing_and_dispatch`) is not part of any assembled live
            // prompt — only the co-pilot composition ships. Dropping the
            // action from the grammar belongs with the wider coordinator
            // removal, not with this change.
            tracing::warn!(
                agent_id = %agent_id,
                team_id = %team,
                "<tasklist action=\"create\"> rejected: team-scoped tasklists are no longer supported",
            );
            let msg = format!(
                "[<tasklist> create rejected: team-scoped tasklists are no longer supported (team '{team}')]"
            );
            Ok(Some(WorkflowFollowup {
                context: msg.clone(),
                system_transcript: Some(msg),
            }))
        }
        TasklistTagAction::Append { groups } => {
            // Resolve the bound tasklist for this co-pilot. Non-co-pilot
            // agents that somehow emit `<tasklist action="append">` get
            // rejected here because no binding exists for them.
            let tasklist = match runner
                .persistence
                .tasklists
                .find_by_copilot_agent_id(agent_id)
                .await?
            {
                Some(tl) => tl,
                None => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "<tasklist action=\"append\"> rejected: no tasklist is bound to this agent",
                    );
                    let msg = format!(
                        "[<tasklist> append rejected: agent '{agent_id}' is not bound to a tasklist as its co-pilot]"
                    );
                    return Ok(Some(WorkflowFollowup {
                        context: msg.clone(),
                        system_transcript: Some(msg),
                    }));
                }
            };
            // Carry the real owner through the whole branch. A co-pilot may be
            // bound to either a team-owned or an agent-owned tasklist, and the
            // store is keyed differently for each — synthesizing a team id from
            // `tasklist.team_id` would send every agent-owned append at the
            // team tree and fail to find the tasklist.
            let owner = tasklist.owner.clone();
            let tasklist_id = tasklist.id.clone();
            let tasklist_title = tasklist.title.clone();
            let project_id = tasklist.project_id.clone();

            if groups.is_empty() {
                let msg = "[<tasklist> append rejected: at least one group is required]".to_string();
                return Ok(Some(WorkflowFollowup {
                    context: msg.clone(),
                    system_transcript: Some(msg),
                }));
            }
            if groups.iter().any(|g| g.tasks.is_empty()) {
                let msg = "[<tasklist> append rejected: every group must contain at least one task]"
                    .to_string();
                return Ok(Some(WorkflowFollowup {
                    context: msg.clone(),
                    system_transcript: Some(msg),
                }));
            }

            // Validate every owner_agent_id against the agent store. This
            // previously validated against the team roster (members +
            // coordinator); with teams removed, "the agent exists" is the
            // equivalent guarantee. Reject the entire append on any unknown
            // id so we don't half-apply.
            let mut bad_ids: Vec<String> = Vec::new();
            for g in &groups {
                for t in &g.tasks {
                    let owner = t.owner_agent_id.trim();
                    if owner.is_empty() {
                        bad_ids.push("(empty)".to_string());
                    } else if runner.persistence.agents.get(owner).await?.is_none() {
                        bad_ids.push(t.owner_agent_id.clone());
                    }
                }
            }
            if !bad_ids.is_empty() {
                bad_ids.sort();
                bad_ids.dedup();
                let msg = format!(
                    "[<tasklist> append rejected: unknown owner_agent_id(s) [{bad}]. \
                     Every task owner must be the id of an existing agent.]",
                    bad = bad_ids.join(", "),
                );
                return Ok(Some(WorkflowFollowup {
                    context: msg.clone(),
                    system_transcript: Some(msg),
                }));
            }

            // Build the new groups (uuids, prefixed expected_outputs, etc.)
            // and remember (group_id, task_id) pairs for event emission.
            let mut new_task_ids: Vec<String> = Vec::new();
            let built_groups: Vec<TaskGroup> = groups
                .into_iter()
                .map(|g| {
                    let group_id = Uuid::new_v4().to_string();
                    let tasks = g
                        .tasks
                        .into_iter()
                        .map(|t| {
                            let task_id = Uuid::new_v4().to_string();
                            new_task_ids.push(task_id.clone());
                            let mut expected_outputs = t.expected_outputs;
                            ao_protocol::tasklist::prefix_expected_outputs(
                                &task_id,
                                &mut expected_outputs,
                            );
                            let task_owner_agent_id = t.owner_agent_id;
                            Task {
                                id: task_id,
                                owner_agent_id: task_owner_agent_id.clone(),
                                prompt: t.prompt,
                                expected_outputs,
                                status: TaskStatus::Pending,
                                group_id: group_id.clone(),
                                attempt_count: 0,
                                error_log: Vec::new(),
                                comments: Vec::new(),
                                attachments: Vec::new(),
                                // The appending co-pilot wants to be woken
                                // when each task completes so it can react in
                                // chat. Coordinator-seeded tasks (the `Create`
                                // arm) intentionally leave this `None`; if a
                                // coordinator wants a callback it appends a
                                // follow-up task assigned to the target agent.
                                remind_me: Some(agent_id.to_string()),
                                parse_failed: false,
                                notification_parse_retry_count: 0,
                                // The tag requires an explicit owner_agent_id
                                // and we validated it against the agent store
                                // above, so record it as a Pinned assignment.
                                // Agent-owned tasklists dispatch on
                                // `task.assignment` and ignore
                                // `owner_agent_id` (see task_feeder::dispatch);
                                // leaving this None would defer every appended
                                // task to the classifier, which could pick a
                                // different agent than the co-pilot named.
                                // Pinned is never overwritten by the
                                // classifier. Team-owned tasklists dispatch on
                                // `owner_agent_id` and ignore this field, so
                                // setting it is inert on that path.
                                assignment: Some(TaskAssignment {
                                    owner_agent_id: task_owner_agent_id,
                                    mode: AssignmentMode::Pinned,
                                }),
                                classifier_token: 0,
                                dispatch_token: 0,
                            }
                        })
                        .collect();
                    TaskGroup {
                        id: group_id,
                        mode: g.mode,
                        tasks,
                    }
                })
                .collect();

            tracing::debug!(
                agent_id = %agent_id,
                tasklist_id = %tasklist_id,
                new_task_count = new_task_ids.len(),
                remind_me_set = true,
                target = %agent_id,
                "co-pilot <tasklist append>: stamped remind_me on new tasks",
            );

            // Append-to-terminal revival mirrors the HTTP append_task route:
            // Completed within an 8-minute window AND no other active slot
            // taken -> auto-resume to Active; otherwise Paused. Failed and
            // Cancelled always revive to Paused.
            const AUTO_RESUME_WINDOW: chrono::Duration = chrono::Duration::minutes(8);
            // "Is the owner's single active slot already spoken for by a
            // different tasklist?" Both ownership kinds enforce one active
            // tasklist per owner, they just look it up in different trees.
            let active_slot_taken = match &owner {
                TasklistOwner::Team { team_id } => runner
                    .persistence
                    .tasklists
                    .find_active(team_id)
                    .await?
                    .map(|other| other.id != tasklist_id)
                    .unwrap_or(false),
                TasklistOwner::Agent { agent_id } => runner
                    .persistence
                    .tasklists
                    .active_for_agent(agent_id)
                    .await?
                    .map(|other| other.id != tasklist_id)
                    .unwrap_or(false),
            };
            let mut revived_to_paused = false;
            let mut revived_to_active = false;

            let updated = runner
                .persistence
                .tasklists
                .mutate_by_owner(&owner, &tasklist_id, |tl| {
                    for g in built_groups.iter().cloned() {
                        tl.groups.push(g);
                    }
                    match tl.status {
                        TasklistStatus::Completed => {
                            let within_window = tl
                                .last_active_at
                                .map(|t| Utc::now().signed_duration_since(t) < AUTO_RESUME_WINDOW)
                                .unwrap_or(false);
                            if within_window && !active_slot_taken {
                                tl.status = TasklistStatus::Active;
                                revived_to_active = true;
                            } else {
                                tl.status = TasklistStatus::Paused;
                                revived_to_paused = true;
                            }
                        }
                        TasklistStatus::Failed | TasklistStatus::Cancelled => {
                            tl.status = TasklistStatus::Paused;
                            revived_to_paused = true;
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await?;

            // Emit SSE events: one TasklistTaskAdded per new task, plus a
            // status change if the tasklist was revived.
            //
            // Unlike the HTTP append route, this path has no response body to
            // carry the new state back to the UI, so the owner's own channel
            // must always be notified: team-owned tasklists fan out on
            // `team:{id}`, agent-owned ones on the agent id. Project-stamped
            // tasklists additionally mirror onto the project channel, which is
            // what the project panel subscribes to. When `project_id` is set
            // the per-agent chat SSE handler skips the agent-channel copy, so
            // the mirror doesn't leak project rows into the agent store.
            let synth_run_id = format!("tasklist:{}", tasklist_id);
            let mut channels: Vec<String> = vec![match &owner {
                TasklistOwner::Team { team_id } => format!("team:{}", team_id),
                TasklistOwner::Agent { agent_id } => agent_id.clone(),
            }];
            if let Some(pid) = &project_id {
                channels.push(format!("project:{}", pid));
            }
            // `team_id` on the payload is the legacy fan-out key; agent-owned
            // tasklists carry the empty string here and are identified by the
            // `owner`/`project_id` fields instead.
            let event_team_id = match &owner {
                TasklistOwner::Team { team_id } => team_id.clone(),
                TasklistOwner::Agent { .. } => String::new(),
            };

            if revived_to_paused || revived_to_active {
                let status_str = if revived_to_active { "active" } else { "paused" };
                for channel in &channels {
                    runner
                        .event_bus
                        .emit(
                            &synth_run_id,
                            channel,
                            None,
                            AgentEventPayload::TasklistStatusChanged {
                                team_id: event_team_id.clone(),
                                tasklist_id: tasklist_id.clone(),
                                status: status_str.to_string(),
                                owner: Some(owner.clone()),
                                project_id: project_id.clone(),
                            },
                        )
                        .await;
                }
            }
            for new_task_id in &new_task_ids {
                for channel in &channels {
                    runner
                        .event_bus
                        .emit(
                            &synth_run_id,
                            channel,
                            None,
                            AgentEventPayload::TasklistTaskAdded {
                                team_id: event_team_id.clone(),
                                tasklist_id: tasklist_id.clone(),
                                task_id: new_task_id.clone(),
                                owner: Some(owner.clone()),
                                project_id: project_id.clone(),
                            },
                        )
                        .await;
                }
            }

            // Wake the mailbox poller so the bound co-pilot re-enrolls if the
            // receiving tasklist had previously fully drained. Team-owned only:
            // `TasklistWoke` carries only a team id and the poller resolves it
            // with a team-keyed `get`, so an agent-owned wake would be a no-op.
            // Agent-owned co-pilots re-enrol through wake-on-deliver in
            // `QueueManagerRegistry::submit_message`, which keys off the agent's
            // profile template rather than tasklist ownership.
            if let TasklistOwner::Team { team_id } = &owner {
                crate::tasklist_lifecycle::emit_wake(
                    &runner.event_bus,
                    team_id,
                    &tasklist_id,
                    crate::tasklist_lifecycle::WakeReason::TaskAdded,
                )
                .await;
            }

            // Poke the feeder so an Active tasklist dispatches the new
            // tasks immediately. Paused (revived) tasklists no-op inside
            // advance — the user must Resume manually.
            if let Some(feeder) = runner.task_feeder.get() {
                if let Err(e) = feeder.advance(&updated).await {
                    tracing::error!(
                        owner = ?owner,
                        tasklist_id = %tasklist_id,
                        "TaskFeeder.advance failed after <tasklist append>: {}",
                        e,
                    );
                }
            }

            let count = new_task_ids.len();
            let plural = if count == 1 { "" } else { "s" };
            let revival_note = if revived_to_active {
                " (tasklist auto-resumed to Active)"
            } else if revived_to_paused {
                " (tasklist revived to Paused — Resume to start)"
            } else {
                ""
            };
            let summary = format!(
                "[Appended {count} task{plural} to tasklist '{tasklist_title}'{revival_note}]"
            );
            Ok(Some(WorkflowFollowup {
                context: summary.clone(),
                system_transcript: Some(summary),
            }))
        }
    }
}

#[cfg(test)]
mod tests;
