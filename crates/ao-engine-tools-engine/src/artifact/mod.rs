pub mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{IoTool, Registry, RunnerContext, ToolOutput};
use ao_persistence::artifact_store::NewArtifact;
use ao_protocol::{
    artifact::{ArtifactKind, CapabilitySpec, IntentSource, OriginIntent, PayloadFormat, RefreshIntent},
    error::AoError,
};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Soft/hard char caps on the payload's string form (the JSON-serialized
/// object for typed kinds, or the raw markup for `html`). Two orders of
/// magnitude above Memory's prose caps (`memory/store.rs`'s 2000/8000
/// `ENTRY_CHAR_SOFT`/`ENTRY_CHAR_HARD`) — artifacts are expected to be fat by
/// design (the over-fetch-on-render convention embeds a whole dataset up
/// front), so this ceiling guards against a runaway dump rather than against
/// thoroughness.
pub const PAYLOAD_CHAR_SOFT: usize = 500_000;
pub const PAYLOAD_CHAR_HARD: usize = 5_000_000;

/// ArtifactWrite — persists a renderable artifact (typed dataset or freeform
/// HTML) via the injected [`ao_persistence::artifact_store::ArtifactStore`]
/// and returns its id. Same persist-a-record-via-injected-store-return-id
/// shape as `MemoryWrite`, without Memory's dedup/contradiction/eviction
/// machinery — an artifact has no equivalent notion of "restates an existing
/// entry".
pub struct ArtifactWrite;

/// Map the model-facing `renderer` string to the stored `(kind, format)`
/// pair. `format` is derived, not independently specified — every typed
/// renderer stores JSON, `html` always stores HTML — which keeps the
/// well-formed pairing entirely on the tool's shoulders rather than trusting
/// the caller to keep two fields in sync.
fn renderer_from_str(s: &str) -> Option<(ArtifactKind, PayloadFormat)> {
    match s {
        "list" => Some((ArtifactKind::List, PayloadFormat::Json)),
        "cards" => Some((ArtifactKind::Cards, PayloadFormat::Json)),
        "table" => Some((ArtifactKind::Table, PayloadFormat::Json)),
        "board" => Some((ArtifactKind::Board, PayloadFormat::Json)),
        "metric" => Some((ArtifactKind::Metric, PayloadFormat::Json)),
        "chart" => Some((ArtifactKind::Chart, PayloadFormat::Json)),
        "html" => Some((ArtifactKind::Html, PayloadFormat::Html)),
        _ => None,
    }
}

/// Inverse of [`renderer_from_str`] — used only to name an existing
/// artifact's renderer back to the model when an update-by-id call's
/// renderer doesn't match what the artifact was originally created with.
fn kind_to_renderer_str(kind: &ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::List => "list",
        ArtifactKind::Cards => "cards",
        ArtifactKind::Table => "table",
        ArtifactKind::Board => "board",
        ArtifactKind::Metric => "metric",
        ArtifactKind::Chart => "chart",
        ArtifactKind::Html => "html",
        ArtifactKind::Unknown => "unknown",
    }
}

/// Reports the artifact's actually-persisted `refresh_intent` back to the
/// model, rather than echoing whatever the caller passed — the two only
/// diverge on an update-by-id call, since [`ao_persistence::artifact_store::ArtifactStore::refresh`]
/// leaves `refresh_intent` untouched.
fn refresh_intent_to_str(intent: RefreshIntent) -> &'static str {
    match intent {
        RefreshIntent::None => "none",
        RefreshIntent::WholeArtifact => "whole_artifact",
        RefreshIntent::Brokered => "brokered",
        RefreshIntent::Unknown => "unknown",
    }
}

/// Cap on the fallback intent-ledger note derived from a title when the
/// caller omits `intent_note` — long enough to carry a meaningful title,
/// short enough that a runaway title can't bloat every ledger entry.
const INTENT_NOTE_FALLBACK_MAX_CHARS: usize = 200;

/// Resolve the note to record on this write's intent-ledger entry: the
/// caller-supplied `intent_note` if present and non-blank, otherwise a
/// truncated form of `title`. An intent-ledger entry is never left with an
/// empty note just because the model omitted the optional field — see
/// `ao_protocol::artifact::IntentLedgerEntry::intent_note`.
fn resolve_intent_note(intent_note: Option<&str>, title: &str) -> Option<String> {
    if let Some(note) = intent_note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let trimmed_title = title.trim();
    if trimmed_title.is_empty() {
        return None;
    }
    if trimmed_title.chars().count() <= INTENT_NOTE_FALLBACK_MAX_CHARS {
        Some(trimmed_title.to_string())
    } else {
        let truncated: String =
            trimmed_title.chars().take(INTENT_NOTE_FALLBACK_MAX_CHARS).collect();
        Some(format!("{truncated}…"))
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Best-effort structural gate on a typed payload's shape, run in addition to
/// the "payload is a JSON object" check in `invoke`. This deliberately does
/// NOT enforce a full strict schema — `TypedArtifactBodies.tsx`/
/// `payloadGuards.ts` stay tolerant of missing fields and synonym key names
/// (e.g. a card's title may arrive as "name" or "heading") as defense in
/// depth for artifacts written before this gate existed, and this function
/// accepts the same synonyms. It only catches shapes that are structurally
/// guaranteed to render wrong or blank — e.g. `columns` holding `{key,
/// label}` objects instead of plain strings, which is silently
/// JSON.stringify'd into the header row and never matches any row key, so
/// every cell renders empty. Rejecting those here, with a corrected example
/// in the message, gives the calling agent a chance to self-correct instead
/// of silently persisting a payload the renderer can't use.
fn validate_typed_payload(renderer_str: &str, payload: &Value) -> Result<(), String> {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => return Ok(()), // non-object payloads are already rejected earlier
    };

    fn find_array<'a>(obj: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Vec<Value>> {
        keys.iter().find_map(|k| obj.get(*k)).and_then(Value::as_array)
    }

    match renderer_str {
        "list" => {
            if find_array(obj, &["items", "list", "data", "entries"]).is_none() {
                return Err(
                    "list payload needs an 'items' array (strings, or objects with 'title'/\
                     'subtitle'/'description'). Example: {\"items\": [\"Task A\", {\"title\": \
                     \"Task B\", \"subtitle\": \"In progress\"}]}."
                        .to_string(),
                );
            }
            Ok(())
        }
        "cards" => {
            if find_array(obj, &["items", "cards", "data", "entries"]).is_none() {
                return Err(
                    "cards payload needs an 'items' array of objects with 'title'/'subtitle'/\
                     'description'/'image'. Example: {\"items\": [{\"title\": \"Task A\", \
                     \"subtitle\": \"High priority\"}]}."
                        .to_string(),
                );
            }
            Ok(())
        }
        "table" => {
            let columns = find_array(obj, &["columns", "headers"]);
            let rows = find_array(obj, &["rows", "data"]);
            match (columns, rows) {
                (Some(cols), Some(_)) => {
                    if let Some(bad) = cols.iter().find(|c| !c.is_string()) {
                        return Err(format!(
                            "table payload's 'columns' must be an array of plain strings (the \
                             column headers) — got a {} element ({bad}). Do not use {{key, \
                             label}} objects. Example: {{\"columns\": [\"Task\", \"Owner\"], \
                             \"rows\": [[\"Write docs\", \"Alex\"], [\"Fix bug\", \"Sam\"]]}}. \
                             Rows may also be objects keyed by the exact column strings instead \
                             of positional arrays.",
                            value_type_name(bad)
                        ));
                    }
                    Ok(())
                }
                _ => Err(
                    "table payload needs a 'columns' array (plain column-header strings) and a \
                     'rows' array. Example: {\"columns\": [\"Task\", \"Owner\"], \"rows\": \
                     [[\"Write docs\", \"Alex\"]]}."
                        .to_string(),
                ),
            }
        }
        "board" => {
            if find_array(obj, &["columns", "lanes"]).is_none() {
                return Err(
                    "board payload needs a 'columns' array of {\"title\", \"items\"} objects. \
                     Example: {\"columns\": [{\"title\": \"Backlog\", \"items\": [{\"title\": \
                     \"Task A\"}]}]}."
                        .to_string(),
                );
            }
            Ok(())
        }
        "metric" => {
            let has_array = find_array(obj, &["metrics", "data"]).is_some();
            let has_single = obj.contains_key("value");
            if !has_array && !has_single {
                return Err(
                    "metric payload needs either a 'metrics' array of {\"label\", \"value\"} \
                     objects, or a single flat {\"label\", \"value\"} object. Example: \
                     {\"metrics\": [{\"label\": \"Open PRs\", \"value\": 3}]}."
                        .to_string(),
                );
            }
            Ok(())
        }
        "chart" => {
            let labels_ok = obj.get("labels").is_some_and(Value::is_array);
            let series_ok = obj.get("series").and_then(Value::as_array).is_some_and(|series| {
                series.iter().all(|item| item.get("values").is_some_and(Value::is_array))
            });
            if !labels_ok || !series_ok {
                return Err(
                    "chart payload needs a 'labels' array and a 'series' array of {\"name\", \
                     \"values\": [...]} objects, one 'values' number per label. Example: \
                     {\"labels\": [\"Mon\", \"Tue\"], \"series\": [{\"name\": \"Visits\", \
                     \"values\": [10, 14]}]}."
                        .to_string(),
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[async_trait]
impl IoTool for ArtifactWrite {
    fn name(&self) -> &str {
        "ArtifactWrite"
    }

    fn description(&self) -> &str {
        prompt::ARTIFACT_WRITE_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::artifact_write_schema()
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // 1. Parse + validate (`.get(...).and_then(...)`, per `memory/write.rs`).
        let title = match input.get("title").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("Missing required field: title", false)),
        };

        let renderer_str = match input.get("renderer").and_then(Value::as_str) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("Missing required field: renderer", false)),
        };
        let (kind, format) = match renderer_from_str(renderer_str) {
            Some(pair) => pair,
            None => {
                return Ok(ToolOutput::error(
                    format!(
                        "Invalid renderer '{renderer_str}'. Must be one of: list, cards, \
                         table, board, metric, chart, html."
                    ),
                    false,
                ));
            }
        };

        let payload = match input.get("payload") {
            Some(v) => v.clone(),
            None => return Ok(ToolOutput::error("Missing required field: payload", false)),
        };
        // Payload-matches-kind (the sandbox invariant itself is enforced at
        // the renderer, not here — this only rejects a malformed pairing
        // early): typed kinds require a JSON object, `html` requires a string.
        let payload_str = match format {
            PayloadFormat::Json => {
                if !payload.is_object() {
                    return Ok(ToolOutput::error(
                        format!(
                            "payload must be a JSON object for renderer '{renderer_str}' (got {}).",
                            value_type_name(&payload)
                        ),
                        false,
                    ));
                }
                if let Err(msg) = validate_typed_payload(renderer_str, &payload) {
                    return Ok(ToolOutput::error(
                        format!("payload shape is wrong for renderer '{renderer_str}': {msg}"),
                        false,
                    ));
                }
                match serde_json::to_string(&payload) {
                    Ok(s) => s,
                    Err(e) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to serialize payload: {e}"),
                            false,
                        ));
                    }
                }
            }
            PayloadFormat::Html => match payload.as_str() {
                Some(s) => s.to_string(),
                None => {
                    return Ok(ToolOutput::error(
                        format!(
                            "payload must be an HTML string for renderer 'html' (got {}).",
                            value_type_name(&payload)
                        ),
                        false,
                    ));
                }
            },
        };

        let refresh_intent_str =
            input.get("refresh_intent").and_then(Value::as_str).unwrap_or("none");
        let refresh_intent = match refresh_intent_str {
            "none" => RefreshIntent::None,
            "whole_artifact" => RefreshIntent::WholeArtifact,
            other => {
                return Ok(ToolOutput::error(
                    format!(
                        "Invalid refresh_intent '{other}'. Must be one of: none, whole_artifact."
                    ),
                    false,
                ));
            }
        };

        let refresh_prompt = input.get("refresh_prompt").and_then(Value::as_str);
        let origin_intent = match refresh_intent {
            RefreshIntent::WholeArtifact => match refresh_prompt {
                Some(p) if !p.trim().is_empty() => {
                    Some(OriginIntent { refresh_prompt: p.to_string() })
                }
                _ => {
                    return Ok(ToolOutput::error(
                        "refresh_prompt is required when refresh_intent='whole_artifact'.",
                        false,
                    ));
                }
            },
            _ => None,
        };

        // Optional, point-in-time note on why this call is happening — old
        // callers that never pass it are unaffected. When present it is
        // recorded verbatim on the artifact's intent ledger below; when
        // absent, `resolve_intent_note` falls back to a truncated title
        // rather than leaving the ledger entry's note empty.
        let intent_note = input.get("intent_note").and_then(Value::as_str).map(|s| s.to_string());

        let capabilities: Vec<CapabilitySpec> = match input.get("capabilities") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(caps) => caps,
                Err(e) => {
                    return Ok(ToolOutput::error(format!("Invalid capabilities: {e}"), false));
                }
            },
            None => Vec::new(),
        };

        // Size cap on the payload's string form — mirrors Memory's
        // soft-warn/hard-reject shape, scaled up per the doc comment on
        // PAYLOAD_CHAR_SOFT/PAYLOAD_CHAR_HARD above.
        let char_len = payload_str.chars().count();
        if char_len > PAYLOAD_CHAR_HARD {
            return Ok(ToolOutput::error(
                format!(
                    "payload is too large ({char_len} chars). Maximum is {PAYLOAD_CHAR_HARD} chars."
                ),
                false,
            ));
        }
        let warning = if char_len > PAYLOAD_CHAR_SOFT {
            Some(format!(
                "⚠ payload is large ({char_len}/{PAYLOAD_CHAR_SOFT} chars). Large payloads are \
                 expected under the over-fetch convention, but consider trimming unused fields."
            ))
        } else {
            None
        };

        // 2. Resolve the store (exactly `memory/write.rs`'s graceful-`None` shape).
        let store = match &ctx.artifact_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Artifact store not available in this context.",
                    false,
                ));
            }
        };

        // 3. Persist. `source_message_id` is stamped from ctx — the producing
        // turn — never taken from model input, so a thread bubble can always
        // resolve its own artifact inline regardless of what the model passed.
        //
        // An `id` routes to an in-place update instead of a create: fetch the
        // existing record, refuse to silently reformat it (renderer must
        // match what it was authored with), then replace its payload via the
        // same `refresh` the `PUT /refresh` HTTP route uses. That call only
        // ever touches the payload blob plus size/checksum/refresh
        // bookkeeping, so title, refresh_intent, origin_intent, pin state,
        // and group membership all survive untouched for free.
        let record = match input.get("id").and_then(Value::as_str) {
            Some(id) => {
                let existing = match store.get(&ctx.agent_id, id).await {
                    Ok(r) => r,
                    Err(AoError::ArtifactNotFound(_)) => {
                        return Ok(ToolOutput::error(
                            format!(
                                "No artifact found with id '{id}'. Omit 'id' to create a new \
                                 artifact instead of updating one."
                            ),
                            true,
                        ));
                    }
                    Err(e) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to look up artifact '{id}': {e}"),
                            false,
                        ));
                    }
                };

                if existing.kind != kind || existing.format != format {
                    return Ok(ToolOutput::error(
                        format!(
                            "Artifact '{id}' was created as renderer '{}'. An update-by-id call \
                             cannot change an artifact's renderer — omit 'id' to create a new \
                             '{renderer_str}' artifact instead.",
                            kind_to_renderer_str(&existing.kind)
                        ),
                        true,
                    ));
                }

                let intent_note_for_edit = resolve_intent_note(intent_note.as_deref(), &existing.title);
                // Defaults to `Chat` for the main agent thread's own
                // conversational edits and the chat-adjust mini-thread
                // subagent (neither sets an override); a whole-artifact
                // regenerate run's synthetic context overrides this to
                // `Regenerate` — see `RunnerContext::artifact_intent_source`.
                let source = ctx.artifact_intent_source.unwrap_or(IntentSource::Chat);
                match store
                    .refresh(
                        &ctx.agent_id,
                        id,
                        payload_str.as_bytes(),
                        source,
                        intent_note_for_edit,
                        ctx.current_message_id.clone(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(AoError::ArtifactNotFound(_)) => {
                        return Ok(ToolOutput::error(
                            format!(
                                "Artifact '{id}' was deleted before this update could be applied."
                            ),
                            true,
                        ));
                    }
                    Err(e) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to update artifact '{id}': {e}"),
                            false,
                        ));
                    }
                }
            }
            None => {
                let intent_note_for_create = resolve_intent_note(intent_note.as_deref(), &title);
                store
                    .create(
                        &ctx.agent_id,
                        NewArtifact {
                            title,
                            kind,
                            format,
                            payload: payload_str.into_bytes(),
                            refresh_intent,
                            origin_intent,
                            capabilities,
                            source_message_id: ctx.current_message_id.clone(),
                            intent_note: intent_note_for_create,
                        },
                    )
                    .await?
            }
        };

        // 4. Return.
        let mut result = json!({
            "id": record.id,
            "renderer": renderer_str,
            "refresh_intent": refresh_intent_to_str(record.refresh_intent),
            "title": record.title,
        });
        if let Some(w) = warning {
            result["warning"] = json!(w);
        }
        Ok(ToolOutput::structured(result))
    }
}

/// Register the Artifact tool family into the supplied [`Registry`].
/// Mirrors `memory::register_memory_tools`.
pub fn register_artifact_tools(registry: &mut Registry) {
    registry.register_io(Arc::new(ArtifactWrite));
}
