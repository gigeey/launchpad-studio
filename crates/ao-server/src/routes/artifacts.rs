use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use ao_engine::artifact_regen::{spawn_artifact_agent, ArtifactAgentMode};
use ao_engine::artifact_task_status::ArtifactTaskState;
use ao_engine::AppState;
use ao_persistence::artifact_store::NewArtifact;
use ao_protocol::artifact::{
    ArtifactGroup, ArtifactKind, ArtifactRecord, CapabilitySpec, IntentLedgerEntry, IntentSource, OriginIntent,
    PayloadFormat, RefreshIntent,
};
use ao_protocol::error::AoError;
use ao_protocol::transcript::{PaginatedResponse, PaginationCursor, TranscriptEntry, TranscriptRole};

use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct CreateArtifactRequest {
    pub title: String,
    pub kind: ArtifactKind,
    pub format: PayloadFormat,
    /// A JSON object for typed kinds, or a JSON string for `format: "html"`.
    pub payload: serde_json::Value,
    #[serde(default)]
    pub refresh_intent: RefreshIntent,
    #[serde(default)]
    pub origin_intent: Option<OriginIntent>,
    #[serde(default)]
    pub capabilities: Vec<CapabilitySpec>,
    #[serde(default)]
    pub source_message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RefreshArtifactRequest {
    pub payload: serde_json::Value,
}

/// Response body for `POST /agents/{agent_id}/artifacts/{artifact_id}/regenerate`.
#[derive(Debug, Serialize)]
pub struct RegenerateArtifactResponse {
    /// Id of the background subagent run that was kicked off. There is no
    /// dedicated status/completion HTTP surface for it today — poll
    /// `GET .../artifacts/{artifact_id}` and watch `updated_at` (or
    /// `checksum_sha256`) for the in-place `ArtifactWrite` to land.
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SetPinnedRequest {
    pub pinned: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetArtifactGroupRequest {
    /// `None` clears the artifact's group (moves it back to the ungrouped
    /// list); `Some(id)` must reference an existing `ArtifactGroup`, though
    /// this endpoint doesn't validate that — an id for a since-deleted group
    /// just reads as ungrouped everywhere the frontend resolves group names.
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateArtifactGroupRequest {
    pub name: String,
}

/// A pinned artifact's record alongside the id of the agent that owns it —
/// the cross-agent pinned listing has no other way to say which agent an
/// entry came from, since [`ArtifactRecord`] itself is agent-agnostic.
#[derive(Debug, Serialize)]
pub struct PinnedArtifact {
    pub agent_id: String,
    #[serde(flatten)]
    pub record: ArtifactRecord,
}

/// An artifact's metadata alongside its current payload, for a single-fetch
/// render (the record alone isn't enough to draw anything).
#[derive(Debug, Serialize)]
pub struct ArtifactWithPayload {
    #[serde(flatten)]
    pub record: ArtifactRecord,
    pub payload: serde_json::Value,
    /// Whether [`undo_artifact`] has a snapshot to restore right now —
    /// derived from `record.history` rather than stored, so it's always
    /// exactly in sync with what an undo call would actually do. Read on
    /// load so the frontend can render the undo button's enabled state
    /// without an extra round trip.
    pub undo_available: bool,
    /// The id of a background regenerate/chat run currently in flight for this
    /// artifact, or `None` when nothing is running (or the latest run already
    /// reached a terminal state). Runtime-derived from the in-memory task
    /// status store on every read, never persisted on the artifact record — it
    /// lets the frontend resume a progress spinner after the artifact view has
    /// been unmounted and remounted (e.g. the user navigated away and back),
    /// which would otherwise lose that purely client-side state.
    pub running_task_id: Option<String>,
}

/// Encode a request payload to on-disk blob bytes per the artifact's format:
/// HTML artifacts carry a JSON string (the markup); typed artifacts carry a
/// JSON object/array, stored as its serialized bytes.
fn payload_to_bytes(format: PayloadFormat, payload: &serde_json::Value) -> Result<Vec<u8>, AoError> {
    match format {
        PayloadFormat::Html => {
            let html = payload.as_str().ok_or_else(|| {
                AoError::ValidationError("html artifacts require a string payload".to_string())
            })?;
            Ok(html.as_bytes().to_vec())
        }
        PayloadFormat::Json => serde_json::to_vec(payload).map_err(|e| AoError::Json(e.to_string())),
    }
}

/// Inverse of [`payload_to_bytes`] — decode stored blob bytes back to a JSON
/// value for the response envelope.
fn bytes_to_payload(format: PayloadFormat, bytes: &[u8]) -> Result<serde_json::Value, AoError> {
    match format {
        PayloadFormat::Html => Ok(serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())),
        PayloadFormat::Json => serde_json::from_slice(bytes).map_err(|e| AoError::Json(e.to_string())),
    }
}

/// POST /agents/{agent_id}/artifacts
pub async fn create_artifact(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateArtifactRequest>,
) -> Result<Json<ArtifactRecord>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let payload = payload_to_bytes(req.format, &req.payload)?;

    let record = state
        .persistence
        .artifacts
        .create(
            &agent_id,
            NewArtifact {
                title: req.title,
                kind: req.kind,
                format: req.format,
                payload,
                refresh_intent: req.refresh_intent,
                origin_intent: req.origin_intent,
                capabilities: req.capabilities,
                source_message_id: req.source_message_id,
                intent_note: None,
            },
        )
        .await?;

    Ok(Json(record))
}

/// GET /agents/{agent_id}/artifacts — lists all artifact records (metadata
/// only, no payload bytes).
pub async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<ArtifactRecord>>, AppError> {
    let artifacts = state.persistence.artifacts.list_by_agent(&agent_id).await?;
    Ok(Json(artifacts))
}

/// GET /agents/{agent_id}/artifacts/{artifact_id} — record + current payload.
pub async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
) -> Result<Json<ArtifactWithPayload>, AppError> {
    let (record, bytes) = state
        .persistence
        .artifacts
        .get_payload(&agent_id, &artifact_id)
        .await?;
    let payload = bytes_to_payload(record.format, &bytes)?;
    let undo_available = !record.history.is_empty();
    let running_task_id = state
        .artifact_task_status
        .running_task_id_for_artifact(&artifact_id);
    Ok(Json(ArtifactWithPayload {
        record,
        payload,
        undo_available,
        running_task_id,
    }))
}

/// PUT /agents/{agent_id}/artifacts/{artifact_id}/refresh — replaces the
/// payload in place and bumps `updated_at`/`last_refreshed_at`/`refresh_count`.
/// Replaying `origin_intent` to regenerate the payload is out of scope here —
/// the caller supplies the new payload directly. Tagged `IntentSource::MainThreadEdit`
/// in the artifact's intent ledger since this route sits outside the model/chat
/// loop entirely (no message id or model-authored note to carry).
pub async fn refresh_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    Json(req): Json<RefreshArtifactRequest>,
) -> Result<Json<ArtifactRecord>, AppError> {
    let existing = state.persistence.artifacts.get(&agent_id, &artifact_id).await?;
    let payload = payload_to_bytes(existing.format, &req.payload)?;

    let record = state
        .persistence
        .artifacts
        .refresh(&agent_id, &artifact_id, &payload, IntentSource::MainThreadEdit, None, None)
        .await?;

    Ok(Json(record))
}

/// Response body for `POST .../artifacts/{artifact_id}/undo` — the same
/// record shape [`refresh_artifact`]/[`get_artifact`] return, plus a fresh
/// `undo_available` so the caller doesn't have to separately inspect
/// `history.len()` to know whether the undo button should stay enabled.
#[derive(Debug, Serialize)]
pub struct UndoArtifactResponse {
    #[serde(flatten)]
    pub record: ArtifactRecord,
    /// `true` iff `history` is still non-empty AFTER this undo — i.e.
    /// whether calling this endpoint again would revert yet another edit.
    pub undo_available: bool,
}

/// POST /agents/{agent_id}/artifacts/{artifact_id}/undo — pops the most
/// recent snapshot off the artifact's bounded undo history
/// ([`ArtifactRecord::history`], depth-capped, oldest evicted first) and
/// restores it as the current body, synchronously — no background subagent,
/// no polling, unlike [`regenerate_artifact`]/[`chat_artifact`]. Covers every
/// edit-in-place surface uniformly (whole-artifact regenerate, chat-adjust,
/// and this route's own sibling `PUT .../refresh`) because all of them
/// funnel body-replacing writes through the same `ArtifactStore::refresh`
/// choke point, which is what pushes each snapshot in the first place.
///
/// No request body — there is nothing to specify; it always undoes exactly
/// one step. 409s with a clear message when `history` is already empty
/// (nothing left to undo).
pub async fn undo_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
) -> Result<Json<UndoArtifactResponse>, AppError> {
    let record = state.persistence.artifacts.undo(&agent_id, &artifact_id, None).await?;
    let undo_available = !record.history.is_empty();
    Ok(Json(UndoArtifactResponse { record, undo_available }))
}

/// POST /agents/{agent_id}/artifacts/{artifact_id}/regenerate — replays
/// `origin_intent.refresh_prompt` through a fresh background subagent run
/// (as the owning agent, with its full tool registry — so a websearch-backed
/// artifact reruns the search) that overwrites the artifact in place via
/// `ArtifactWrite(id=artifact_id)`.
///
/// 409s if the artifact isn't configured for whole-artifact regeneration
/// (`refresh_intent != WholeArtifact`) or carries no replayable
/// `origin_intent.refresh_prompt`. Otherwise returns 202 immediately with the
/// spawned subagent's id — this does NOT block until the subagent finishes;
/// see [`spawn_artifact_agent`] for why, and for how completion is observed.
pub async fn regenerate_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<RegenerateArtifactResponse>), AppError> {
    let record = state.persistence.artifacts.get(&agent_id, &artifact_id).await?;

    if record.refresh_intent != RefreshIntent::WholeArtifact {
        return Err(AppError(AoError::Conflict(format!(
            "artifact '{artifact_id}' is not whole-artifact-refreshable (refresh_intent is {:?}, expected whole_artifact)",
            record.refresh_intent
        ))));
    }

    let refresh_prompt = match record.origin_intent {
        Some(OriginIntent { refresh_prompt }) if !refresh_prompt.trim().is_empty() => refresh_prompt,
        _ => {
            return Err(AppError(AoError::Conflict(format!(
                "artifact '{artifact_id}' has no origin_intent.refresh_prompt to replay"
            ))));
        }
    };

    let task_id = spawn_artifact_agent(
        &state,
        &agent_id,
        &artifact_id,
        refresh_prompt,
        ArtifactAgentMode::Regenerate,
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(RegenerateArtifactResponse {
            task_id: task_id.to_string(),
        }),
    ))
}

/// One turn of the frontend's per-artifact chat mini-thread (keyed
/// `artifact:{artifactId}` client-side), replayed as context ahead of the
/// new message so the subagent can follow a back-and-forth instead of only
/// ever seeing the latest message in isolation.
#[derive(Debug, Deserialize)]
pub struct ChatTranscriptTurn {
    pub role: ChatTurnRole,
    pub content: String,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatTurnRole {
    User,
    Assistant,
}

/// Request body for `POST .../artifacts/{artifact_id}/chat`.
#[derive(Debug, Deserialize)]
pub struct ChatArtifactRequest {
    /// The user's new chat message — becomes (part of) the seed instruction
    /// for the chat-adjust subagent run.
    pub message: String,
    /// Prior turns of this artifact's mini-thread, oldest first, NOT
    /// including `message` itself. Optional — an empty/omitted transcript
    /// just means this is the first message about this artifact. Only the
    /// most recent [`CHAT_SEED_TRANSCRIPT_MAX_TURNS`] are folded into the
    /// seed prompt.
    #[serde(default)]
    pub transcript: Vec<ChatTranscriptTurn>,
}

/// Response body for `POST .../artifacts/{artifact_id}/chat`.
#[derive(Debug, Serialize)]
pub struct ChatArtifactResponse {
    /// Id of the spawned background subagent run. Identical polling contract
    /// to [`RegenerateArtifactResponse::task_id`] — this route is the second
    /// caller of the same fire-and-forget [`spawn_artifact_agent`] seam, so
    /// it has no separate completion surface either: poll
    /// `GET .../artifacts/{artifact_id}` (the existing `useArtifactRegen`
    /// hook already does this) and watch `updated_at`/`checksum_sha256`.
    ///
    /// There is deliberately no synchronous "final reply" field on this
    /// response — the subagent run is not awaited here. Once the poll
    /// observes the artifact change, read that same
    /// `GET .../artifacts/{artifact_id}` response's `intent_ledger` — its
    /// *last* entry's `intent_note` is the subagent's reply, and should be
    /// shown in the chat panel as the assistant's bubble: the seed prompt
    /// built by [`build_chat_seed_prompt`] explicitly instructs the subagent
    /// to phrase `intent_note` as a short first-person confirmation of what
    /// it changed, exactly so it doubles as a chat reply with no extra
    /// plumbing. See that function's doc comment for the reasoning, and this
    /// module's task report for the follow-up if a truly live/streamed reply
    /// is wanted later.
    pub task_id: String,
}

/// Upper bound on how many of the artifact's most recent intent-ledger
/// entries are folded into a chat-adjust seed prompt as "recent edit
/// history" — enough for the subagent to avoid redoing or undoing a change
/// from a couple of turns ago, without ballooning the prompt on a
/// long-lived artifact (the ledger itself already caps at
/// [`ao_protocol::artifact::INTENT_LEDGER_MAX_LEN`]).
const CHAT_SEED_LEDGER_MAX_ENTRIES: usize = 5;

/// Upper bound on how many prior turns of the frontend's per-artifact mini-thread
/// are folded into a chat-adjust seed prompt.
const CHAT_SEED_TRANSCRIPT_MAX_TURNS: usize = 10;

/// Assemble the seed instruction for a chat-adjust subagent run from the
/// user's new message plus recent context. [`spawn_artifact_agent`] appends
/// the artifact's CURRENT body on top of whatever this returns — current
/// body plus this function's ledger/transcript context is the whole point of
/// carrying the intent ledger: the subagent edits from what the artifact
/// actually looks like *now*, not from a stale snapshot the user's message
/// alone would imply.
///
/// Pure/deterministic on purpose, so it's unit-testable with no
/// server/spawner infrastructure — see the tests module below.
fn build_chat_seed_prompt(message: &str, ledger: &[IntentLedgerEntry], transcript: &[ChatTranscriptTurn]) -> String {
    let mut seed = message.trim().to_string();

    if !ledger.is_empty() {
        let start = ledger.len().saturating_sub(CHAT_SEED_LEDGER_MAX_ENTRIES);
        seed.push_str("\n\n## Recent edit history for this artifact (oldest first)\n");
        for entry in &ledger[start..] {
            let note = entry.intent_note.as_deref().unwrap_or("(no note)");
            seed.push_str(&format!(
                "- [{}] {:?}: {}\n",
                entry.timestamp.to_rfc3339(),
                entry.source,
                note
            ));
        }
    }

    if !transcript.is_empty() {
        let start = transcript.len().saturating_sub(CHAT_SEED_TRANSCRIPT_MAX_TURNS);
        seed.push_str("\n\n## Recent conversation about this artifact (oldest first)\n");
        for turn in &transcript[start..] {
            let role = match turn.role {
                ChatTurnRole::User => "User",
                ChatTurnRole::Assistant => "Assistant",
            };
            seed.push_str(&format!("{role}: {}\n", turn.content.trim()));
        }
    }

    seed.push_str(
        "\n\nAfter writing, set ArtifactWrite's intent_note to a short (under 200 characters) \
         first-person confirmation of what you changed (e.g. \"Changed the header color to \
         blue.\") — the host displays it to the user as your reply in the chat panel, so phrase \
         it like a reply, not a commit message.",
    );

    seed
}

/// POST /agents/{agent_id}/artifacts/{artifact_id}/chat — the chat-to-adjust
/// counterpart to [`regenerate_artifact`]: instead of replaying
/// `origin_intent.refresh_prompt` to redo the whole artifact from scratch,
/// this applies one targeted adjustment described in a user chat message.
/// Works on any artifact regardless of `refresh_intent` (unlike regenerate,
/// a chat edit doesn't require a replayable origin prompt) — either way the
/// subagent edits from the artifact's CURRENT body via
/// `ArtifactWrite(id=artifact_id)`.
///
/// Same fire-and-forget contract as `regenerate_artifact`: 202 immediately
/// with the spawned subagent's id, no blocking on completion. See
/// [`ChatArtifactResponse`] for how the frontend should surface the
/// subagent's reply once its poll observes the artifact change.
pub async fn chat_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    Json(req): Json<ChatArtifactRequest>,
) -> Result<(StatusCode, Json<ChatArtifactResponse>), AppError> {
    let message = req.message.trim();
    if message.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "message must not be empty".to_string(),
        )));
    }

    let record = state.persistence.artifacts.get(&agent_id, &artifact_id).await?;

    let seed_prompt = build_chat_seed_prompt(message, &record.intent_ledger, &req.transcript);

    // Persist the user's message to the artifact's durable chat transcript
    // BEFORE spawning, so it survives even if the subagent run below fails —
    // the caller sent it, so it happened, regardless of what comes next.
    let user_entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: message.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    let thread = state
        .persistence
        .threads
        .ensure_artifact_thread(&artifact_id)
        .await?;
    state
        .persistence
        .transcripts
        .append_at(&PathBuf::from(&thread.transcript_path), &user_entry)
        .await?;

    let task_id = spawn_artifact_agent(
        &state,
        &agent_id,
        &artifact_id,
        seed_prompt,
        ArtifactAgentMode::ChatAdjust,
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(ChatArtifactResponse {
            task_id: task_id.to_string(),
        }),
    ))
}

/// Response body for `GET .../artifacts/{artifact_id}/task/{task_id}/status`.
#[derive(Debug, Serialize)]
pub struct ArtifactTaskStatusResponse {
    /// One of `"running"`, `"completed"`, `"failed"`, `"unknown"`.
    /// `"unknown"` covers both a bogus/expired `task_id` and a `task_id`
    /// that hasn't reached `mark_running` yet (e.g. queried before the
    /// spawn call returns) — the client's job is only to tell "slow" from
    /// "dead", and a fresh unknown is transient either way.
    pub status: &'static str,
    pub error: Option<String>,
}

/// GET /agents/{agent_id}/artifacts/{artifact_id}/task/{task_id}/status —
/// queries the in-memory status of a background subagent run kicked off by
/// [`regenerate_artifact`] or [`chat_artifact`]. Never 500s on an unknown
/// `task_id`; `agent_id`/`artifact_id` are accepted for URL symmetry with the
/// other artifact routes but are not themselves validated against the store,
/// since the status store is keyed on `task_id` alone.
pub async fn get_artifact_task_status(
    State(state): State<Arc<AppState>>,
    Path((_agent_id, _artifact_id, task_id)): Path<(String, String, String)>,
) -> Json<ArtifactTaskStatusResponse> {
    let (status, error) = match state.artifact_task_status.get(&task_id) {
        None => ("unknown", None),
        Some(s) => match s.state {
            ArtifactTaskState::Running => ("running", None),
            ArtifactTaskState::Completed => ("completed", None),
            ArtifactTaskState::Failed => ("failed", s.error),
        },
    };
    Json(ArtifactTaskStatusResponse { status, error })
}

#[derive(Debug, Deserialize)]
pub struct GetArtifactChatQuery {
    /// Max number of most-recent transcript entries to return. Defaults to
    /// [`ARTIFACT_CHAT_DEFAULT_LAST`] when omitted, mirroring `get_messages`.
    pub last: Option<usize>,
}

const ARTIFACT_CHAT_DEFAULT_LAST: usize = 50;

/// Response body for `GET .../artifacts/{artifact_id}/chat`.
#[derive(Debug, Serialize)]
pub struct ArtifactChatResponse {
    pub entries: Vec<TranscriptEntry>,
    pub cursor: Option<PaginationCursor>,
}

/// GET /agents/{agent_id}/artifacts/{artifact_id}/chat — durable chat
/// transcript for one artifact's mini-thread, written to by [`chat_artifact`]
/// (user turns) and [`ao_engine::artifact_task_status::ArtifactTaskCompletionSink`]
/// (assistant replies). Read-only tail pagination, same shape as
/// `get_messages`'s `last`-based path.
pub async fn get_artifact_chat(
    State(state): State<Arc<AppState>>,
    Path((_agent_id, artifact_id)): Path<(String, String)>,
    Query(query): Query<GetArtifactChatQuery>,
) -> Result<Json<ArtifactChatResponse>, AppError> {
    let n = query.last.unwrap_or(ARTIFACT_CHAT_DEFAULT_LAST);
    let thread = state
        .persistence
        .threads
        .ensure_artifact_thread(&artifact_id)
        .await?;
    let path = PathBuf::from(&thread.transcript_path);
    let PaginatedResponse { entries, cursor } =
        state.persistence.transcripts.read_tail_at(&path, n).await?;
    Ok(Json(ArtifactChatResponse { entries, cursor }))
}

/// DELETE /agents/{agent_id}/artifacts/{artifact_id}
pub async fn delete_artifact(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state.persistence.artifacts.delete(&agent_id, &artifact_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// PUT /agents/{agent_id}/artifacts/{artifact_id}/pin — save-to-Assets toggle.
/// Metadata-only; never touches the payload.
pub async fn set_artifact_pinned(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    Json(req): Json<SetPinnedRequest>,
) -> Result<Json<ArtifactRecord>, AppError> {
    let record = state
        .persistence
        .artifacts
        .set_pinned(&agent_id, &artifact_id, req.pinned)
        .await?;
    Ok(Json(record))
}

/// GET /artifacts/pinned — every pinned artifact across every agent, for the
/// global Assets page. Metadata only (no payload bytes), same as
/// [`list_artifacts`].
pub async fn list_pinned_artifacts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<PinnedArtifact>>, AppError> {
    let pinned = state.persistence.artifacts.list_pinned_across_agents().await?;
    let response = pinned
        .into_iter()
        .map(|(agent_id, record)| PinnedArtifact { agent_id, record })
        .collect();
    Ok(Json(response))
}

/// PUT /agents/{agent_id}/artifacts/{artifact_id}/group — files a pinned
/// artifact under a group (or clears it back to ungrouped with
/// `group_id: null`) for the Assets sidebar's collapsible sections.
pub async fn set_artifact_group(
    State(state): State<Arc<AppState>>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    Json(req): Json<SetArtifactGroupRequest>,
) -> Result<Json<ArtifactRecord>, AppError> {
    let record = state
        .persistence
        .artifacts
        .set_group(&agent_id, &artifact_id, req.group_id)
        .await?;
    Ok(Json(record))
}

/// POST /artifact-groups — create a new group to file pinned artifacts under.
pub async fn create_artifact_group(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateArtifactGroupRequest>,
) -> Result<Json<ArtifactGroup>, AppError> {
    let group = state.persistence.artifact_groups.create(req.name).await?;
    Ok(Json(group))
}

/// GET /artifact-groups — every group, for the Assets sidebar and the
/// group-picker modal.
pub async fn list_artifact_groups(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ArtifactGroup>>, AppError> {
    let groups = state.persistence.artifact_groups.list().await?;
    Ok(Json(groups))
}

/// DELETE /artifact-groups/{group_id} — deletes the group and unfiles every
/// artifact that referenced it (across every agent) so nothing is left
/// pointing at a group that no longer exists.
pub async fn delete_artifact_group(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.persistence.artifact_groups.delete(&group_id).await?;
    state.persistence.artifacts.clear_group_across_agents(&group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod chat_artifact_tests {
    use std::collections::HashMap;

    use ao_engine::artifact_task_status::{ArtifactTaskCompletionSink, ArtifactTaskStatusStore};
    use ao_engine_tools_core::background_agents::handle::{TaskFinalReport, TaskFinalStatus};
    use ao_engine_tools_core::delegate_completion_sink::DelegateCompletionSink;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::thread::ThreadScope;
    use chrono::Utc;

    use super::*;

    // --- build_chat_seed_prompt: pure-function coverage (no server infra) ---

    fn ledger_entry(note: &str, source: IntentSource) -> IntentLedgerEntry {
        IntentLedgerEntry {
            timestamp: Utc::now(),
            source,
            intent_note: Some(note.to_string()),
            source_message_id: None,
        }
    }

    #[test]
    fn seed_prompt_includes_message_ledger_and_transcript() {
        let ledger = vec![ledger_entry("Created the dashboard.", IntentSource::Create)];
        let transcript = vec![
            ChatTranscriptTurn {
                role: ChatTurnRole::User,
                content: "Make the header blue.".to_string(),
            },
            ChatTranscriptTurn {
                role: ChatTurnRole::Assistant,
                content: "Done — header is now blue.".to_string(),
            },
        ];

        let seed = build_chat_seed_prompt("Now make it green instead.", &ledger, &transcript);

        assert!(seed.contains("Now make it green instead."));
        assert!(seed.contains("Recent edit history"));
        assert!(seed.contains("Created the dashboard."));
        assert!(seed.contains("Recent conversation"));
        assert!(seed.contains("User: Make the header blue."));
        assert!(seed.contains("Assistant: Done — header is now blue."));
        // The whole point of the ledger: the subagent must be told to keep
        // filling it in, scoped to this chat turn.
        assert!(seed.contains("intent_note"));
    }

    #[test]
    fn seed_prompt_omits_empty_sections_but_keeps_intent_note_instruction() {
        let seed = build_chat_seed_prompt("Just this message.", &[], &[]);
        assert!(seed.contains("Just this message."));
        assert!(!seed.contains("Recent edit history"));
        assert!(!seed.contains("Recent conversation"));
        assert!(seed.contains("intent_note"));
    }

    #[test]
    fn seed_prompt_caps_ledger_to_last_n_entries_newest_last() {
        let ledger: Vec<IntentLedgerEntry> = (0..(CHAT_SEED_LEDGER_MAX_ENTRIES + 3))
            .map(|i| ledger_entry(&format!("edit #{i}"), IntentSource::Chat))
            .collect();

        let seed = build_chat_seed_prompt("adjust it", &ledger, &[]);

        // The oldest entries fell off the front...
        assert!(!seed.contains("edit #0"));
        assert!(!seed.contains("edit #1"));
        assert!(!seed.contains("edit #2"));
        // ...only the most recent CHAT_SEED_LEDGER_MAX_ENTRIES survive, in order.
        let last_idx = CHAT_SEED_LEDGER_MAX_ENTRIES + 2;
        assert!(seed.contains(&format!("edit #{last_idx}")));
        let first_kept_idx = 3;
        assert!(seed.contains(&format!("edit #{first_kept_idx}")));
        let pos_first_kept = seed.find(&format!("edit #{first_kept_idx}")).unwrap();
        let pos_last = seed.find(&format!("edit #{last_idx}")).unwrap();
        assert!(pos_first_kept < pos_last, "ledger entries must stay oldest-first");
    }

    #[test]
    fn seed_prompt_caps_transcript_to_last_n_turns_newest_last() {
        let transcript: Vec<ChatTranscriptTurn> = (0..(CHAT_SEED_TRANSCRIPT_MAX_TURNS + 2))
            .map(|i| ChatTranscriptTurn {
                role: ChatTurnRole::User,
                content: format!("turn #{i}"),
            })
            .collect();

        let seed = build_chat_seed_prompt("adjust it", &[], &transcript);

        // Trailing `\n` in the needle avoids "turn #1" false-matching inside
        // the still-present "turn #10"/"turn #11" lines.
        assert!(!seed.contains("turn #0\n"));
        assert!(!seed.contains("turn #1\n"));
        let last_idx = CHAT_SEED_TRANSCRIPT_MAX_TURNS + 1;
        assert!(seed.contains(&format!("turn #{last_idx}")));
    }

    // --- chat_artifact handler: end-to-end wiring through spawn_artifact_agent ---

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "Test agent".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            max_turns: None,
        }
    }

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    async fn create_test_artifact(state: &Arc<AppState>, agent_id: &str) -> String {
        let record = state
            .persistence
            .artifacts
            .create(
                agent_id,
                NewArtifact {
                    title: "Test dashboard".to_string(),
                    kind: ArtifactKind::Cards,
                    format: PayloadFormat::Json,
                    payload: br#"{"items": []}"#.to_vec(),
                    refresh_intent: RefreshIntent::None,
                    origin_intent: None,
                    capabilities: vec![],
                    source_message_id: None,
                    intent_note: Some("Initial creation.".to_string()),
                },
            )
            .await
            .expect("create artifact");
        record.id
    }

    /// Acceptance criterion: the chat route invokes `spawn_artifact_agent`
    /// with `ArtifactAgentMode::ChatAdjust` and a seed built from the user's
    /// message. `spawn_artifact_agent` fire-and-forgets the actual subagent
    /// run (see its doc comment), so the only thing observable synchronously
    /// from the HTTP layer is that the call was reached and succeeded — a
    /// 202 with a real `task_id`. It cannot succeed here unless `chat_artifact`
    /// actually called through to `spawn_artifact_agent`: the id only comes
    /// from a live `BackgroundAgentId`, and the `AoError`s that would
    /// short-circuit first are exercised separately below (unknown artifact,
    /// blank message). The seed's exact shape (message + ledger + transcript)
    /// is covered deterministically by the `build_chat_seed_prompt` unit
    /// tests above, since the assembled string isn't itself part of the HTTP
    /// response contract.
    #[tokio::test]
    async fn chat_artifact_invokes_spawn_artifact_agent_and_returns_202_with_task_id() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        let result = chat_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
            Json(ChatArtifactRequest {
                message: "Make the header blue.".to_string(),
                transcript: vec![ChatTranscriptTurn {
                    role: ChatTurnRole::User,
                    content: "Earlier turn.".to_string(),
                }],
            }),
        )
        .await;
        let (status, Json(body)) = match result {
            Ok(v) => v,
            Err(e) => panic!("chat_artifact should succeed, got error: {:?}", e.0),
        };

        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(!body.task_id.is_empty());
    }

    #[tokio::test]
    async fn chat_artifact_unknown_artifact_returns_not_found_like_regenerate() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        let err = chat_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), "ghost-artifact".to_string())),
            Json(ChatArtifactRequest {
                message: "Adjust it.".to_string(),
                transcript: vec![],
            }),
        )
        .await
        .expect_err("unknown artifact should fail");

        assert!(matches!(err.0, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn chat_artifact_blank_message_returns_validation_error() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        let err = chat_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id)),
            Json(ChatArtifactRequest {
                message: "   ".to_string(),
                transcript: vec![],
            }),
        )
        .await
        .expect_err("blank message should fail");

        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    // --- get_artifact_task_status: bogus id must never 500 -----------------

    #[tokio::test]
    async fn task_status_unknown_id_returns_unknown_not_error() {
        let (state, _tmp) = setup_state().await;

        let Json(body) = get_artifact_task_status(
            State(Arc::clone(&state)),
            Path((
                "agent-1".to_string(),
                "ghost-artifact".to_string(),
                "no-such-task".to_string(),
            )),
        )
        .await;

        assert_eq!(body.status, "unknown");
        assert!(body.error.is_none());
    }

    #[tokio::test]
    async fn task_status_reflects_store_state() {
        let (state, _tmp) = setup_state().await;
        state
            .artifact_task_status
            .mark_running("task-running".to_string(), "artifact-1".to_string());

        let Json(body) = get_artifact_task_status(
            State(Arc::clone(&state)),
            Path((
                "agent-1".to_string(),
                "artifact-1".to_string(),
                "task-running".to_string(),
            )),
        )
        .await;

        assert_eq!(body.status, "running");
    }

    // --- get_artifact: running_task_id spinner-resume field -----------------

    /// Acceptance criteria for the spinner-resume backend half: `get_artifact`
    /// surfaces `running_task_id` as `Some(task_id)` while a background run is
    /// in flight for the artifact, serializes it under exactly that snake_case
    /// key (the pinned frontend contract), and drops back to `None` once the
    /// run reaches a terminal state — never reporting a finished run as still
    /// running.
    #[tokio::test]
    async fn get_artifact_exposes_running_task_id_only_while_running() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        // No in-flight task yet → field is absent/None.
        let Json(before) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert!(before.running_task_id.is_none());

        // Mark a task running for THIS artifact → getArtifact surfaces it.
        state
            .artifact_task_status
            .mark_running("task-xyz".to_string(), artifact_id.clone());
        let Json(during) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert_eq!(during.running_task_id.as_deref(), Some("task-xyz"));

        // The field serializes under the exact snake_case key the frontend
        // codes against.
        let json = serde_json::to_value(&during).expect("serialize ArtifactWithPayload");
        assert_eq!(
            json.get("running_task_id").and_then(|v| v.as_str()),
            Some("task-xyz"),
        );

        // Terminal state → the run must no longer be reported as running.
        state.artifact_task_status.mark_terminal(
            "task-xyz".to_string(),
            TaskFinalStatus::Completed,
            None,
        );
        let Json(after) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id)),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert!(after.running_task_id.is_none());
    }

    // --- chat_artifact: durable transcript persistence ----------------------

    /// Acceptance criterion from the task spec: `chat_artifact` must persist
    /// the user's message to the artifact's durable chat transcript BEFORE
    /// spawning the subagent, so it survives even if that subagent run later
    /// fails. Verified here by reading the transcript straight back via
    /// `get_artifact_chat` after a successful `chat_artifact` call.
    #[tokio::test]
    async fn chat_artifact_persists_user_message_to_durable_transcript() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        let (status, _body) = match chat_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
            Json(ChatArtifactRequest {
                message: "Make the header blue.".to_string(),
                transcript: vec![],
            }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("chat_artifact should succeed, got error: {:?}", e.0),
        };
        assert_eq!(status, StatusCode::ACCEPTED);

        let Json(chat) = match get_artifact_chat(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id)),
            Query(GetArtifactChatQuery { last: None }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact_chat should succeed, got error: {:?}", e.0),
        };

        assert_eq!(chat.entries.len(), 1, "user message must be persisted before spawn");
        assert_eq!(chat.entries[0].content, "Make the header blue.");
        assert!(matches!(&chat.entries[0].role, TranscriptRole::System(role) if role == "user"));
    }

    #[tokio::test]
    async fn get_artifact_chat_empty_when_no_messages_yet() {
        let (state, _tmp) = setup_state().await;

        let Json(chat) = match get_artifact_chat(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), "never-chatted-artifact".to_string())),
            Query(GetArtifactChatQuery { last: None }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact_chat should succeed, got error: {:?}", e.0),
        };

        assert!(chat.entries.is_empty());
        assert!(chat.cursor.is_none());
    }

    // --- artifact chat round-trips through ThreadStore ----------------------

    /// Acceptance criterion: the artifact chat mini-thread is a first-class
    /// `Thread` row, not bespoke path I/O. After `chat_artifact` persists the
    /// user's turn and a simulated subagent completion appends the assistant's
    /// reply, a `Thread` row scoped `ThreadScope::Artifact` must exist and
    /// `get_artifact_chat` must read both turns back — proving both write
    /// paths and the read path all resolve through the same `ThreadStore`.
    #[tokio::test]
    async fn artifact_chat_round_trips_through_thread_store() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        // (a) user turn, via chat_artifact.
        let (status, _body) = match chat_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
            Json(ChatArtifactRequest {
                message: "Make the header blue.".to_string(),
                transcript: vec![],
            }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("chat_artifact should succeed, got error: {:?}", e.0),
        };
        assert_eq!(status, StatusCode::ACCEPTED);

        // A Thread row must now exist, scoped to this artifact.
        let thread_id = ao_persistence::thread_store::ThreadStore::artifact_thread_id(&artifact_id);
        let thread = state
            .persistence
            .threads
            .get(&thread_id)
            .await
            .unwrap()
            .expect("artifact thread row should exist after chat_artifact");
        match &thread.scope {
            ThreadScope::Artifact { artifact_id: scoped } => assert_eq!(scoped, &artifact_id),
            other => panic!("expected ThreadScope::Artifact, got {other:?}"),
        }

        // Artifact threads must stay invisible to the agent's regular
        // tab-strip listing.
        let listed = state.persistence.threads.list_for_agent("agent-1").await.unwrap();
        assert!(!listed.iter().any(|t| t.id == thread_id));

        // (b) assistant turn, via a simulated completion sink notification —
        // the same path ArtifactTaskCompletionSink::notify takes on a real
        // subagent run's `Completed` terminal event.
        let sink = ArtifactTaskCompletionSink {
            status: Arc::new(ArtifactTaskStatusStore::new()),
            persistence: Arc::clone(&state.persistence),
            agent_id: "agent-1".to_string(),
            artifact_id: artifact_id.clone(),
        };
        sink.notify(
            "agent-1",
            "task-1",
            &TaskFinalReport::completed(None),
            &thread.transcript_path,
        )
        .await;

        // Both turns are readable back through get_artifact_chat.
        let Json(chat) = match get_artifact_chat(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id)),
            Query(GetArtifactChatQuery { last: None }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact_chat should succeed, got error: {:?}", e.0),
        };

        assert_eq!(chat.entries.len(), 2, "both user and assistant turns must round-trip");
        assert!(matches!(&chat.entries[0].role, TranscriptRole::System(role) if role == "user"));
        assert_eq!(chat.entries[0].content, "Make the header blue.");
        assert!(matches!(&chat.entries[1].role, TranscriptRole::System(role) if role == "assistant"));
        assert_eq!(chat.entries[1].content, "Initial creation.");
    }

    // --- undo_artifact: full round trip --------------------------------

    /// Acceptance round trip from the task spec: create → edit (a snapshot
    /// is pushed) → undo (body restored, `undo_available` reflects the
    /// remaining depth) → undo down to empty → 409. Exercised through the
    /// actual HTTP handlers (`refresh_artifact`, `get_artifact`,
    /// `undo_artifact`), not the store directly, so this also proves the
    /// route wiring (including `undo_available` surfacing on `GET`).
    #[tokio::test]
    async fn undo_round_trips_create_edit_undo_then_conflict_on_empty_history() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        // Fresh artifact: nothing to undo yet.
        let Json(fresh) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert!(!fresh.undo_available, "a brand-new artifact has no prior body to undo to");
        let original_payload = fresh.payload.clone();

        // Edit via the PUT .../refresh route (main-thread-edit surface) —
        // this must push a snapshot at the shared `ArtifactStore::refresh`
        // choke point.
        let Json(refreshed) = match refresh_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
            Json(RefreshArtifactRequest {
                payload: serde_json::json!({"items": [{"title": "New item"}]}),
            }),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("refresh_artifact should succeed, got error: {:?}", e.0),
        };
        assert_eq!(refreshed.history.len(), 1, "the edit must push exactly one snapshot");

        let Json(after_edit) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert!(after_edit.undo_available, "GET must surface undo_available after an edit");

        // Undo: body restored to the pre-edit content, undo_available flips
        // back to false since that was the only edit.
        let Json(undone) = match undo_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("undo_artifact should succeed while history is non-empty, got error: {:?}", e.0),
        };
        assert!(!undone.undo_available, "the only snapshot was just consumed");
        assert!(undone.record.history.is_empty());
        assert_eq!(
            undone.record.intent_ledger.last().unwrap().source,
            IntentSource::Undo,
            "undo must append an Undo-sourced ledger entry"
        );

        let Json(restored) = match get_artifact(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), artifact_id.clone())),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => panic!("get_artifact should succeed, got error: {:?}", e.0),
        };
        assert_eq!(restored.payload, original_payload, "body must be restored to the pre-edit content");
        assert_eq!(restored.record.checksum_sha256, undone.record.checksum_sha256);
        assert!(!restored.undo_available);

        // Undo again with nothing left: 409 Conflict.
        match undo_artifact(State(Arc::clone(&state)), Path(("agent-1".to_string(), artifact_id))).await {
            Ok(Json(v)) => panic!("undo with empty history should fail, got success: {:?}", v),
            Err(e) => assert!(matches!(e.0, AoError::Conflict(_))),
        }
    }

    /// A second edit after the first undo pushes a fresh snapshot again —
    /// undo isn't a one-shot: it's available again after any subsequent
    /// edit, and undoing down through multiple edits drains the stack one
    /// at a time (`undo_available` stays true until the last one).
    #[tokio::test]
    async fn undo_available_reflects_remaining_depth_across_multiple_edits() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let artifact_id = create_test_artifact(&state, "agent-1").await;

        for i in 0..3 {
            if let Err(e) = refresh_artifact(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), artifact_id.clone())),
                Json(RefreshArtifactRequest {
                    payload: serde_json::json!({"items": [{"title": format!("item {i}")}]}),
                }),
            )
            .await
            {
                panic!("refresh_artifact should succeed, got error: {:?}", e.0);
            }
        }

        // 3 edits pushed 3 snapshots; undo 3 times drains them with
        // undo_available true after the first two and false after the last.
        for expect_more_after in [true, true, false] {
            let Json(resp) = match undo_artifact(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), artifact_id.clone())),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => panic!("undo_artifact should succeed while history is non-empty, got error: {:?}", e.0),
            };
            assert_eq!(resp.undo_available, expect_more_after);
        }
    }
}
