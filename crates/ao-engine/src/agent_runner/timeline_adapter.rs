use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::skill_registry::{SkillEntry, SkillRegistry};
use ao_engine_tools_core::ToolOutput;
use ao_engine_tools_runner::message::ContentBlock;
use ao_engine_tools_runner::query_loop::{SessionEvent, SessionEventSink};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentId;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::event_bus::EventBus;

struct TurnState {
    current_id: Option<String>,
}

impl TurnState {
    fn new() -> Self {
        Self { current_id: None }
    }

    /// Return the existing turn_id, or generate one on first access.
    ///
    /// A `TimelineAdapter` is constructed per run, so the turn_id seeded
    /// here lives for the lifetime of one user turn. ALL entries written
    /// during that turn — assistant text, tool_use, tool_result, and any
    /// continuation-loop respawn text — share the same turn_id. The
    /// frontend uses this to coalesce a tool-using turn into a single
    /// chat bubble (the single-bubble continuation contract).
    ///
    /// Previously this reset after every tool_result, which forced the
    /// CLI continuation loop to write two separate response entries with
    /// different turn_ids and produced the two-bubble bug observed in
    /// dogfood. The reset is gone; new turns are bounded by adapter
    /// construction (i.e. the start of the next user-prompt run).
    fn get_or_create_id(&mut self) -> String {
        if self.current_id.is_none() {
            let new_id = Uuid::new_v4().to_string();
            self.current_id = Some(new_id.clone());
            new_id
        } else {
            self.current_id.clone().unwrap()
        }
    }

    fn current_id_only(&self) -> Option<String> {
        self.current_id.clone()
    }

    /// Marker call kept as a no-op so callers don't need a code change.
    /// Tool-result arrival no longer rotates the turn_id — see
    /// [`get_or_create_id`] for the new contract.
    fn on_tool_result(&mut self) {
        // intentionally a no-op
    }
}

struct ToolCallRecord {
    name: String,
    turn_id: String,
}

/// Derive an optional native chip label for a tool call before emitting.
///
/// Returns `Some(label)` only for tool calls where the backend has information
/// the frontend cannot derive from `tool_input` alone. Currently handles
/// `RunSkill` by resolving the skill's canonical registry name, which may
/// differ from the raw input slug (e.g. for MCP qualified form
/// `"server:skill-name"` → `"skill-name"`).
///
/// Returns `None` for all other tools so the frontend falls back to its own
/// Layer-1 label derivation.
fn native_progress_label(
    tool_name: &str,
    input: &Value,
    skill_registry: Option<&SkillRegistry>,
) -> Option<String> {
    if tool_name != "RunSkill" {
        return None;
    }
    let raw_skill = input.get("skill")?.as_str()?;
    let skill_name = raw_skill.strip_prefix('/').unwrap_or(raw_skill);
    let registry = skill_registry?;
    // Mirror RunSkill's resolution: exact name first, then qualified plugin/MCP form.
    let record = match registry.get(skill_name) {
        Some(SkillEntry::Ok(r)) => r,
        _ => {
            // Qualified `server:name` form — find by plain name + matching source.
            let (_, plain_name) = skill_name.split_once(':')?;
            registry
                .all_visible()
                .find_map(|(_, e)| {
                    if let SkillEntry::Ok(r) = e { if r.name == plain_name { return Some(r); } }
                    None
                })?
        }
    };
    Some(format!("Loading skill: {}", record.name))
}

pub struct TimelineAdapter {
    run_id: String,
    agent_id: AgentId,
    /// Channel id used for event-bus emissions when this run must not stream
    /// into the agent's own chat feed. Delegated child runs set this to a
    /// delegate-scoped channel (e.g. `delegate:<delegation_id>`); `None`
    /// emits on `agent_id` as usual. Persistence and transcript-entry roles
    /// always use `agent_id` regardless — only the live stream is rerouted.
    event_channel: Option<String>,
    thread_id: Option<String>,
    event_bus: Arc<EventBus>,
    /// Optional persistence layer for writing transcript entries.
    /// None when TimelineAdapter is used without persistence.
    pub persistence: Option<Arc<PersistenceLayer>>,
    /// Where this run's transcript entries are written: `None` routes to the
    /// agent-keyed personal file, `Some(path)` routes elsewhere (a per-thread
    /// file for an ordinary secondary-thread conversation, or a per-task/
    /// sidechain file for dispatched/delegated work). Purely a file-routing
    /// concern — it does NOT by itself imply the run is background/hidden;
    /// see `suppress_visible_snapshot` for that decision.
    pub bg_transcript_override: Option<PathBuf>,
    /// Set by the caller when this run must NOT mutate the agent's visible
    /// chat-list snapshot (last_message / last_activity_at /
    /// last_agent_activity_at / message_count) — genuinely background or
    /// dispatched work (team/tasklist/project scope, isolated delegate
    /// children) whose content the user never sees in the agent's own chat
    /// thread. Deliberately independent of `bg_transcript_override`: an
    /// ordinary secondary-thread conversation also writes through an
    /// override path (that thread's own file) but is fully user-visible and
    /// must still update the preview. Callers should derive this from
    /// `RunScope::suppresses_visible_snapshot`. Defaults to `false`.
    suppress_visible_snapshot: bool,
    /// Model identity that authored this run's turns. Stamped onto persisted
    /// `thinking`/`redacted_thinking` entries so reconstruction can tell whether
    /// a reasoning block's signature is still valid for the model that will
    /// replay it. `None` when the agent uses the provider's default model.
    model: Option<String>,
    /// Non-secret fingerprint of the API key active during this run. Anthropic
    /// signatures are bound to both the model and the API key, so a key rotation
    /// invalidates a persisted block even when the model is unchanged. Stamped
    /// alongside `model` so reconstruction can detect the mismatch and drop the
    /// block rather than replaying a stale signature into a 400.
    key_fingerprint: Option<String>,
    text_buffer: Mutex<String>,
    tool_calls: Mutex<HashMap<String, ToolCallRecord>>,
    turn_state: Mutex<TurnState>,
    /// Transcript entries accumulated during emit() calls, drained by persist_pending().
    pending_entries: Mutex<Vec<TranscriptEntry>>,
    /// Last assistant text emitted — used to update the agent snapshot. Set when
    /// text is flushed; consumed in persist_pending().
    pending_snapshot_message: Mutex<Option<String>>,
    /// Snapshot of the skill registry at session start. Used to resolve a
    /// human-readable label for RunSkill chip events without touching the
    /// RunnerContext (which is constructed after the adapter).
    skill_registry: Option<Arc<SkillRegistry>>,
}

impl TimelineAdapter {
    pub fn new(
        run_id: String,
        agent_id: AgentId,
        thread_id: Option<String>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            run_id,
            agent_id,
            event_channel: None,
            thread_id,
            event_bus,
            persistence: None,
            bg_transcript_override: None,
            suppress_visible_snapshot: false,
            model: None,
            key_fingerprint: None,
            text_buffer: Mutex::new(String::new()),
            tool_calls: Mutex::new(HashMap::new()),
            turn_state: Mutex::new(TurnState::new()),
            pending_entries: Mutex::new(Vec::new()),
            pending_snapshot_message: Mutex::new(None),
            skill_registry: None,
        }
    }

    /// Attach a persistence layer for inline transcript writes.
    pub fn with_persistence(
        mut self,
        persistence: Arc<PersistenceLayer>,
        bg_transcript_override: Option<PathBuf>,
    ) -> Self {
        self.persistence = Some(persistence);
        self.bg_transcript_override = bg_transcript_override;
        self
    }

    /// Opt this run out of visible-snapshot updates in `persist_pending()`.
    /// Call for genuinely background/dispatched work — see the field doc on
    /// `suppress_visible_snapshot` and `RunScope::suppresses_visible_snapshot`
    /// for which runs qualify. Do NOT call this just because the run also
    /// happens to pass a `bg_transcript_override` path; that field alone no
    /// longer implies suppression (an ordinary secondary-thread conversation
    /// sets an override path too, but must keep updating the preview).
    pub fn suppress_visible_snapshot(mut self) -> Self {
        self.suppress_visible_snapshot = true;
        self
    }

    /// Record the model identity driving this run so reasoning blocks persisted
    /// during it can be attributed to the model that signed them. Reconstruction
    /// compares this against the resuming run's model and drops any reasoning
    /// block whose signature would be replayed to a different model (Anthropic
    /// signatures are model-bound and 400 on mismatch).
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Record the API key fingerprint for this run. Stamped onto persisted
    /// `thinking`/`redacted_thinking` entries alongside `model` so reconstruction
    /// can detect a key rotation and drop the stale block rather than replaying
    /// its invalidated signature. The raw key must never be passed here — only a
    /// pre-computed non-secret hash (see `ProviderClient::key_fingerprint`).
    pub fn with_key_fingerprint(mut self, fingerprint: Option<String>) -> Self {
        self.key_fingerprint = fingerprint;
        self
    }

    /// Attach a skill registry snapshot so `RunSkill` chip events can carry a
    /// resolved label rather than the raw input slug.
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// Reroute live event-bus emissions to a dedicated channel. Used by
    /// delegated child runs so their streaming output never renders in the
    /// owning agent's chat feed. Transcript persistence is unaffected.
    pub fn with_event_channel(mut self, channel: Option<String>) -> Self {
        self.event_channel = channel;
        self
    }

    /// The channel id live events are emitted on: the override when set,
    /// otherwise the agent's own id.
    fn bus_channel(&self) -> &AgentId {
        self.event_channel.as_ref().unwrap_or(&self.agent_id)
    }

    fn bus_emit(&self, payload: AgentEventPayload) {
        self.event_bus.emit_sync(
            &self.run_id,
            self.bus_channel(),
            self.thread_id.clone(),
            payload,
        );
    }

    /// Flush the text buffer, emit `TextComplete`, and return the flushed text.
    /// Returns `None` when the buffer was already empty.
    fn flush_text_inner(
        buffer: &mut String,
        run_id: &str,
        agent_id: &AgentId,
        thread_id: Option<String>,
        event_bus: &EventBus,
    ) -> Option<String> {
        if !buffer.is_empty() {
            let text = std::mem::take(buffer);
            event_bus.emit_sync(run_id, agent_id, thread_id, AgentEventPayload::TextComplete { text: text.clone() });
            Some(text)
        } else {
            None
        }
    }

    /// Flush any accumulated text as `TextComplete`. Called by `NativeAgentRunner` at
    /// the end of a turn before emitting `RunEnded`.
    /// Accumulates a pending response transcript entry if text was flushed.
    pub fn flush_text(&self) {
        let flushed = {
            let mut buf = self.text_buffer.lock().expect("text_buffer poisoned");
            Self::flush_text_inner(
                &mut buf,
                &self.run_id,
                self.bus_channel(),
                self.thread_id.clone(),
                &self.event_bus,
            )
        };
        if let Some(text) = flushed {
            let turn_id = self.turn_state
                .lock()
                .expect("turn_state poisoned")
                .current_id_only()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            self.queue_response_entry(text.clone(), turn_id, false);
            *self.pending_snapshot_message.lock().expect("pending_snapshot_message poisoned") = Some(text);
        }
    }

    /// Write all pending transcript entries to disk and update the agent snapshot.
    /// Called by `NativeAgentRunner` after `flush_text()` and before emitting `RunEnded`.
    /// No-op when persistence is None.
    pub async fn persist_pending(&self) {
        let persistence = match &self.persistence {
            Some(p) => Arc::clone(p),
            None => return,
        };
        let entries: Vec<TranscriptEntry> = {
            let mut guard = self.pending_entries.lock().expect("pending_entries poisoned");
            std::mem::take(&mut *guard)
        };
        let override_path = self.bg_transcript_override.clone();
        for entry in &entries {
            if let Err(e) = persistence.transcripts.append_for_run(
                &self.agent_id,
                override_path.as_deref(),
                entry,
            ).await {
                tracing::error!(agent_id = %self.agent_id, "Failed to persist transcript entry: {}", e);
            }
        }
        let snapshot_message = {
            let mut guard = self.pending_snapshot_message.lock().expect("pending_snapshot_message poisoned");
            guard.take()
        };
        if let Some(last_message) = snapshot_message {
            // Only update the user-facing snapshot when this run isn't flagged
            // as background/dispatched work. This is deliberately independent
            // of `bg_transcript_override`: an ordinary secondary-thread
            // conversation also writes through an override path (that
            // thread's own file) but is fully user-visible and must still
            // update the preview. Only `suppress_visible_snapshot` (set for
            // team/tasklist/project-scoped runs and isolated delegate
            // children — see `RunScope::suppresses_visible_snapshot`) skips
            // this: those runs' content never surfaces in the agent's own
            // chat thread, so mutating the preview here would show a phantom
            // last-message the user never actually saw there.
            if !self.suppress_visible_snapshot {
                if let Err(e) = persistence.snapshots.update_agent_entry(&self.agent_id, |entry| {
                    entry.message_count += 1;
                    entry.last_activity_at = Some(Utc::now());
                    entry.last_agent_activity_at = Some(Utc::now());
                    entry.last_message = Some(last_message.clone());
                    // Same event as `last_message` above — set in the same
                    // closure so the pair can never diverge. `self.thread_id`
                    // is `None` for the default thread, `Some(id)` otherwise,
                    // covering both native and CLI runner modes (both
                    // construct this adapter with the run's concrete thread).
                    entry.last_message_thread_id = self.thread_id.clone();
                }).await {
                    tracing::error!(agent_id = %self.agent_id, "Failed to update agent snapshot: {}", e);
                }
            }
        }
    }

    fn queue_response_entry(&self, text: String, turn_id: String, hidden: bool) {
        if self.persistence.is_none() {
            return;
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: self.agent_id.clone() },
            content: text,
            event_type: "response".to_string(),
            metadata: Some({
                let mut m = HashMap::new();
                m.insert("turn_id".to_string(), Value::String(turn_id));
                m
            }),
            hidden_from_user: hidden,
        };
        self.pending_entries.lock().expect("pending_entries poisoned").push(entry);
    }

    fn queue_tool_use_entry(&self, tool_use_id: String, name: String, input: Value, turn_id: String) {
        if self.persistence.is_none() {
            return;
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: self.agent_id.clone() },
            content: String::new(),
            event_type: "tool_use".to_string(),
            metadata: Some({
                let mut m = HashMap::new();
                m.insert("tool_use_id".to_string(), Value::String(tool_use_id));
                m.insert("tool_name".to_string(), Value::String(name));
                m.insert("input".to_string(), input);
                m.insert("turn_id".to_string(), Value::String(turn_id));
                m
            }),
            hidden_from_user: false,
        };
        self.pending_entries.lock().expect("pending_entries poisoned").push(entry);
    }

    /// Build a hidden user-role transcript entry for a synthesized message
    /// (e.g. an inline `RunSkill` body the runner injected between two
    /// agent turns). Persisted with `hidden_from_user: true` so the user
    /// chat stream stays clean, but bus-emitted via
    /// [`AgentEventPayload::HiddenTranscriptEntry`] so the frontend can
    /// coalesce the matching skill-load chip into the surrounding agent
    /// bubble in real time (see `parseSkillLoadInfo` /
    /// `SKILL_LOAD_LOADED_RE` on the client). Caller guarantees the
    /// content is already wrapped in whichever marker shape the frontend
    /// parses — this helper is shape-agnostic.
    fn queue_hidden_user_entry(&self, content: String) {
        // Bus-emit first so the chip can coalesce into the in-flight
        // bubble; persist afterwards so a reload sees the same shape.
        // User role on the wire is `"role":"user"` — the untagged enum
        // serializes `System("user")` as just `"user"`, which is what the
        // frontend `isUserMessage` helper expects.
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content,
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: true,
        };
        self.bus_emit(AgentEventPayload::HiddenTranscriptEntry {
            entry: entry.clone(),
        });
        if self.persistence.is_some() {
            self.pending_entries
                .lock()
                .expect("pending_entries poisoned")
                .push(entry);
        }
    }

    fn queue_tool_result_entry(
        &self,
        tool_use_id: String,
        output: String,
        is_error: bool,
        turn_id: String,
    ) {
        if self.persistence.is_none() {
            return;
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("tool".to_string()),
            content: String::new(),
            event_type: "tool_result".to_string(),
            metadata: Some({
                let mut m = HashMap::new();
                m.insert("tool_use_id".to_string(), Value::String(tool_use_id));
                m.insert("output".to_string(), Value::String(output));
                m.insert("is_error".to_string(), Value::Bool(is_error));
                m.insert("turn_id".to_string(), Value::String(turn_id));
                m
            }),
            hidden_from_user: false,
        };
        self.pending_entries.lock().expect("pending_entries poisoned").push(entry);
    }

    /// Write a `"thinking"` transcript entry for a completed signed reasoning
    /// block. The block is serialized as a single `block_json` value (TB-4 literal
    /// storage) so reconstruction can deserialize it without re-splitting fields.
    /// `model` and `key_fingerprint` are stored alongside for the drop-on-mismatch
    /// check in `history::to_messages`.
    fn queue_thinking_entry(
        &self,
        text: Option<String>,
        signature: Option<String>,
        turn_id: String,
    ) {
        if self.persistence.is_none() {
            return;
        }
        let mut m = HashMap::new();
        m.insert("turn_id".to_string(), Value::String(turn_id));
        // Store the complete block as a nested JSON value so round-trips are
        // byte-identical regardless of field ordering or future schema additions.
        let block = ContentBlock::Thinking { text, signature };
        m.insert(
            "block_json".to_string(),
            serde_json::to_value(&block).unwrap_or(Value::Null),
        );
        if let Some(model) = &self.model {
            m.insert("model".to_string(), Value::String(model.clone()));
        }
        if let Some(fp) = &self.key_fingerprint {
            m.insert("key_fingerprint".to_string(), Value::String(fp.clone()));
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: self.agent_id.clone() },
            content: String::new(),
            event_type: "thinking".to_string(),
            metadata: Some(m),
            hidden_from_user: false,
        };
        self.pending_entries
            .lock()
            .expect("pending_entries poisoned")
            .push(entry);
    }

    /// Write a `"redacted_thinking"` transcript entry for an opaque reasoning
    /// block. Same `block_json` literal-storage contract as `queue_thinking_entry`
    /// — the `data` blob is embedded in the serialized block value so
    /// reconstruction can deserialize it without field-splitting.
    fn queue_redacted_thinking_entry(&self, data: String, turn_id: String) {
        if self.persistence.is_none() {
            return;
        }
        let mut m = HashMap::new();
        m.insert("turn_id".to_string(), Value::String(turn_id));
        let block = ContentBlock::RedactedThinking { data };
        m.insert(
            "block_json".to_string(),
            serde_json::to_value(&block).unwrap_or(Value::Null),
        );
        if let Some(model) = &self.model {
            m.insert("model".to_string(), Value::String(model.clone()));
        }
        if let Some(fp) = &self.key_fingerprint {
            m.insert("key_fingerprint".to_string(), Value::String(fp.clone()));
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: self.agent_id.clone() },
            content: String::new(),
            event_type: "redacted_thinking".to_string(),
            metadata: Some(m),
            hidden_from_user: false,
        };
        self.pending_entries
            .lock()
            .expect("pending_entries poisoned")
            .push(entry);
    }

    // ─── CLI-path record helpers ──────────────────────────────────────────────
    //
    // These methods let CliAgentRunner queue transcript entries without going
    // through SessionEventSink::emit (which would double-emit events to the bus,
    // since the CLI drain loop already emits them). persist_pending() writes the
    // queued entries once at the end of the run.

    /// Queue a terminal text response for persistence. Called by CliAgentRunner
    /// at the point where the legacy code called `append_for_run` for an agent
    /// response. No-op when persistence is None (team runs).
    pub fn record_text_complete(&self, text: &str) {
        if self.persistence.is_none() {
            return;
        }
        let turn_id = self.turn_state.lock().expect("turn_state poisoned").get_or_create_id();
        self.queue_response_entry(text.to_string(), turn_id, false);
        *self.pending_snapshot_message.lock().expect("pending_snapshot_message poisoned") =
            Some(text.to_string());
    }

    /// Queue a turn whose visible text was fully consumed by structural tag
    /// stripping (e.g. an agent's entire tasklist response was a `<task
    /// action="...">` completion tag, leaving nothing to show in the chat
    /// feed) as a hidden agent-role entry. Keeps the per-tasklist transcript
    /// an accurate record of every turn — including ones with no user-visible
    /// content — without resurrecting the stripped text into the live chat or
    /// updating the agent's snapshot preview. No-op when persistence is None
    /// (team runs) or `raw_text` is blank.
    pub fn record_suppressed_text_complete(&self, raw_text: &str) {
        if self.persistence.is_none() || raw_text.trim().is_empty() {
            return;
        }
        let turn_id = self.turn_state.lock().expect("turn_state poisoned").get_or_create_id();
        self.queue_response_entry(raw_text.to_string(), turn_id, true);
    }

    /// Queue a synthesized hidden user-role message for persistence AND
    /// bus-emit `HiddenTranscriptEntry`. Called by `CliAgentRunner` after
    /// the per-step dispatch loop drains `pending_user_messages` (e.g.
    /// an inline `RunSkill` body). Mirrors `SessionEvent::HiddenUserMessage`
    /// on the API path; same shape lands in both runners.
    pub fn record_hidden_user_message(&self, content: &str) {
        self.queue_hidden_user_entry(content.to_string());
    }

    /// Queue an XML tool-use entry for persistence. Called by CliAgentRunner
    /// before dispatching each XML tool call. No-op when persistence is None.
    pub fn record_xml_tool_use(&self, tool_use_id: &str, name: &str, input: Value) {
        if self.persistence.is_none() {
            return;
        }
        let turn_id = self.turn_state.lock().expect("turn_state poisoned").get_or_create_id();
        self.tool_calls.lock().expect("tool_calls poisoned").insert(
            tool_use_id.to_string(),
            ToolCallRecord { name: name.to_string(), turn_id: turn_id.clone() },
        );
        self.queue_tool_use_entry(tool_use_id.to_string(), name.to_string(), input, turn_id);
    }

    /// Queue an XML tool-result entry for persistence. Called by CliAgentRunner
    /// after each XML tool dispatch. No-op when persistence is None.
    pub fn record_xml_tool_result(&self, tool_use_id: &str, output: &str, is_error: bool) {
        if self.persistence.is_none() {
            return;
        }
        let call_turn_id = {
            let calls = self.tool_calls.lock().expect("tool_calls poisoned");
            calls.get(tool_use_id)
                .map(|r| r.turn_id.clone())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        tool_use_id = %tool_use_id,
                        "record_xml_tool_result: unknown tool_use_id"
                    );
                    Uuid::new_v4().to_string()
                })
        };
        self.turn_state.lock().expect("turn_state poisoned").on_tool_result();
        self.queue_tool_result_entry(
            tool_use_id.to_string(),
            output.to_string(),
            is_error,
            call_turn_id,
        );
    }
}

impl SessionEventSink for TimelineAdapter {
    /// The turn id every entry in this run is persisted under (one per run
    /// since the tool-result reset was removed — see [`TurnState`]). Returns
    /// `Some` once the first assistant text or tool_use of the run has seeded
    /// it, which is always the case by the time `run_session` executes a tool
    /// call. `ArtifactWrite` stamps this onto `source_message_id` so the thread
    /// bubble (which carries the same `turn_id` in its metadata) can resolve
    /// the produced artifact inline.
    fn current_turn_id(&self) -> Option<String> {
        self.turn_state
            .lock()
            .expect("turn_state poisoned")
            .current_id_only()
    }

    fn emit(&self, event: SessionEvent) {
        match event {
            SessionEvent::AssistantText(s) => {
                // Ensure turn_id is seeded at the start of each assistant turn.
                self.turn_state.lock().expect("turn_state poisoned").get_or_create_id();
                self.bus_emit(AgentEventPayload::TextDelta { text: s.clone() });
                self.text_buffer
                    .lock()
                    .expect("text_buffer poisoned")
                    .push_str(&s);
            }
            SessionEvent::ToolUse { id, name, input } => {
                // Get (or generate) the current turn_id before flushing text so the
                // response entry and the tool_use entry share the same turn_id.
                let turn_id = self.turn_state.lock().expect("turn_state poisoned").get_or_create_id();

                // Flush buffered assistant text (emits TextComplete).
                let flushed = {
                    let mut buf = self.text_buffer.lock().expect("text_buffer poisoned");
                    Self::flush_text_inner(
                        &mut buf,
                        &self.run_id,
                        self.bus_channel(),
                        self.thread_id.clone(),
                        &self.event_bus,
                    )
                };
                // Queue response and snapshot update if text was flushed.
                if let Some(text) = flushed {
                    self.queue_response_entry(text.clone(), turn_id.clone(), false);
                    *self.pending_snapshot_message.lock().expect("pending_snapshot_message poisoned") = Some(text);
                }

                // Store record for ToolResult lookup.
                self.tool_calls
                    .lock()
                    .expect("tool_calls poisoned")
                    .insert(id.clone(), ToolCallRecord { name: name.clone(), turn_id: turn_id.clone() });

                self.bus_emit(AgentEventPayload::ToolCallStarted {
                    tool_name: name.clone(),
                    tool_input: Some(input.clone()),
                    label: native_progress_label(&name, &input, self.skill_registry.as_deref()),
                    tool_use_id: Some(id.clone()),
                });

                // Queue the tool_use transcript entry.
                self.queue_tool_use_entry(id, name, input, turn_id);
            }
            SessionEvent::ToolResult { tool_use_id, output } => {
                let is_error = matches!(&output, ToolOutput::Error { .. });
                let output_text = output.as_text();

                let (tool_name, call_turn_id) = {
                    let calls = self.tool_calls.lock().expect("tool_calls poisoned");
                    calls.get(&tool_use_id).map(|r| (r.name.clone(), r.turn_id.clone())).unwrap_or_else(|| {
                        tracing::warn!(
                            tool_use_id = %tool_use_id,
                            "TimelineAdapter: unknown tool_use_id in ToolResult"
                        );
                        ("unknown".to_string(), Uuid::new_v4().to_string())
                    })
                };

                // Mark turn boundary: next AssistantText/ToolUse starts a fresh turn_id.
                self.turn_state.lock().expect("turn_state poisoned").on_tool_result();

                // Queue the tool_result transcript entry.
                self.queue_tool_result_entry(
                    tool_use_id.clone(),
                    output_text.clone(),
                    is_error,
                    call_turn_id,
                );

                self.bus_emit(AgentEventPayload::ToolCallCompleted {
                    tool_name,
                    output: Some(output_text),
                    tool_use_id: Some(tool_use_id),
                    is_error,
                });
            }
            SessionEvent::HiddenUserMessage { content } => {
                // Flush any in-flight assistant text first so the upcoming
                // hidden entry doesn't visually "fall inside" an unfinished
                // bubble — the chip should land between turns, not mid-stream.
                let flushed = {
                    let mut buf = self.text_buffer.lock().expect("text_buffer poisoned");
                    Self::flush_text_inner(
                        &mut buf,
                        &self.run_id,
                        self.bus_channel(),
                        self.thread_id.clone(),
                        &self.event_bus,
                    )
                };
                if let Some(text) = flushed {
                    let turn_id = self
                        .turn_state
                        .lock()
                        .expect("turn_state poisoned")
                        .get_or_create_id();
                    self.queue_response_entry(text.clone(), turn_id, false);
                    *self
                        .pending_snapshot_message
                        .lock()
                        .expect("pending_snapshot_message poisoned") = Some(text);
                }
                self.queue_hidden_user_entry(content);
            }
            SessionEvent::ThinkingStart => {
                // Mount the "Thinking…" indicator. Emit even when the provider
                // is configured with `display = "omitted"` and no deltas will
                // follow — the start/end pair alone is a valid signal that the
                // model engaged its reasoning channel.
                self.bus_emit(AgentEventPayload::ThinkingStarted);
            }
            SessionEvent::ThinkingDelta { text } => {
                self.bus_emit(AgentEventPayload::ThinkingDelta { text });
            }
            SessionEvent::ThinkingEnd { elapsed_ms } => {
                self.bus_emit(AgentEventPayload::ThinkingEnded { elapsed_ms });
            }
            SessionEvent::ThinkingBlock { text, signature } => {
                // Persist the completed thinking block to the transcript so it
                // survives the persist→reload→replay round-trip. Thinking blocks
                // arrive in the stream BEFORE any assistant text or tool-use, so
                // we seed the turn_id here — the response/tool_use entries that
                // follow will share it. No bus event: ThinkingStart/Delta/End
                // already covered the UI surface.
                let turn_id = self
                    .turn_state
                    .lock()
                    .expect("turn_state poisoned")
                    .get_or_create_id();
                self.queue_thinking_entry(text, signature, turn_id);
            }
            SessionEvent::RedactedThinkingBlock { data } => {
                // Same persistence contract as ThinkingBlock. No bus event:
                // redacted blocks have no readable reasoning to surface in
                // the UI.
                let turn_id = self
                    .turn_state
                    .lock()
                    .expect("turn_state poisoned")
                    .get_or_create_id();
                self.queue_redacted_thinking_entry(data, turn_id);
            }
            SessionEvent::FormPosted { form_id, spec } => {
                self.bus_emit(AgentEventPayload::FormPosted { form_id, spec: spec.into() });
            }
            SessionEvent::Usage(usage) => {
                let cache_read = usage.cache_read.unwrap_or(0);
                let cache_creation = usage.cache_creation.unwrap_or(0);
                // Cache-token observability. Greppable as:
                //   ao_engine::cache usage agent_id=… run_id=… input=… output=… cache_read=… cache_creation=…
                // The cache-anchor work uses this signal to confirm that the
                // `messages[]` prefix is stable across turns: cache_creation > 0
                // on the first turn of a session (cache write), cache_read > 0
                // on subsequent turns (cache hit). A persistent `cache_read = 0`
                // pattern across turns is the symptom we're fixing.
                tracing::info!(
                    target: "ao_engine::cache",
                    agent_id = %self.agent_id,
                    run_id = %self.run_id,
                    input = usage.input_tokens,
                    output = usage.output_tokens,
                    cache_read = cache_read,
                    cache_creation = cache_creation,
                    "cache usage",
                );
                self.bus_emit(AgentEventPayload::Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: cache_read,
                    cache_creation_tokens: cache_creation,
                    total_tokens: usage.input_tokens + usage.output_tokens + cache_read,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::ToolOutput as CoreToolOutput;
    use ao_engine_tools_runner::query_loop::SessionEvent;
    use ao_engine_tools_runner::provider::Usage;
    use ao_persistence::paths::DataRoot;
    use ao_protocol::event::{AgentEvent, AgentEventPayload};
    use serde_json::json;
    use tempfile::TempDir;

    fn make_adapter(bus: &Arc<EventBus>) -> TimelineAdapter {
        TimelineAdapter::new(
            "run-test".to_string(),
            "agent-test".to_string(),
            None,
            Arc::clone(bus),
        )
    }

    /// Build a real PersistenceLayer backed by a temporary directory.
    async fn make_test_persistence() -> (Arc<PersistenceLayer>, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.expect("ensure_directories");
        let persistence = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
        (Arc::new(persistence), tmp)
    }

    /// Drain all currently queued events from the broadcast receiver (non-blocking).
    fn drain(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEventPayload> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => out.push(e.payload),
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn text_aggregation_emits_delta_then_complete() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        SessionEventSink::emit(&adapter, SessionEvent::AssistantText("a".into()));
        SessionEventSink::emit(&adapter, SessionEvent::AssistantText("b".into()));
        SessionEventSink::emit(&adapter, SessionEvent::AssistantText("c".into()));
        SessionEventSink::emit(&adapter, SessionEvent::ToolUse {
            id: "1".to_string(),
            name: "Read".to_string(),
            input: json!({}),
        });

        let events = drain(&mut rx);
        assert_eq!(events.len(), 5);
        assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "a"));
        assert!(matches!(&events[1], AgentEventPayload::TextDelta { text } if text == "b"));
        assert!(matches!(&events[2], AgentEventPayload::TextDelta { text } if text == "c"));
        assert!(matches!(&events[3], AgentEventPayload::TextComplete { text } if text == "abc"));
        assert!(matches!(&events[4], AgentEventPayload::ToolCallStarted { tool_name, .. } if tool_name == "Read"));
    }

    #[test]
    fn multi_tool_turn_lookup() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        SessionEventSink::emit(&adapter, SessionEvent::ToolUse {
            id: "id-1".to_string(),
            name: "Read".to_string(),
            input: json!({}),
        });
        SessionEventSink::emit(&adapter, SessionEvent::ToolUse {
            id: "id-2".to_string(),
            name: "Write".to_string(),
            input: json!({}),
        });
        SessionEventSink::emit(&adapter, SessionEvent::ToolResult {
            tool_use_id: "id-1".to_string(),
            output: CoreToolOutput::text("content"),
        });
        SessionEventSink::emit(&adapter, SessionEvent::ToolResult {
            tool_use_id: "id-2".to_string(),
            output: CoreToolOutput::text("written"),
        });

        let events = drain(&mut rx);
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], AgentEventPayload::ToolCallStarted { tool_name, .. } if tool_name == "Read"));
        assert!(matches!(&events[1], AgentEventPayload::ToolCallStarted { tool_name, .. } if tool_name == "Write"));
        assert!(matches!(&events[2], AgentEventPayload::ToolCallCompleted { tool_name, .. } if tool_name == "Read"));
        assert!(matches!(&events[3], AgentEventPayload::ToolCallCompleted { tool_name, .. } if tool_name == "Write"));
    }

    /// Pins the full SessionEvent → AgentEventPayload mapping for a scripted tool turn:
    /// text → tool-use (with buffer flush) → tool-result → text → flush.
    #[test]
    fn scripted_tool_turn_pins_full_mapping() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        // pre-tool assistant text
        SessionEventSink::emit(&adapter, SessionEvent::AssistantText("I'll grep that.".into()));
        // tool use — must flush the accumulated text buffer before emitting ToolCallStarted
        SessionEventSink::emit(&adapter, SessionEvent::ToolUse {
            id: "tu_1".to_string(),
            name: "Grep".to_string(),
            input: json!({ "pattern": "NativeAgentRunner", "path": "src" }),
        });
        // tool result — looked up by id
        SessionEventSink::emit(&adapter, SessionEvent::ToolResult {
            tool_use_id: "tu_1".to_string(),
            output: CoreToolOutput::text("3 matches found"),
        });
        // post-tool assistant text
        SessionEventSink::emit(&adapter, SessionEvent::AssistantText("Found 3 matches.".into()));
        // NativeAgentRunner calls flush_text() at the end of a turn
        adapter.flush_text();

        let events = drain(&mut rx);
        assert_eq!(events.len(), 6);

        // [0] TextDelta from first AssistantText
        assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "I'll grep that."));

        // [1] TextComplete flushed on ToolUse arrival
        assert!(matches!(&events[1], AgentEventPayload::TextComplete { text } if text == "I'll grep that."));

        // [2] ToolCallStarted with correct name and input
        match &events[2] {
            AgentEventPayload::ToolCallStarted { tool_name, tool_input, .. } => {
                assert_eq!(tool_name, "Grep");
                let input = tool_input.as_ref().expect("tool_input must be Some");
                assert_eq!(input["pattern"], "NativeAgentRunner");
            }
            other => panic!("expected ToolCallStarted, got {:?}", other),
        }

        // [3] ToolCallCompleted with correct name and output text
        match &events[3] {
            AgentEventPayload::ToolCallCompleted { tool_name, output, .. } => {
                assert_eq!(tool_name, "Grep");
                assert_eq!(output.as_deref(), Some("3 matches found"));
            }
            other => panic!("expected ToolCallCompleted, got {:?}", other),
        }

        // [4] TextDelta from second AssistantText
        assert!(matches!(&events[4], AgentEventPayload::TextDelta { text } if text == "Found 3 matches."));

        // [5] TextComplete from explicit flush_text()
        assert!(matches!(&events[5], AgentEventPayload::TextComplete { text } if text == "Found 3 matches."));
    }

    /// Thinking SessionEvents must translate 1:1 to the canonical
    /// `AgentEventPayload::ThinkingStarted` → `ThinkingDelta` → `ThinkingEnded`
    /// triplet, with the same payload semantics that the CLI normalizer
    /// already emits. Pins the native (API) path to the existing
    /// frontend "Thinking…" pill contract — no separate UI work needed.
    #[test]
    fn thinking_events_round_trip_to_canonical_payloads() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        SessionEventSink::emit(&adapter, SessionEvent::ThinkingStart);
        SessionEventSink::emit(
            &adapter,
            SessionEvent::ThinkingDelta {
                text: "let me think…".into(),
            },
        );
        SessionEventSink::emit(&adapter, SessionEvent::ThinkingEnd { elapsed_ms: 1234 });

        let events = drain(&mut rx);
        assert_eq!(events.len(), 3);
        assert!(
            matches!(&events[0], AgentEventPayload::ThinkingStarted),
            "event[0] should be ThinkingStarted, got: {:?}",
            events[0]
        );
        assert!(
            matches!(&events[1], AgentEventPayload::ThinkingDelta { text } if text == "let me think…"),
            "event[1] should be ThinkingDelta('let me think…'), got: {:?}",
            events[1]
        );
        assert!(
            matches!(
                &events[2],
                AgentEventPayload::ThinkingEnded { elapsed_ms: 1234 }
            ),
            "event[2] should be ThinkingEnded(1234), got: {:?}",
            events[2]
        );
    }

    /// `display = "omitted"` shape: a Start/End pair with zero deltas. The
    /// adapter must still emit `ThinkingStarted` and `ThinkingEnded` so the
    /// UI mounts a "Thinking…" indicator even when no reasoning text will
    /// stream — the indicator collapse on End is the only feedback the
    /// user gets for a reasoning turn whose deltas were suppressed at
    /// the provider level.
    #[test]
    fn thinking_omitted_shape_still_emits_start_and_end() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        SessionEventSink::emit(&adapter, SessionEvent::ThinkingStart);
        SessionEventSink::emit(&adapter, SessionEvent::ThinkingEnd { elapsed_ms: 87 });

        let events = drain(&mut rx);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEventPayload::ThinkingStarted));
        assert!(matches!(
            &events[1],
            AgentEventPayload::ThinkingEnded { elapsed_ms: 87 }
        ));
    }

    #[test]
    fn usage_round_trip() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let adapter = make_adapter(&bus);

        SessionEventSink::emit(&adapter, SessionEvent::Usage(Usage {
            input_tokens: 7,
            output_tokens: 3,
            cache_read: Some(1),
            cache_creation: Some(4),
        }));

        let events = drain(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEventPayload::Usage {
                input_tokens: 7,
                output_tokens: 3,
                cache_read_tokens: 1,
                cache_creation_tokens: 4,
                total_tokens: 11,
            }
        ));
    }

    /// Cache observability: every Usage event must emit an
    /// INFO log line on `ao_engine::cache` carrying all four token fields
    /// (input, output, cache_read, cache_creation) plus agent_id + run_id.
    /// The cache-anchor smoke procedure greps for this line to compare
    /// pre/post anchor cache-hit rates.
    #[test]
    fn usage_emits_cache_observability_log_line() {
        use std::io;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct SharedBufWriter(StdArc<StdMutex<Vec<u8>>>);
        impl io::Write for SharedBufWriter {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        impl<'a> MakeWriter<'a> for SharedBufWriter {
            type Writer = SharedBufWriter;
            fn make_writer(&'a self) -> Self::Writer { self.clone() }
        }

        let buf = StdArc::new(StdMutex::new(Vec::<u8>::new()));
        let writer = SharedBufWriter(StdArc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_target(true)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let bus = Arc::new(EventBus::new(64));
            let adapter = TimelineAdapter::new(
                "run-observability".to_string(),
                "agent-observability".to_string(),
                None,
                Arc::clone(&bus),
            );
            SessionEventSink::emit(&adapter, SessionEvent::Usage(Usage {
                input_tokens: 1234,
                output_tokens: 567,
                cache_read: Some(890),
                cache_creation: Some(42),
            }));
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");
        // Target tag + every field name + every value must appear in the line.
        // Note: `%` (Display) field formatting produces `agent_id=…` without
        // surrounding quotes; quoted form would only appear under `?` (Debug).
        for substring in [
            "ao_engine::cache",
            "agent_id=agent-observability",
            "run_id=run-observability",
            "input=1234",
            "output=567",
            "cache_read=890",
            "cache_creation=42",
            "cache usage",
        ] {
            assert!(
                captured.contains(substring),
                "missing `{}` in captured log line:\n{}",
                substring,
                captured
            );
        }
    }

    /// `cache_creation = None` (delta event before message_start has fired)
    /// must still emit the log line, with `cache_creation=0`. Prevents a
    /// silent gap in the smoke-procedure greppable history.
    #[test]
    fn usage_log_line_defaults_cache_creation_to_zero_when_none() {
        use std::io;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct SharedBufWriter(StdArc<StdMutex<Vec<u8>>>);
        impl io::Write for SharedBufWriter {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        impl<'a> MakeWriter<'a> for SharedBufWriter {
            type Writer = SharedBufWriter;
            fn make_writer(&'a self) -> Self::Writer { self.clone() }
        }

        let buf = StdArc::new(StdMutex::new(Vec::<u8>::new()));
        let writer = SharedBufWriter(StdArc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let bus = Arc::new(EventBus::new(64));
            let adapter = make_adapter(&bus);
            SessionEventSink::emit(&adapter, SessionEvent::Usage(Usage {
                input_tokens: 5,
                output_tokens: 2,
                cache_read: None,
                cache_creation: None,
            }));
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");
        assert!(captured.contains("cache_read=0"), "missing cache_read=0:\n{captured}");
        assert!(captured.contains("cache_creation=0"), "missing cache_creation=0:\n{captured}");
    }

    // ─── Persistence tests ────────────────────────────────────────────────────

    /// TextComplete event writes a response TranscriptEntry with correct shape.
    #[tokio::test]
    async fn text_complete_writes_response_transcript_entry() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-1".to_string(), "agent-1".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("hello world".into()));
        adapter.flush_text();
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-1").await.expect("read_all");
        assert_eq!(entries.len(), 1, "expected exactly one transcript entry");
        let entry = &entries[0];
        assert_eq!(entry.event_type, "response");
        assert_eq!(entry.content, "hello world");
        assert!(matches!(&entry.role, ao_protocol::transcript::TranscriptRole::Agent { agent } if agent == "agent-1"));
        let meta = entry.metadata.as_ref().expect("metadata must be present");
        assert!(meta.contains_key("turn_id"), "metadata must contain turn_id");
        assert!(meta["turn_id"].is_string(), "turn_id must be a string");
    }

    /// The `current_turn_id` bridge (read by `run_session` to stamp
    /// `ArtifactWrite`'s `source_message_id`) must return exactly the turn id
    /// the response bubble is persisted under — otherwise a produced artifact
    /// can never resolve inline against its bubble. This asserts that identity
    /// end to end: `None` before the turn is seeded, then equal to the
    /// persisted response entry's `metadata.turn_id` once a tool_use has run.
    #[tokio::test]
    async fn current_turn_id_matches_persisted_response_turn_id() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-turn".to_string(), "agent-turn".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        // Nothing emitted yet → no turn to anchor to.
        assert_eq!(SessionEventSink::current_turn_id(&*adapter), None);

        // A tool-using turn: text, then a tool_use (which seeds/holds the id).
        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("making it".into()));
        SessionEventSink::emit(&*adapter, SessionEvent::ToolUse {
            id: "tu-1".to_string(),
            name: "ArtifactWrite".to_string(),
            input: serde_json::json!({}),
        });

        let bridge_id = SessionEventSink::current_turn_id(&*adapter)
            .expect("turn id must be set once a tool_use has been emitted");

        adapter.persist_pending().await;
        let entries = persistence.transcripts.read_all("agent-turn").await.expect("read_all");
        let response = entries
            .iter()
            .find(|e| e.event_type == "response")
            .expect("a response entry must be persisted");
        let persisted_turn_id = response
            .metadata
            .as_ref()
            .and_then(|m| m.get("turn_id"))
            .and_then(|v| v.as_str())
            .expect("response entry must carry a turn_id");

        assert_eq!(
            bridge_id, persisted_turn_id,
            "current_turn_id (stamped onto the artifact) must equal the bubble's turn_id",
        );
    }

    /// `record_suppressed_text_complete` persists a hidden agent-role entry
    /// even though the caller decided the visible text was empty — this is
    /// what keeps a per-tasklist transcript non-empty for an agent whose
    /// entire turn was consumed by `<task>`/`<task-item-notification>`
    /// stripping.
    #[tokio::test]
    async fn suppressed_text_complete_persists_hidden_entry() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-3".to_string(), "agent-3".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        adapter.record_suppressed_text_complete(
            "<task action=\"complete\" task_id=\"t1\"><task-item-notification>...</task-item-notification></task>",
        );
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-3").await.expect("read_all");
        assert_eq!(entries.len(), 1, "expected exactly one transcript entry");
        let entry = &entries[0];
        assert_eq!(entry.event_type, "response");
        assert!(entry.content.contains("task action=\"complete\""));
        assert!(entry.hidden_from_user, "entry must be hidden from the live chat feed");
        assert!(matches!(&entry.role, ao_protocol::transcript::TranscriptRole::Agent { agent } if agent == "agent-3"));
    }

    /// Blank raw text (e.g. an agent turn with truly nothing) must not
    /// produce an empty transcript entry.
    #[tokio::test]
    async fn suppressed_text_complete_ignores_blank_text() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-4".to_string(), "agent-4".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        adapter.record_suppressed_text_complete("   ");
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-4").await.expect("read_all");
        assert!(entries.is_empty(), "blank text must not be persisted");
    }

    /// ToolCallStarted event writes a tool_use TranscriptEntry with correct shape.
    #[tokio::test]
    async fn tool_call_started_writes_tool_use_transcript_entry() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-2".to_string(), "agent-2".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::ToolUse {
            id: "tu-abc".to_string(),
            name: "Read".to_string(),
            input: json!({ "path": "/tmp/test.txt" }),
        });
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-2").await.expect("read_all");
        assert_eq!(entries.len(), 1, "expected exactly one tool_use entry");
        let entry = &entries[0];
        assert_eq!(entry.event_type, "tool_use");
        assert_eq!(entry.content, "");
        assert!(matches!(&entry.role, ao_protocol::transcript::TranscriptRole::Agent { agent } if agent == "agent-2"));
        let meta = entry.metadata.as_ref().expect("metadata must be present");
        assert_eq!(meta["tool_use_id"], Value::String("tu-abc".to_string()));
        assert_eq!(meta["tool_name"], Value::String("Read".to_string()));
        assert_eq!(meta["input"]["path"], Value::String("/tmp/test.txt".to_string()));
        assert!(meta.contains_key("turn_id"), "metadata must contain turn_id");
    }

    /// ToolCallCompleted event writes a tool_result TranscriptEntry with correct shape.
    #[tokio::test]
    async fn tool_call_completed_writes_tool_result_transcript_entry() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-3".to_string(), "agent-3".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None),
        );

        // Emit ToolUse first so the turn_id is set and the id is registered.
        SessionEventSink::emit(&*adapter, SessionEvent::ToolUse {
            id: "tu-xyz".to_string(),
            name: "Bash".to_string(),
            input: json!({ "command": "ls" }),
        });
        SessionEventSink::emit(&*adapter, SessionEvent::ToolResult {
            tool_use_id: "tu-xyz".to_string(),
            output: CoreToolOutput::text("file1.txt\nfile2.txt"),
        });
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-3").await.expect("read_all");
        assert_eq!(entries.len(), 2, "expected tool_use + tool_result entries; got {:?}", entries.iter().map(|e| &e.event_type).collect::<Vec<_>>());

        let tool_result_entry = entries.iter().find(|e| e.event_type == "tool_result").expect("missing tool_result entry");
        assert_eq!(tool_result_entry.content, "");
        assert!(matches!(&tool_result_entry.role, ao_protocol::transcript::TranscriptRole::System(s) if s == "tool"));
        let meta = tool_result_entry.metadata.as_ref().expect("metadata must be present");
        assert_eq!(meta["tool_use_id"], Value::String("tu-xyz".to_string()));
        assert_eq!(meta["output"], Value::String("file1.txt\nfile2.txt".to_string()));
        assert_eq!(meta["is_error"], Value::Bool(false));
        assert!(meta.contains_key("turn_id"), "metadata must contain turn_id");

        // Verify tool_use and tool_result share the same turn_id.
        let tool_use_entry = entries.iter().find(|e| e.event_type == "tool_use").expect("missing tool_use entry");
        let use_turn_id = tool_use_entry.metadata.as_ref().unwrap()["turn_id"].as_str().unwrap();
        let result_turn_id = meta["turn_id"].as_str().unwrap();
        assert_eq!(use_turn_id, result_turn_id, "tool_use and tool_result must share the same turn_id");
    }

    // ─── CLI-path record helper tests ──────────────────────────────────────────

    /// record_text_complete writes a response TranscriptEntry with correct shape.
    #[tokio::test]
    async fn cli_record_text_complete_writes_response_entry() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = TimelineAdapter::new("run-cli-1".to_string(), "agent-cli-1".to_string(), None, Arc::clone(&bus))
            .with_persistence(Arc::clone(&persistence), None);

        adapter.record_text_complete("hello from CLI");
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-cli-1").await.expect("read_all");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.event_type, "response");
        assert_eq!(e.content, "hello from CLI");
        assert!(matches!(&e.role, ao_protocol::transcript::TranscriptRole::Agent { agent } if agent == "agent-cli-1"));
        assert!(e.metadata.as_ref().map(|m| m.contains_key("turn_id")).unwrap_or(false));
    }

    /// record_xml_tool_use and record_xml_tool_result write correctly shaped entries.
    #[tokio::test]
    async fn cli_record_xml_tool_use_and_result_write_entries() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = TimelineAdapter::new("run-cli-2".to_string(), "agent-cli-2".to_string(), None, Arc::clone(&bus))
            .with_persistence(Arc::clone(&persistence), None);

        adapter.record_xml_tool_use("tu-cli-1", "DateTime", json!({}));
        adapter.record_xml_tool_result("tu-cli-1", "<tool_result>2026-05-11</tool_result>", false);
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-cli-2").await.expect("read_all");
        assert_eq!(entries.len(), 2);

        let tu = entries.iter().find(|e| e.event_type == "tool_use").expect("tool_use entry");
        let meta = tu.metadata.as_ref().expect("tool_use metadata");
        assert_eq!(meta["tool_use_id"], Value::String("tu-cli-1".to_string()));
        assert_eq!(meta["tool_name"], Value::String("DateTime".to_string()));

        let tr = entries.iter().find(|e| e.event_type == "tool_result").expect("tool_result entry");
        let rmeta = tr.metadata.as_ref().expect("tool_result metadata");
        assert_eq!(rmeta["tool_use_id"], Value::String("tu-cli-1".to_string()));
        assert_eq!(rmeta["is_error"], Value::Bool(false));

        // tool_use and tool_result must share the same turn_id.
        let use_turn = meta["turn_id"].as_str().unwrap();
        let res_turn = rmeta["turn_id"].as_str().unwrap();
        assert_eq!(use_turn, res_turn, "turn_ids must match");
    }

    /// Regression: SessionEvent::HiddenUserMessage (API path) MUST
    /// (a) persist a hidden user-role TranscriptEntry whose content matches
    /// the frontend skill-load coalesce regex AND (b) bus-emit
    /// HiddenTranscriptEntry so the live SSE stream coalesces a "Loaded
    /// skill" chip into the in-flight agent bubble in real time. Without
    /// this the IoTool's `enqueue_user_message` is a write-only sink and
    /// the skill body never reaches the next provider turn.
    #[tokio::test]
    async fn hidden_user_message_persists_and_bus_emits() {
        let bus = Arc::new(EventBus::new(64));
        let mut bus_rx = bus.subscribe();
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new(
                "run-bug17-api".to_string(),
                "agent-bug17-api".to_string(),
                None,
                Arc::clone(&bus),
            )
            .with_persistence(Arc::clone(&persistence), None),
        );

        let content = "[skill \"karpathy-guidelines\" loaded]\nBe surgical. Avoid overcomplication.".to_string();
        SessionEventSink::emit(
            &*adapter,
            SessionEvent::HiddenUserMessage { content: content.clone() },
        );
        adapter.persist_pending().await;

        // (a) persisted hidden entry shape.
        let entries = persistence
            .transcripts
            .read_all("agent-bug17-api")
            .await
            .expect("read_all");
        assert_eq!(entries.len(), 1, "expected exactly one persisted entry");
        let e = &entries[0];
        assert!(e.hidden_from_user, "entry must be hidden_from_user=true");
        assert_eq!(e.event_type, "message");
        assert_eq!(e.content, content);
        assert!(
            matches!(&e.role, ao_protocol::transcript::TranscriptRole::System(r) if r == "user"),
            "role must serialize as `\"user\"`; got {:?}",
            e.role
        );

        // (b) bus emission: a HiddenTranscriptEntry event landed.
        let mut got_hidden = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if let AgentEventPayload::HiddenTranscriptEntry { entry } = ev.payload {
                got_hidden = true;
                assert_eq!(entry.content, content);
                assert!(entry.hidden_from_user);
                break;
            }
        }
        assert!(got_hidden, "HiddenTranscriptEntry must be bus-emitted");
    }

    /// Regression — CLI path: `record_hidden_user_message` is the
    /// parallel entry point CliAgentRunner uses after draining
    /// `pending_user_messages` per continuation step. Same on-disk + bus
    /// shape as the API path's SessionEvent handler.
    #[tokio::test]
    async fn cli_record_hidden_user_message_persists_and_bus_emits() {
        let bus = Arc::new(EventBus::new(64));
        let mut bus_rx = bus.subscribe();
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = TimelineAdapter::new(
            "run-bug17-cli".to_string(),
            "agent-bug17-cli".to_string(),
            None,
            Arc::clone(&bus),
        )
        .with_persistence(Arc::clone(&persistence), None);

        let content = "[skill \"karpathy-guidelines\" loaded]\nbody…".to_string();
        adapter.record_hidden_user_message(&content);
        adapter.persist_pending().await;

        let entries = persistence
            .transcripts
            .read_all("agent-bug17-cli")
            .await
            .expect("read_all");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].hidden_from_user);
        assert_eq!(entries[0].content, content);

        let mut got_hidden = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if let AgentEventPayload::HiddenTranscriptEntry { entry } = ev.payload {
                got_hidden = true;
                assert_eq!(entry.content, content);
                break;
            }
        }
        assert!(got_hidden, "HiddenTranscriptEntry must be bus-emitted on the CLI path too");
    }

    /// Failure-mode guard: `record_hidden_user_message` without persistence
    /// (e.g. team runs) still bus-emits the chip — the runtime UX shouldn't
    /// silently drop. The transcript-write side is correctly a no-op.
    #[tokio::test]
    async fn cli_record_hidden_user_message_bus_emits_without_persistence() {
        let bus = Arc::new(EventBus::new(64));
        let mut bus_rx = bus.subscribe();
        // No with_persistence — simulates a team-run TimelineAdapter.
        let adapter = TimelineAdapter::new(
            "run-bug17-team".to_string(),
            "agent-bug17-team".to_string(),
            None,
            Arc::clone(&bus),
        );
        adapter.record_hidden_user_message("[skill \"x\" loaded]\nbody");

        let mut got_hidden = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if matches!(ev.payload, AgentEventPayload::HiddenTranscriptEntry { .. }) {
                got_hidden = true;
                break;
            }
        }
        assert!(
            got_hidden,
            "bus emission must fire even without persistence (live UX must not depend on disk writes)"
        );
    }

    /// record_text_complete is a no-op when persistence is None (team runs).
    #[tokio::test]
    async fn cli_record_text_complete_noop_without_persistence() {
        let bus = Arc::new(EventBus::new(64));
        // No with_persistence call — simulates team run.
        let adapter = TimelineAdapter::new("run-cli-3".to_string(), "agent-cli-3".to_string(), None, Arc::clone(&bus));
        adapter.record_text_complete("team run response");
        // persist_pending is a no-op; no panic.
        adapter.persist_pending().await;
        // No assertions needed beyond "no panic".
    }

    /// Regression: a CLI continuation chain (text → tool_use →
    /// tool_result → MORE text → tool_use → tool_result → final text)
    /// must produce entries that ALL share one turn_id so the frontend
    /// can render the whole tool-using turn as a single bubble.
    /// Previously `on_tool_result()` rotated turn_id, splitting the chain
    /// into multiple bubbles on reload.
    #[tokio::test]
    async fn cli_continuation_chain_shares_single_turn_id() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = TimelineAdapter::new(
            "run-bug12".to_string(),
            "agent-bug12".to_string(),
            None,
            Arc::clone(&bus),
        )
        .with_persistence(Arc::clone(&persistence), None);

        // Step 1: assistant text + tool_use + tool_result.
        adapter.record_text_complete("calling tool 1");
        adapter.record_xml_tool_use("tu-1", "WorkflowActionWriteOutput", json!({"task_id": "t"}));
        adapter.record_xml_tool_result("tu-1", "ok", false);

        // Step 2 (continuation respawn): assistant text + another tool round.
        adapter.record_text_complete("calling tool 2");
        adapter.record_xml_tool_use("tu-2", "WorkflowActionCompletePhase", json!({"task_id": "t"}));
        adapter.record_xml_tool_result("tu-2", "ok", false);

        // Step 3 (terminal): final assistant text.
        adapter.record_text_complete("all done");

        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-bug12").await.expect("read_all");
        assert_eq!(entries.len(), 7, "expected 3 response + 2 tool_use + 2 tool_result entries, got {}", entries.len());

        let turn_ids: std::collections::HashSet<_> = entries
            .iter()
            .map(|e| {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.get("turn_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .expect("each entry must have a turn_id in metadata")
            })
            .collect();

        assert_eq!(
            turn_ids.len(),
            1,
            "all entries in one CLI continuation chain must share one turn_id; got {:?}",
            turn_ids
        );
    }

    /// A dispatched-task run (bg_transcript_override = Some, and the caller
    /// opts into `suppress_visible_snapshot()`) must NOT update the target
    /// agent's snapshot (last_message / last_activity_at / message_count).
    /// Previously the guard was absent, which caused a phantom preview entry
    /// to appear in the agent sidebar for agents that only received a
    /// delegated task — their chat thread was empty but the snapshot showed
    /// the last text the dispatched runner emitted.
    #[tokio::test]
    async fn dispatched_run_does_not_update_agent_snapshot() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, tmp) = make_test_persistence().await;

        // Create a snapshot entry so we can confirm it is NOT mutated.
        persistence
            .snapshots
            .update_agent_entry("agent-dispatch", |entry| {
                entry.name = "Dispatch Target".to_string();
                entry.message_count = 5;
                entry.last_message = Some("original preview".to_string());
                entry.last_message_thread_id = Some("original-thread".to_string());
            })
            .await
            .expect("seed snapshot");

        // bg_transcript_override set to Some(...) AND suppress_visible_snapshot()
        // called — simulates a tasklist-dispatched run. thread_id is Some(...)
        // too, so a bug that forgot the guard would be caught by the assertion
        // below (a real thread id would overwrite "original-thread" instead of
        // leaving it untouched).
        let override_path = tmp.path().join("tasks/t1/transcript.jsonl");
        let adapter = Arc::new(
            TimelineAdapter::new(
                "run-dispatch".to_string(),
                "agent-dispatch".to_string(),
                Some("dispatch-runner-thread".to_string()),
                Arc::clone(&bus),
            )
            .with_persistence(Arc::clone(&persistence), Some(override_path))
            .suppress_visible_snapshot(),
        );

        // Emit some text — exactly the kind of content that used to pollute the snapshot.
        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("task complete!".into()));
        adapter.flush_text();
        adapter.persist_pending().await;

        // The agent snapshot must be unchanged.
        let snap = persistence.snapshots.get().await;
        let entry = snap
            .agents
            .get("agent-dispatch")
            .expect("snapshot entry must exist");
        assert_eq!(entry.message_count, 5, "message_count must not be bumped by a dispatched run");
        assert_eq!(
            entry.last_message.as_deref(),
            Some("original preview"),
            "last_message must not be overwritten by a dispatched run"
        );
        assert_eq!(
            entry.last_message_thread_id.as_deref(),
            Some("original-thread"),
            "last_message_thread_id must not be overwritten by a dispatched run either"
        );
    }

    /// ThinkingBlock session event writes a transcript entry with a literal
    /// `block_json` field (TB-4 format). The stored value must deserialize back
    /// to a `ContentBlock::Thinking` with byte-identical fields.
    #[tokio::test]
    async fn thinking_block_persists_as_literal_block_json() {
        use ao_engine_tools_runner::message::ContentBlock;

        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-tb4-think".to_string(), "agent-tb4".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None)
                .with_model(Some("model-tb4".to_string()))
                .with_key_fingerprint(Some("fp-tb4".to_string())),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::ThinkingBlock {
            text: Some("some reasoning text".to_string()),
            signature: Some("EqRsT7uV8w/Xy+ZaB==".to_string()),
        });
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-tb4").await.expect("read_all");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.event_type, "thinking");
        let meta = e.metadata.as_ref().expect("metadata must be present");

        // Must have block_json, not the old split fields.
        assert!(meta.contains_key("block_json"), "must write block_json field");
        assert!(!meta.contains_key("thinking_text"), "must not write legacy thinking_text field");
        assert!(!meta.contains_key("signature"), "must not write legacy signature field");

        // block_json must deserialize back to the original block byte-identically.
        let block_val = meta["block_json"].clone();
        let block: ContentBlock = serde_json::from_value(block_val).expect("block_json must deserialize");
        match block {
            ContentBlock::Thinking { text, signature } => {
                assert_eq!(text.as_deref(), Some("some reasoning text"));
                assert_eq!(signature.as_deref(), Some("EqRsT7uV8w/Xy+ZaB=="));
            }
            other => panic!("expected Thinking block, got {:?}", other),
        }

        // model and key_fingerprint must still be present for the drop-decision check.
        assert_eq!(meta["model"].as_str(), Some("model-tb4"));
        assert_eq!(meta["key_fingerprint"].as_str(), Some("fp-tb4"));
    }

    /// RedactedThinkingBlock session event writes a `block_json` field whose
    /// data round-trips byte-identically.
    #[tokio::test]
    async fn redacted_thinking_block_persists_as_literal_block_json() {
        use ao_engine_tools_runner::message::ContentBlock;

        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new("run-tb4-redact".to_string(), "agent-tb4-r".to_string(), None, Arc::clone(&bus))
                .with_persistence(Arc::clone(&persistence), None)
                .with_model(Some("model-tb4".to_string()))
                .with_key_fingerprint(Some("fp-tb4".to_string())),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::RedactedThinkingBlock {
            data: "opaque-blob-XyZ==".to_string(),
        });
        adapter.persist_pending().await;

        let entries = persistence.transcripts.read_all("agent-tb4-r").await.expect("read_all");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.event_type, "redacted_thinking");
        let meta = e.metadata.as_ref().expect("metadata must be present");

        assert!(meta.contains_key("block_json"), "must write block_json field");
        assert!(!meta.contains_key("data"), "must not write legacy data field");

        let block: ContentBlock = serde_json::from_value(meta["block_json"].clone())
            .expect("block_json must deserialize");
        match block {
            ContentBlock::RedactedThinking { data } => {
                assert_eq!(data, "opaque-blob-XyZ==");
            }
            other => panic!("expected RedactedThinking block, got {:?}", other),
        }
    }

    /// Sanity-check: a normal (non-dispatched) run with bg_transcript_override = None
    /// DOES update the agent snapshot as expected.
    #[tokio::test]
    async fn direct_run_updates_agent_snapshot() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        // bg_transcript_override = None — normal user-facing chat-thread run.
        let adapter = Arc::new(
            TimelineAdapter::new(
                "run-direct".to_string(),
                "agent-direct".to_string(),
                None,
                Arc::clone(&bus),
            )
            .with_persistence(Arc::clone(&persistence), None),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("hello from agent".into()));
        adapter.flush_text();
        adapter.persist_pending().await;

        let snap = persistence.snapshots.get().await;
        let entry = snap
            .agents
            .get("agent-direct")
            .expect("snapshot entry must be created");
        assert_eq!(entry.message_count, 1, "message_count must be incremented for a direct run");
        assert_eq!(
            entry.last_message.as_deref(),
            Some("hello from agent"),
            "last_message must be set for a direct run"
        );
        assert!(entry.last_activity_at.is_some(), "last_activity_at must be set for a direct run");
        assert_eq!(
            entry.last_message_thread_id, None,
            "adapter was constructed with thread_id=None (default thread), so the snapshot must record None too"
        );
    }

    /// A run scoped to a concrete (non-default) thread must stamp that
    /// thread's id onto `last_message_thread_id` alongside `last_message` —
    /// this is what lets the sidebar's "jump to the thread with the last
    /// message" click land on the right thread instead of always the default.
    #[tokio::test]
    async fn thread_scoped_run_stamps_last_message_thread_id() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, _tmp) = make_test_persistence().await;

        let adapter = Arc::new(
            TimelineAdapter::new(
                "run-threaded".to_string(),
                "agent-threaded".to_string(),
                Some("fresh-thread-42".to_string()),
                Arc::clone(&bus),
            )
            .with_persistence(Arc::clone(&persistence), None),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("hello from a branch".into()));
        adapter.flush_text();
        adapter.persist_pending().await;

        let snap = persistence.snapshots.get().await;
        let entry = snap
            .agents
            .get("agent-threaded")
            .expect("snapshot entry must be created");
        assert_eq!(
            entry.last_message_thread_id.as_deref(),
            Some("fresh-thread-42"),
            "last_message_thread_id must mirror the run's own thread_id"
        );
    }

    /// Regression test for the bug where every reply on a secondary thread
    /// silently stopped updating the sidebar preview. Both `native.rs` and
    /// `cli.rs` route a secondary-thread run's transcript through
    /// `bg_transcript_override` (the thread's own file) — this test mirrors
    /// that real wiring (`thread_id = Some(..)` AND an override path both
    /// set) without calling `suppress_visible_snapshot()`, since an ordinary
    /// thread conversation is not background/dispatched work. Before the fix,
    /// `persist_pending()` inferred suppression from `bg_transcript_override`
    /// alone and silently skipped the snapshot update here too — the two
    /// tests above never combined both fields the way production code does,
    /// which is how this shipped unnoticed.
    #[tokio::test]
    async fn thread_scoped_run_with_override_path_still_updates_snapshot() {
        let bus = Arc::new(EventBus::new(64));
        let (persistence, tmp) = make_test_persistence().await;

        let thread_path = tmp.path().join("threads/fresh-thread-99.jsonl");
        let adapter = Arc::new(
            TimelineAdapter::new(
                "run-thread-override".to_string(),
                "agent-thread-override".to_string(),
                Some("fresh-thread-99".to_string()),
                Arc::clone(&bus),
            )
            .with_persistence(Arc::clone(&persistence), Some(thread_path)),
        );

        SessionEventSink::emit(&*adapter, SessionEvent::AssistantText("reply on a secondary thread".into()));
        adapter.flush_text();
        adapter.persist_pending().await;

        let snap = persistence.snapshots.get().await;
        let entry = snap
            .agents
            .get("agent-thread-override")
            .expect("snapshot entry must be created");
        assert_eq!(
            entry.message_count, 1,
            "message_count must be incremented for an ordinary thread-scoped run"
        );
        assert_eq!(
            entry.last_message.as_deref(),
            Some("reply on a secondary thread"),
            "last_message must reflect the agent's reply, not stay frozen on the user's message"
        );
        assert!(
            entry.last_agent_activity_at.is_some(),
            "last_agent_activity_at must be set so the sidebar unread/typing state stays accurate"
        );
        assert_eq!(
            entry.last_message_thread_id.as_deref(),
            Some("fresh-thread-99"),
            "last_message_thread_id must still mirror the run's own thread_id"
        );
    }

    // ── native_progress_label unit tests ──────────────────────────────────────

    fn make_skill_registry(name: &str) -> SkillRegistry {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        let record = SkillRecord {
            name: name.to_string(),
            description: "test skill".to_string(),
            context: ContextMode::Inline,
            agent: None,
            allowed_tools: vec![],
            arguments: vec![],
            body: String::new(),
            source: SkillSource::User,
            when_to_use: None,
            model: None,
            disable_model_invocation: false,
            provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
            retired: false,
            retired_reason: None,
            superseded_by: None,
            distilled_from: vec![],
            version: 1,
        };
        let mut registry = SkillRegistry::empty();
        registry.insert(name.to_string(), SkillEntry::Ok(record));
        registry
    }

    #[test]
    fn native_progress_label_returns_none_for_non_runskill() {
        let reg = make_skill_registry("verify-studio");
        assert_eq!(
            native_progress_label("Read", &json!({"file_path": "/foo.rs"}), Some(&reg)),
            None
        );
    }

    #[test]
    fn native_progress_label_returns_none_without_registry() {
        let label = native_progress_label("RunSkill", &json!({"skill": "verify-studio"}), None);
        assert_eq!(label, None);
    }

    #[test]
    fn native_progress_label_resolves_exact_name() {
        let reg = make_skill_registry("verify-studio");
        let label = native_progress_label("RunSkill", &json!({"skill": "verify-studio"}), Some(&reg));
        assert_eq!(label, Some("Loading skill: verify-studio".to_string()));
    }

    #[test]
    fn native_progress_label_strips_leading_slash() {
        let reg = make_skill_registry("verify-studio");
        let label = native_progress_label("RunSkill", &json!({"skill": "/verify-studio"}), Some(&reg));
        assert_eq!(label, Some("Loading skill: verify-studio".to_string()));
    }

    #[test]
    fn native_progress_label_returns_none_for_unknown_skill() {
        let reg = make_skill_registry("verify-studio");
        let label = native_progress_label("RunSkill", &json!({"skill": "does-not-exist"}), Some(&reg));
        assert_eq!(label, None);
    }
}
