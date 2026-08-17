pub mod anchor;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_persistence::PersistenceLayer;
use ao_protocol::reflection_trigger::{
    ReflectionTrigger, ReflectionTriggerReason, ReflectionTriggerSubscriber,
};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::context::{compute_message_count, ContextConfig};

pub enum HistorySource {
    Personal { agent_id: String },
    /// A specific thread of an agent.
    ///
    /// Reads from `transcript_path` (the thread's own JSONL file). For branch
    /// threads, `branch_source_path` identifies the source thread transcript and
    /// `history_floor_ts` is the fork timestamp. Entries from the source with
    /// `ts <= floor` are prepended to the thread's own entries, producing the
    /// TRUE FORK view: the agent sees the inherited source history followed by
    /// its own post-branch turns without any tool-call required. Default and
    /// fresh threads carry no source path or floor; they read their own file
    /// only and behave identically to the `Personal` source.
    PersonalThread {
        agent_id: String,
        /// Absolute path to the thread's own JSONL file.
        transcript_path: PathBuf,
        /// For branch threads: the source thread's transcript path.
        /// Source entries with `ts <= history_floor_ts` are prepended to
        /// `transcript_path` entries before windowing. `None` for non-branch.
        branch_source_path: Option<PathBuf>,
        /// For branch threads: the branch-point timestamp (= `Thread::history_floor_ts`).
        /// Entries from the source with `ts > floor` are post-fork writes on the source
        /// and are NOT inherited. `None` for non-branch threads.
        history_floor_ts: Option<DateTime<Utc>>,
    },
    TeamShared { team_id: String },
    TeamPerAgent { team_id: String, agent_id: String },
    TasklistPath { path: PathBuf },
    Project { project_id: String },
}

pub struct HistorySelectInput {
    pub source: HistorySource,
    pub current_message_already_persisted: bool,
    pub now: DateTime<Utc>,
    pub config: ContextConfig,
    /// Optional anchor registry for stable cache-floor pinning.
    /// When `None`, `select` behaves byte-identically to the pre-anchor baseline.
    pub anchor_registry: Option<Arc<anchor::WindowAnchorRegistry>>,
    /// Optional subscriber for the OBSERVE/reflection trigger. When `Some`, `select` notifies it once per call for each
    /// of the two thread-scoped conditions it can detect on its own: the
    /// anchor floor rotating forward (`AnchorRotated`) and the thread having
    /// gone idle past `config.active_window_minutes` (`IdleTimeout`). `None`
    /// disables emission entirely — `select` behaves byte-identically to the
    /// pre-trigger baseline either way.
    ///
    /// Only fires for `HistorySource::Personal` / `PersonalThread`, the two
    /// variants backed by a `Thread` row and its `distilled_through_ts`
    /// watermark; other sources (team/tasklist/project history) are outside
    /// this workstream's scope and never emit.
    pub reflection_subscriber: Option<Arc<dyn ReflectionTriggerSubscriber>>,
}

/// Select prior transcript entries for a given source and dispatch context.
///
/// When `current_message_already_persisted=true`, the last entry is dropped before
/// the time-decay count window is applied (tail-exclusion), so the caller's current
/// message doesn't duplicate itself in the history block.
///
/// Returns `(slice, signal)` where `signal` is:
/// - `None` when no anchor registry was provided, or when the existing anchor produced a cache hit.
/// - `Some(AnchorRotated::Fresh)` when a new anchor was pinned (first call with a registry).
/// - `Some(AnchorRotated::Rotated)` when the window grew past `max_window` and the floor rotated.
///
/// Caller is responsible for slice stability; this function does NOT re-slice.
///
/// Note: RecallHistory deliberately bypasses this function. `select` performs
/// forward windowing automatically on every dispatch; RecallHistory is agent-initiated
/// backward extension reading before the window floor stored in `RunnerContext`.
pub async fn select(
    persistence: &PersistenceLayer,
    input: HistorySelectInput,
) -> (Vec<TranscriptEntry>, Option<anchor::AnchorRotated>) {
    // Derive anchor key before destructuring (which moves `source`).
    let maybe_anchor_key = input
        .anchor_registry
        .as_ref()
        .map(|_| anchor_key_for_source(&input.source));

    let HistorySelectInput {
        source,
        current_message_already_persisted,
        now,
        config,
        anchor_registry,
        reflection_subscriber,
    } = input;

    let entries = match source {
        HistorySource::Personal { ref agent_id } => persistence
            .transcripts
            .read_all(agent_id)
            .await
            .unwrap_or_default(),
        HistorySource::PersonalThread {
            ref transcript_path,
            ref branch_source_path,
            history_floor_ts,
            ..
        } => {
            // Thread's own post-fork entries (starts empty for a new branch; grows with each turn).
            let own = persistence
                .transcripts
                .read_all_at(transcript_path)
                .await
                .unwrap_or_default();

            match (branch_source_path, history_floor_ts) {
                (Some(src_path), Some(floor)) => {
                    // Branch thread with source: graft source history up to the
                    // fork point so the agent reasons over inherited context (TRUE
                    // FORK). Source entries with ts > floor were written after the
                    // branch was cut and are NOT inherited. Own entries always have
                    // ts > floor by construction (new turns start after the fork).
                    let mut combined: Vec<TranscriptEntry> = persistence
                        .transcripts
                        .read_all_at(src_path)
                        .await
                        .unwrap_or_default();
                    combined.retain(|e| e.ts <= floor);
                    combined.extend(own);
                    combined
                }
                (None, Some(floor)) => {
                    // Branch thread without explicit source path: keep only entries
                    // at or after the floor (e.g. the branch transcript was already
                    // pre-seeded, or guard against accidental sub-floor writes).
                    own.into_iter().filter(|e| e.ts >= floor).collect()
                }
                _ => own,
            }
        }
        HistorySource::TeamShared { ref team_id } => {
            let key = format!("team_{}", team_id);
            persistence
                .transcripts
                .read_recent(&key, config.hard_max)
                .await
                .unwrap_or_default()
        }
        HistorySource::TeamPerAgent {
            ref team_id,
            ref agent_id,
        } => {
            let key = format!("team_{}_{}", team_id, agent_id);
            persistence
                .transcripts
                .read_recent(&key, config.hard_max)
                .await
                .unwrap_or_default()
        }
        HistorySource::TasklistPath { ref path } => persistence
            .transcripts
            .read_recent_at(path, config.hard_max)
            .await
            .unwrap_or_default(),
        HistorySource::Project { ref project_id } => {
            let key = format!("project_{}", project_id);
            persistence
                .transcripts
                .read_recent(&key, config.hard_max)
                .await
                .unwrap_or_default()
        }
    };

    // Tail-exclusion: drop the last entry (current user message) before window computation.
    let slice = if current_message_already_persisted && !entries.is_empty() {
        &entries[..entries.len() - 1]
    } else {
        &entries[..]
    };

    if slice.is_empty() {
        return (Vec::new(), None);
    }

    let last_ts = slice.last().map(|e| e.ts);
    let target = compute_message_count(last_ts, now, &config);

    // Idle-timeout detection (reason `IdleTimeout`): the thread's last
    // message is already older than the active window. There is no
    // background timer for this — `select` runs on every dispatch, so an
    // idle-past-window thread is caught retroactively the next time it's
    // dispatched, which is the cheapest signal available without adding a
    // poller (mirrors `anchor.rs`'s own no-background-state philosophy).
    let is_idle_timeout = last_ts
        .map(|ts| now.signed_duration_since(ts).num_minutes() >= config.active_window_minutes)
        .unwrap_or(false);

    // Pair the registry with its key (both Some or both None).
    let registry_with_key = match (anchor_registry, maybe_anchor_key) {
        (Some(reg), Some(key)) => Some((reg, key)),
        _ => None,
    };

    // Determine slice start index. Cache hit reuses the anchor floor; miss recomputes.
    let (mut start, is_cache_hit) = if let Some((ref registry, ref key)) = registry_with_key {
        let existing = registry.get(key);
        let max_window = existing
            .as_ref()
            .map(|a| a.pinned_target)
            .unwrap_or(target)
            * 2
            + config.anchor_grace;

        // CACHE HIT: anchor exists, locatable in this slice, and within max_window.
        let hit_idx = existing
            .as_ref()
            .and_then(|a| locate(slice, &a.floor_marker))
            .filter(|&idx| (slice.len() - idx) <= max_window);

        match hit_idx {
            Some(idx) => (idx, true),
            None => (slice.len().saturating_sub(target), false),
        }
    } else {
        // No registry — backward-compat path, byte-identical to pre-anchor baseline.
        (slice.len().saturating_sub(target), false)
    };

    // Pair-preservation walk runs AFTER anchor lookup.
    // When the window starts on a `tool_result`, walk leftward to its `tool_use`
    // so the Anthropic API pairing constraint is satisfied. Captured post-walk
    // so the marker re-locates cleanly on the next turn (append-only invariant).
    while start > 0 && slice[start].event_type == "tool_result" {
        start -= 1;
    }

    // On cache miss with a registry: pin (Fresh) or rotate (Rotated) the floor.
    let rotation_signal = if !is_cache_hit {
        if let Some((ref registry, ref key)) = registry_with_key {
            let marker = anchor::FloorMarker::for_entry(&slice[start]);
            let new_anchor = anchor::WindowAnchor {
                floor_marker: marker,
                pinned_target: target,
                pinned_at: Utc::now(),
            };
            Some(registry.set(key.clone(), new_anchor))
        } else {
            None
        }
    } else {
        None
    };

    // Reflection-trigger emission. Runs after the anchor
    // bookkeeping above so it observes the final `rotation_signal`. Two of
    // two of the three trigger conditions are detected here — `AnchorRotated` (the
    // sharpest cue: content just fell out of the live window) and
    // `IdleTimeout` — the third, `Archived`, fires from
    // `ThreadStore::archive` instead, since `select` has no reason to run at
    // archive time. Both conditions can fire from the same call; each is
    // independent evidence a reflection pass would want to see.
    if let Some(subscriber) = reflection_subscriber.as_deref() {
        if rotation_signal == Some(anchor::AnchorRotated::Rotated) {
            emit_reflection_trigger(
                persistence,
                &source,
                subscriber,
                ReflectionTriggerReason::AnchorRotated,
                now,
            );
        }
        if is_idle_timeout {
            emit_reflection_trigger(
                persistence,
                &source,
                subscriber,
                ReflectionTriggerReason::IdleTimeout,
                now,
            );
        }
    }

    (slice[start..].to_vec(), rotation_signal)
}

/// Build and dispatch a [`ReflectionTrigger`] for `source`, if `source` is
/// backed by a `Thread` row. Only `Personal` and `PersonalThread` are —
/// they're the sources whose watermark (`Thread::distilled_through_ts`)
/// this trigger exists to eventually advance. Team/tasklist/project history
/// isn't part of the reflection trigger's scope, so those sources are silently
/// skipped rather than emitted with a fabricated identity.
fn emit_reflection_trigger(
    persistence: &PersistenceLayer,
    source: &HistorySource,
    subscriber: &dyn ReflectionTriggerSubscriber,
    reason: ReflectionTriggerReason,
    ts: DateTime<Utc>,
) {
    let (agent_id, transcript_path) = match source {
        HistorySource::Personal { agent_id } => (
            agent_id.clone(),
            persistence
                .data_root
                .agent_transcript_path(agent_id)
                .to_string_lossy()
                .into_owned(),
        ),
        HistorySource::PersonalThread {
            agent_id,
            transcript_path,
            ..
        } => (
            agent_id.clone(),
            transcript_path.to_string_lossy().into_owned(),
        ),
        _ => return,
    };
    subscriber.on_reflection_trigger(ReflectionTrigger {
        reason,
        agent_id,
        transcript_path,
        ts,
    });
}

/// Derive the `AnchorKey` scope for a given `HistorySource` (one key per scope).
fn anchor_key_for_source(source: &HistorySource) -> anchor::AnchorKey {
    match source {
        HistorySource::Personal { agent_id } => anchor::AnchorKey::Personal(agent_id.clone()),
        HistorySource::PersonalThread {
            agent_id,
            transcript_path,
            ..
        } => anchor::AnchorKey::AgentThread(agent_id.clone(), transcript_path.clone()),
        HistorySource::TeamShared { team_id } => anchor::AnchorKey::TeamShared(team_id.clone()),
        HistorySource::TeamPerAgent { team_id, agent_id } => {
            anchor::AnchorKey::TeamPerAgent(team_id.clone(), agent_id.clone())
        }
        HistorySource::TasklistPath { path } => anchor::AnchorKey::TasklistPath(path.clone()),
        HistorySource::Project { project_id } => anchor::AnchorKey::Project(project_id.clone()),
    }
}

/// Scan `slice` for the first entry matching `marker`; return its index.
fn locate(slice: &[TranscriptEntry], marker: &anchor::FloorMarker) -> Option<usize> {
    slice
        .iter()
        .position(|e| anchor::FloorMarker::for_entry(e) == *marker)
}

/// Translate persisted transcript entries into a `Vec<Message>` for the API messages array.
///
/// Translation table:
/// - `event_type="message"`, `role=System(_)` or `Schedule{..}` → `Message::User`
/// - `event_type="response"`, `role=Agent` → `Message::Assistant` with `Text` block
/// - `event_type="tool_use"`, `role=Agent` → `Message::Assistant` with `ToolUse` block
/// - `event_type="tool_result"`, `role=System("tool")` → `Message::ToolResult`
/// - Other `event_type` values are dropped with `tracing::warn!`
///
/// Consecutive `Assistant` entries sharing the same `turn_id` metadata key are merged
/// into a single `Message::Assistant`. Entries with missing or mismatched `turn_id`
/// each produce their own message (Anthropic accepts consecutive same-role messages).
///
/// # Model-bound reasoning blocks
///
/// `current_model` is the model that will consume the reconstructed transcript
/// (the resuming agent's configured model). Anthropic's `thinking` /
/// `redacted_thinking` signatures are bound to the model that produced them, so
/// replaying a block authored by a different model is a hard 400. A reconstructed
/// reasoning block is therefore kept **only when its persisted `model` tag is
/// present and equals `current_model`**; in every other case — a different model,
/// an untagged (legacy) block whose author we cannot verify, or an unknown
/// `current_model` — the block is dropped. The reasoning *text* still lives in the
/// transcript for the UI; only the API replay is suppressed. Dropping is always
/// API-safe: Anthropic accepts a tool continuation that omits the prior reasoning
/// block, it only rejects one that replays a stale signature.
///
/// This only affects the cross-run resume path. The live in-session loop replays
/// its own in-memory blocks (same model, byte-identical) and never routes through
/// here, so active-cycle continuity within a single run is unaffected.
///
/// `current_key_fingerprint` is the non-secret hash of the API key active in the
/// resuming run (see `ProviderClient::key_fingerprint`). A block persisted under
/// a different key fingerprint is dropped with the same logic as a model mismatch —
/// Anthropic signatures are bound to both the model and the key that authored them.
pub fn to_messages(
    entries: &[TranscriptEntry],
    current_model: Option<&str>,
    current_key_fingerprint: Option<&str>,
) -> Vec<Message> {
    // Pre-pass: identify ordered `tool_use` ↔ `tool_result` pairs that are both
    // present in this slice. A pair is valid only when the `tool_use` precedes
    // its `tool_result` — that's exactly the Anthropic API constraint, and the
    // intersection it forms gives us a single set we can use to drop orphans
    // in either direction:
    //
    //   - Left-edge: a `tool_result` whose `tool_use` was sliced away upstream.
    //     The slicer expansion in `select` widens the window leftward over
    //     leading tool_results, but if pairing data is corrupted (mismatched
    //     metadata, missing turn_id, etc.) the expansion may stop short. This
    //     pre-pass is the belt.
    //
    //   - Right-edge: a `tool_use` whose `tool_result` was never persisted
    //     (runner crashed mid-iteration, cancellation token tripped between
    //     dispatch and completion). Without this filter, the API errors with
    //     "missing tool_result for tool_use" on the next dispatch.
    //
    // Both directions matter for Anthropic's strict pairing requirement; the
    // pre-pass cost is O(n) and the filter is a single set lookup per block.
    let valid_pairs: HashSet<String> = {
        let mut seen_uses: HashSet<String> = HashSet::new();
        let mut pairs: HashSet<String> = HashSet::new();
        for entry in entries {
            match entry.event_type.as_str() {
                "tool_use" => {
                    if let Some(id) = extract_tool_use_id_meta(entry) {
                        seen_uses.insert(id);
                    }
                }
                "tool_result" => {
                    if let Some(id) = extract_tool_use_id_meta(entry) {
                        if seen_uses.contains(&id) {
                            pairs.insert(id);
                        }
                    }
                }
                _ => {}
            }
        }
        pairs
    };

    let mut result: Vec<Message> = Vec::with_capacity(entries.len());
    // (turn_id, content_blocks) for the assistant message currently being built.
    let mut pending: Option<(Option<String>, Vec<ContentBlock>)> = None;

    for entry in entries {
        match entry.event_type.as_str() {
            "message" => {
                if let Some((_, blocks)) = pending.take() {
                    if !blocks.is_empty() {
                        result.push(Message::Assistant { content: blocks });
                    }
                }
                match &entry.role {
                    TranscriptRole::System(_) | TranscriptRole::Schedule { .. } => {
                        result.push(Message::User {
                            content: vec![ContentBlock::Text { text: entry.content.clone() }],
                        });
                    }
                    _ => {
                        tracing::warn!(
                            "history::to_messages: unexpected role for message entry, skipping"
                        );
                    }
                }
            }
            "response" => {
                let turn_id = extract_turn_id(entry);
                let block = ContentBlock::Text { text: entry.content.clone() };
                coalesce_or_flush_assistant(&mut result, &mut pending, block, turn_id);
            }
            "tool_use" => {
                let meta = entry.metadata.as_ref();
                let id = meta
                    .and_then(|m| m.get("tool_use_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() || !valid_pairs.contains(&id) {
                    tracing::warn!(
                        tool_use_id = %id,
                        "history::to_messages: dropping tool_use with no matching tool_result in slice"
                    );
                    continue;
                }
                let name = meta
                    .and_then(|m| m.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let input = meta
                    .and_then(|m| m.get("input"))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));
                let turn_id = extract_turn_id(entry);
                let block = ContentBlock::ToolUse { id, name, input };
                coalesce_or_flush_assistant(&mut result, &mut pending, block, turn_id);
            }
            "tool_result" => {
                let meta = entry.metadata.as_ref();
                let tool_use_id = meta
                    .and_then(|m| m.get("tool_use_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if tool_use_id.is_empty() || !valid_pairs.contains(&tool_use_id) {
                    tracing::warn!(
                        tool_use_id = %tool_use_id,
                        "history::to_messages: dropping orphan tool_result with no preceding tool_use in slice"
                    );
                    continue;
                }
                if let Some((_, blocks)) = pending.take() {
                    if !blocks.is_empty() {
                        result.push(Message::Assistant { content: blocks });
                    }
                }
                let output = meta
                    .and_then(|m| m.get("output"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let is_error = meta
                    .and_then(|m| m.get("is_error"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                result.push(Message::ToolResult {
                    tool_use_id,
                    content: vec![ContentBlock::Text { text: output }],
                    is_error,
                });
            }
            "thinking" => {
                // Reconstruct a signed reasoning block. Thinking entries arrive in
                // the transcript BEFORE the response/tool_use entries from the same
                // turn, so `coalesce_or_flush_assistant` puts them first in the
                // content array — exactly what Anthropic requires.
                let meta = entry.metadata.as_ref();
                // Drop the block unless we can prove it was authored by the same
                // model AND the same API key that will replay it.
                if !reasoning_block_replayable(meta, current_model, current_key_fingerprint) {
                    tracing::debug!(
                        authoring_model = ?meta.and_then(|m| m.get("model")).and_then(|v| v.as_str()),
                        authoring_fp = ?meta.and_then(|m| m.get("key_fingerprint")).and_then(|v| v.as_str()),
                        current_model = ?current_model,
                        "history::to_messages: dropping thinking block (model or key-fingerprint mismatch)"
                    );
                    continue;
                }
                let turn_id = extract_turn_id(entry);
                // TB-4: try the literal-block format first; fall back to the legacy
                // split-field format for entries written before this change.
                let block = if let Some(block_val) = meta.and_then(|m| m.get("block_json")).cloned() {
                    match serde_json::from_value::<ContentBlock>(block_val) {
                        Ok(b @ ContentBlock::Thinking { .. }) => b,
                        Ok(_) => {
                            tracing::warn!("history::to_messages: thinking block_json is wrong type, skipping");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "history::to_messages: thinking block_json failed to deserialize, skipping");
                            continue;
                        }
                    }
                } else {
                    // Legacy split-field format (pre-TB-4).
                    let text = meta
                        .and_then(|m| m.get("thinking_text"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    let signature = meta
                        .and_then(|m| m.get("signature"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    ContentBlock::Thinking { text, signature }
                };
                coalesce_or_flush_assistant(&mut result, &mut pending, block, turn_id);
            }
            "redacted_thinking" => {
                // Reconstruct an opaque redacted reasoning block. The payload is
                // model-bound and must be echoed verbatim; any corruption triggers
                // the same 400 "cannot be modified" error.
                let meta = entry.metadata.as_ref();
                if !reasoning_block_replayable(meta, current_model, current_key_fingerprint) {
                    tracing::debug!(
                        authoring_model = ?meta.and_then(|m| m.get("model")).and_then(|v| v.as_str()),
                        authoring_fp = ?meta.and_then(|m| m.get("key_fingerprint")).and_then(|v| v.as_str()),
                        current_model = ?current_model,
                        "history::to_messages: dropping redacted_thinking block (model or key-fingerprint mismatch)"
                    );
                    continue;
                }
                let turn_id = extract_turn_id(entry);
                // TB-4: try the literal-block format first; fall back to the legacy
                // `data` field for entries written before this change.
                let block = if let Some(block_val) = meta.and_then(|m| m.get("block_json")).cloned() {
                    match serde_json::from_value::<ContentBlock>(block_val) {
                        Ok(b @ ContentBlock::RedactedThinking { .. }) => b,
                        Ok(_) => {
                            tracing::warn!("history::to_messages: redacted_thinking block_json is wrong type, skipping");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "history::to_messages: redacted_thinking block_json failed to deserialize, skipping");
                            continue;
                        }
                    }
                } else {
                    // Legacy split-field format (pre-TB-4).
                    let data = meta
                        .and_then(|m| m.get("data"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if data.is_empty() {
                        tracing::warn!("history::to_messages: redacted_thinking entry has empty data field, skipping");
                        continue;
                    }
                    ContentBlock::RedactedThinking { data }
                };
                coalesce_or_flush_assistant(&mut result, &mut pending, block, turn_id);
            }
            other => {
                tracing::warn!(
                    event_type = %other,
                    "history::to_messages: unknown event_type, skipping"
                );
            }
        }
    }

    if let Some((_, blocks)) = pending.take() {
        if !blocks.is_empty() {
            result.push(Message::Assistant { content: blocks });
        }
    }

    // Drop reasoning blocks from assistant turns that are no longer part of an
    // active tool-use cycle. Replaying a reconstructed `thinking`/`redacted_thinking`
    // block on a closed turn (one followed by a user message rather than a
    // tool_result) is both unnecessary and risky: Anthropic validates the
    // signatures of the reasoning blocks in the latest assistant message and
    // rejects the request when a rebuilt copy isn't byte-identical to the
    // original. Blocks on the active cycle (assistant tool_use → tool_result)
    // are preserved.
    ao_engine_tools_runner::message::strip_closed_turn_reasoning(&mut result);

    result
}

/// Decide whether a persisted reasoning block may be replayed in the current run.
///
/// Anthropic's `thinking`/`redacted_thinking` signatures are bound to BOTH the
/// model that produced them AND the API key that was active at generation time.
/// A block is replayable only when BOTH its `model` tag and its `key_fingerprint`
/// tag match the current run's model and key fingerprint. Anything that cannot be
/// positively verified — a different model, a different key, an untagged legacy
/// block, or an unknown current model/key — is treated as non-replayable so the
/// caller drops it rather than risking a 400.
fn reasoning_block_replayable(
    meta: Option<&std::collections::HashMap<String, Value>>,
    current_model: Option<&str>,
    current_key_fingerprint: Option<&str>,
) -> bool {
    let authoring_model = meta.and_then(|m| m.get("model")).and_then(|v| v.as_str());
    let authoring_fp = meta.and_then(|m| m.get("key_fingerprint")).and_then(|v| v.as_str());
    match (authoring_model, current_model) {
        (Some(authored), Some(current)) if authored == current => {
            // Model matches — now verify the key fingerprint too.
            match (authoring_fp, current_key_fingerprint) {
                (Some(authored_fp), Some(current_fp)) => authored_fp == current_fp,
                _ => false,
            }
        }
        _ => false,
    }
}

fn extract_tool_use_id_meta(entry: &TranscriptEntry) -> Option<String> {
    entry
        .metadata
        .as_ref()
        .and_then(|m| m.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_turn_id(entry: &TranscriptEntry) -> Option<String> {
    entry
        .metadata
        .as_ref()
        .and_then(|m| m.get("turn_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn coalesce_or_flush_assistant(
    result: &mut Vec<Message>,
    pending: &mut Option<(Option<String>, Vec<ContentBlock>)>,
    block: ContentBlock,
    turn_id: Option<String>,
) {
    // Coalesce only when both sides have the same non-None turn_id.
    let should_coalesce = match (pending.as_ref(), turn_id.as_deref()) {
        (Some((Some(tid), _)), Some(new_tid)) if tid == new_tid => true,
        _ => false,
    };
    if should_coalesce {
        if let Some((_, blocks)) = pending.as_mut() {
            blocks.push(block);
        }
    } else {
        if let Some((_, blocks)) = pending.take() {
            if !blocks.is_empty() {
                result.push(Message::Assistant { content: blocks });
            }
        }
        *pending = Some((turn_id, vec![block]));
    }
}

#[cfg(test)]
mod tests;
