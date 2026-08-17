use std::collections::HashMap;
use std::path::PathBuf;

use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FORM_REQUEST: &str = "form_request";
pub const FORM_ANSWER: &str = "form_answer";
pub const FORM_DISMISSED: &str = "form_dismissed";
/// A still-pending form was dropped because a newer one replaced it on the
/// same thread — see [`form_withdrawn_entry`] and [`persist_posted_form`].
pub const FORM_WITHDRAWN: &str = "form_withdrawn";

/// Metadata shape for a `form_request` transcript entry.
/// Written by the runner immediately after `AskUserQuestionWithForm` returns
/// an async outcome; read by the form routes to validate submissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormRequestMeta {
    pub form_id: String,
    /// Full async form spec JSON (title, intro, and fields in flat wire shape).
    pub spec: Value,
    /// Always `"async"` for entries written via this path.
    pub mode: String,
}

/// Metadata shape for a `form_answer` transcript entry (written by the form route).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormAnswerMeta {
    pub form_id: String,
    /// Map of field_id → answer value as returned by the client.
    pub values: HashMap<String, Value>,
    /// Snapshot of the answered form's own spec (title/intro/fields, flat
    /// wire shape — see [`form_answer_spec_snapshot`]), captured at answer
    /// time so this entry is self-contained: the UI renders the SAME
    /// interactive form component the operator answered, disabled and
    /// filled in from `values`, with no dependency on a live/pending form
    /// registry entry that may since have been superseded or withdrawn.
    /// `None` for entries written before this field existed, or when the
    /// spec couldn't be recovered at submit time — the frontend falls back
    /// to a plain values list for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
}

/// Metadata shape for a `form_dismissed` transcript entry (written by the form route).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormDismissedMeta {
    pub form_id: String,
}

/// Metadata shape for a `form_withdrawn` transcript entry. Carries the
/// withdrawn form's own `form_id` for symmetry with `form_answer`/
/// `form_dismissed` — nothing currently looks it up (there is deliberately no
/// late-answer path for a superseded form), but the entry's `content` is
/// what actually needs to stand alone, not this map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormWithdrawnMeta {
    pub form_id: String,
}

/// One question extracted from an async form spec — enough to pair a label
/// with a submitted answer, or to name a withdrawn question, without needing
/// the (possibly already-cleared) `PendingForm` record it came from. See
/// [`summarize_form_spec`].
struct FormQuestionSummary {
    id: String,
    label: String,
    /// `(option id, option label)` pairs for checkbox/radio fields — empty
    /// for text/textarea/file fields and for any field whose spec omitted
    /// `options`.
    options: Vec<(String, String)>,
}

/// A form's title plus its questions, extracted from the wrapper JSON stored
/// on `PendingForm.spec` (`{"form_id", "spec": {title, intro, fields},
/// "mode"}` — see [`persist_posted_form`]'s `fspec`). The single extraction
/// both [`form_answer_content`] and [`form_withdrawn_content`] render from,
/// so the two transcript lines describe the same form the same way.
struct FormSummary {
    title: Option<String>,
    questions: Vec<FormQuestionSummary>,
}

/// Tolerant of missing or malformed spec JSON — degrades to an empty title
/// and question list rather than erroring, since both render functions built
/// on this run on write paths (answer submission, form supersede) that must
/// never fail because a spec was absent or shaped unexpectedly.
fn summarize_form_spec(pending_spec: &Value) -> FormSummary {
    let inner = pending_spec.get("spec").unwrap_or(pending_spec);
    let title = inner
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let questions = inner
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().filter_map(summarize_field).collect())
        .unwrap_or_default();
    FormSummary { title, questions }
}

fn summarize_field(field: &Value) -> Option<FormQuestionSummary> {
    let id = field.get("id")?.as_str()?.to_string();
    let label = field
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    let options = field
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| opts.iter().filter_map(summarize_option).collect())
        .unwrap_or_default();
    Some(FormQuestionSummary { id, label, options })
}

fn summarize_option(option: &Value) -> Option<(String, String)> {
    let id = option.get("id")?.as_str()?.to_string();
    let label = option
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    Some((id, label))
}

/// Renders one submitted answer value back to text: a selected option id
/// (a bare string for radio, an array of them for checkbox) maps to its
/// label when `options` names it, falling back to the raw id/value when it
/// doesn't (text/textarea fields, or an id the spec no longer lists).
fn render_answer_value(value: &Value, options: &[(String, String)]) -> String {
    match value {
        Value::String(s) => option_label(s, options),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => option_label(s, options),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        Value::Null => "(no answer)".to_string(),
        other => other.to_string(),
    }
}

fn option_label(id: &str, options: &[(String, String)]) -> String {
    options
        .iter()
        .find(|(opt_id, _)| opt_id == id)
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| id.to_string())
}

/// Self-contained content for a `form_answer` transcript entry: the form's
/// title (when the spec carries one) plus each question's label paired with
/// its submitted answer. Reading this string alone — no pending-form record,
/// no client-side join against a `form_request` entry, no refetch — tells a
/// transcript reader what was asked and what was answered.
///
/// `pending_spec` is the wrapper JSON off `PendingForm.spec`; callers must
/// read it out before clearing the pending-form record (see `ao-server`'s
/// `async_form_answer`, which captures it up front for exactly this reason).
/// Degrades gracefully when the spec carries no readable question list —
/// falls back to raw `field_id: value` pairs from `values` rather than
/// going blank.
pub fn form_answer_content(pending_spec: &Value, values: &HashMap<String, Value>) -> String {
    let summary = summarize_form_spec(pending_spec);
    let mut lines = Vec::new();
    if let Some(title) = &summary.title {
        lines.push(format!("**{title}**"));
    }
    if summary.questions.is_empty() {
        for (field_id, value) in values {
            lines.push(format!("- {field_id}: {}", render_answer_value(value, &[])));
        }
    } else {
        for question in &summary.questions {
            let rendered = values
                .get(&question.id)
                .map(|v| render_answer_value(v, &question.options))
                .unwrap_or_else(|| "(no answer)".to_string());
            lines.push(format!("- {}: {}", question.label, rendered));
        }
    }
    if lines.is_empty() {
        return "Form answered.".to_string();
    }
    lines.join("\n")
}

/// Extracts the answered form's own spec (title/intro/fields, the same flat
/// wire shape [`crate::FormFieldPayload`] serializes to and the frontend's
/// `AsyncFormSpec` type expects) for [`FormAnswerMeta::spec`] — the snapshot
/// that makes a `form_answer` entry self-contained. Same `pending_spec`
/// wrapper and the same inner-object unwrap as [`summarize_form_spec`], so
/// the two extractions never drift out of sync with each other.
///
/// Returns `None` when the wrapper carries no readable `fields` array
/// (missing/malformed spec) rather than snapshotting something the UI can't
/// render — callers then fall back to a values-only summary, same as
/// [`form_answer_content`] does for the plain-text case.
pub fn form_answer_spec_snapshot(pending_spec: &Value) -> Option<Value> {
    let inner = pending_spec.get("spec").unwrap_or(pending_spec);
    if inner.get("fields").and_then(Value::as_array).is_some() {
        Some(inner.clone())
    } else {
        None
    }
}

/// Plain, non-interactive content for the transcript line appended when
/// [`ao_persistence::snapshot::SnapshotStore::set_pending_form`] replaces a
/// still-pending form on the same thread with a newer one — see
/// [`persist_posted_form`]. Reuses [`summarize_form_spec`], the same
/// extraction [`form_answer_content`] renders from, rather than a second
/// content builder.
pub fn form_withdrawn_content(pending_spec: &Value) -> String {
    let summary = summarize_form_spec(pending_spec);
    match summary.title {
        Some(title) => format!(
            "\"{title}\" was withdrawn — a newer question replaced it before it could be answered."
        ),
        None => "A pending question was withdrawn — a newer question replaced it before it \
                  could be answered."
            .to_string(),
    }
}

/// Build the plain, non-interactive `form_withdrawn` transcript entry for
/// `replaced` — see [`form_withdrawn_content`]. Not a card, not answerable,
/// carries no field spec; `content` alone is the whole point of this entry.
pub fn form_withdrawn_entry(agent_id: &str, form_id: &str, spec: &Value) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent {
            agent: agent_id.to_string(),
        },
        content: form_withdrawn_content(spec),
        event_type: FORM_WITHDRAWN.to_string(),
        metadata: Some(to_meta_map(&FormWithdrawnMeta {
            form_id: form_id.to_string(),
        })),
        hidden_from_user: false,
    }
}

/// Build a `form_request` transcript entry for the given agent.
///
/// `hidden_from_user` controls whether the entry renders in the message
/// timeline. Both the async path ([`persist_posted_form`] below) and the
/// sync path (see
/// `ao_engine_tools_runner::prompt_bridge::LiveFormBridge::ask_form`) now
/// pass `true` — neither form kind has ever been represented in the UI as
/// its own timeline entry; the visible surface is the `pending_forms`
/// snapshot pointer instead (a composer overlay for sync, a pinned nudge
/// card for async — see `ChatView.tsx`'s `pendingAsyncFormMeta`), and this
/// entry exists purely so [`crate::snapshot`]-adjacent readers can
/// reconstruct pending state, not to be shown. `hidden_from_user: true` also
/// keeps this entry out of `is_pending_form_latest_in_thread`'s "last
/// visible entry" scan (`ao-server`'s `GET /agents`), so it can never be
/// mistaken for the thing that superseded an unrelated form pending on the
/// same thread. (The caller still passes the flag explicitly — kept as a
/// parameter, not hardcoded here, so a future caller with a genuine reason
/// to show one isn't forced to fight this function to do it.)
pub fn form_request_entry(
    agent_id: &str,
    meta: FormRequestMeta,
    hidden_from_user: bool,
) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent {
            agent: agent_id.to_string(),
        },
        content: String::new(),
        event_type: FORM_REQUEST.to_string(),
        metadata: Some(to_meta_map(&meta)),
        hidden_from_user,
    }
}

/// Name of the engine tool that posts operator forms. Matching on it keeps the
/// post-dispatch hook a clean no-op for every other tool.
const ASK_FORM_TOOL: &str = "AskUserQuestionWithForm";

/// Persist an async form-post's `form_request` transcript entry and upsert the
/// pending-form pointer on the snapshot, scoped to `ctx.thread_id` (`None` =
/// the agent's default thread — see [`ao_persistence::snapshot::PendingForm`]).
///
/// Shared write path for the two sites that observe an async
/// `AskUserQuestionWithForm` post: [`wire_posted_async_form`] below (tool
/// calls dispatched outside the interactive loop, e.g. the MCP bridge) and the
/// interactive query loop's inline equivalent, which drains tool results and
/// emits its own `SessionEvent::FormPosted` through a different sink type —
/// factored out here so a future schema change to either store only needs to
/// land once.
///
/// Both steps are best-effort and independently gated on the matching handle
/// being present on `ctx`.
pub async fn persist_posted_form(
    ctx: &crate::context::RunnerContext,
    scope_key: &str,
    form_id: &str,
    spec: &Value,
) {
    // 1. Persist the form_request transcript entry (spec included), routed to
    // the posting thread's OWN transcript file when `ctx.thread_id` names a
    // non-default thread — mirrors the routing `TimelineAdapter` already uses
    // for ordinary turn messages. Without this, the entry always landed in
    // `scope_key`'s default-thread file, which left non-default threads with
    // no `form_request` entry to correlate their pending form against (the
    // "is this form still the latest thing in its thread" read depends on
    // finding it there).
    if let Some(store) = ctx.transcript_store.as_ref() {
        let meta = FormRequestMeta {
            form_id: form_id.to_string(),
            spec: spec.clone(),
            mode: "async".to_string(),
        };
        // `hidden_from_user: true` — mirrors the sync write site
        // (`LiveFormBridge::persist_pending`, `prompt_bridge/mod.rs`): the
        // card renders from the `pending_forms` snapshot pointer (upserted
        // just below), not from this transcript entry, so it must never
        // surface as its own visible message. See `form_request_entry`'s
        // doc comment for the full rationale.
        let entry = form_request_entry(&ctx.agent_id, meta, true);
        let result = match resolve_thread_override_path(ctx).await {
            Some(path) => store.append_at(&path, &entry).await,
            None => store.append(scope_key, &entry).await,
        };
        if let Err(e) = result {
            tracing::warn!(
                agent_id = %ctx.agent_id,
                scope_key = %scope_key,
                error = %e,
                "failed to persist form_request transcript entry for posted async form"
            );
        }
    }

    // 2. Upsert the pending-form pointer + spec on the snapshot, keyed by
    // thread. If this replaces a still-pending form on the same thread, log
    // a plain `form_withdrawn` trace line for it (see
    // `persist_withdrawn_form_entry`) — the drop itself is correct and
    // unchanged, this only makes it visible instead of silent.
    if let Some(store) = ctx.snapshot_store.as_ref() {
        let fspec = serde_json::json!({ "form_id": form_id, "spec": spec, "mode": "async" });
        match store
            .set_pending_form(scope_key, ctx.thread_id.clone(), form_id.to_string(), fspec)
            .await
        {
            Ok(Some(replaced)) => persist_withdrawn_form_entry(ctx, scope_key, &replaced).await,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    agent_id = %ctx.agent_id,
                    scope_key = %scope_key,
                    error = %e,
                    "failed to set pending_form on snapshot for posted async form"
                );
            }
        }
    }
}

/// Appends the `form_withdrawn` line for `replaced` — a still-pending form on
/// the same thread that the `set_pending_form` call in [`persist_posted_form`]
/// just dropped in favor of the form that call is posting.
///
/// Routed exactly like that new form's own `form_request` entry: `replaced`
/// came back from a `set_pending_form` call keyed on `ctx.thread_id`, so its
/// `thread_id` is always that same thread, and the same
/// `resolve_thread_override_path`/`scope_key` fallback applies. Best-effort —
/// a write failure here loses only the trace line, never the (already
/// applied) supersede itself.
async fn persist_withdrawn_form_entry(
    ctx: &crate::context::RunnerContext,
    scope_key: &str,
    replaced: &ao_persistence::snapshot::PendingForm,
) {
    let Some(store) = ctx.transcript_store.as_ref() else {
        return;
    };
    let entry = form_withdrawn_entry(&ctx.agent_id, &replaced.form_id, &replaced.spec);
    let result = match resolve_thread_override_path(ctx).await {
        Some(path) => store.append_at(&path, &entry).await,
        None => store.append(scope_key, &entry).await,
    };
    if let Err(e) = result {
        tracing::warn!(
            agent_id = %ctx.agent_id,
            scope_key = %scope_key,
            form_id = %replaced.form_id,
            error = %e,
            "failed to persist form_withdrawn transcript entry for superseded form"
        );
    }
}

/// Resolves `ctx.thread_id` to its own transcript file path, when it names a
/// non-default thread with a thread store wired on `ctx`. Returns `None`
/// whenever `ctx` is missing either `thread_id` or `thread_store`
/// (project-scoped runs, tests, and any other caller that hasn't wired
/// thread awareness) — those fall back to the pre-existing scope-keyed
/// append. Delegates the actual resolution to
/// [`ao_persistence::thread_store::ThreadStore::resolve_transcript_path_override`],
/// the single canonical implementation shared with `ao-server`'s
/// answered-form write path.
async fn resolve_thread_override_path(ctx: &crate::context::RunnerContext) -> Option<PathBuf> {
    let store = ctx.thread_store.as_ref()?;
    store
        .resolve_transcript_path_override(ctx.thread_id.as_deref())
        .await
}

/// Post-dispatch hook for an async `AskUserQuestionWithForm` result.
///
/// When `tool_name` is the form tool and `output` is a structured payload with
/// `posted == true`, this wires the just-posted form into operator-visible state
/// so the UI can render it and hand the composer over to the form:
///   1. appends a `form_request` transcript entry and upserts the pending-form
///      pointer on the snapshot via [`persist_posted_form`] — an O(1) "is a
///      form waiting?" lookup independent of the message window, and the gate
///      the composer uses to swap the text input for the form;
///   2. emits [`UserEvent::FormPosted`] so subscribed clients update live
///      instead of needing a manual refresh.
///
/// Every step is best-effort and gated on the matching handle being present on
/// `ctx`; tools other than the form tool, non-posted results, and callers that
/// don't wire those handles all get a no-op. Returns the posted `form_id` when a
/// form was wired, `None` otherwise.
///
/// The interactive agent loop performs the equivalent wiring inline as it drains
/// tool results; this helper exists so tool calls dispatched outside that loop
/// (e.g. the MCP bridge) reach the same end state.
///
/// For project-scoped sessions (`ctx.project_id` is `Some`), transcript entries
/// and the pending-form snapshot pointer land in the project scope
/// (`project_{id}`) so they are visible in the project chat and invisible in the
/// bound agent's personal profile view.
pub async fn wire_posted_async_form(
    ctx: &crate::context::RunnerContext,
    tool_name: &str,
    output: &crate::output::ToolOutput,
) -> Option<String> {
    if tool_name != ASK_FORM_TOOL {
        return None;
    }
    let value = match output {
        crate::output::ToolOutput::Structured(v) => v,
        _ => return None,
    };
    if value.get("posted").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let form_id = value.get("form_id").and_then(Value::as_str)?.to_string();
    let spec = value.get("spec").cloned().unwrap_or(Value::Null);

    // Project-scoped runs write form state under the project key so the
    // project transcript and snapshot carry it — not the agent's personal ones.
    let scope_key: String = ctx
        .project_id
        .as_deref()
        .map(|pid| format!("project_{}", pid))
        .unwrap_or_else(|| ctx.agent_id.clone());

    persist_posted_form(ctx, &scope_key, &form_id, &spec).await;

    // Notify subscribers so the operator UI surfaces the form live, spec
    // included so a connected client can render the card straight from this
    // event instead of waiting on a transcript refetch.
    let _ = ctx
        .event_sink
        .emit(crate::context::UserEvent::FormPosted {
            form_id: form_id.clone(),
            spec: parse_form_spec_payload(&form_id, &spec),
        })
        .await;

    Some(form_id)
}

/// Parses the async form tool's own `spec` JSON (see [`wire_posted_async_form`]'s
/// `value.get("spec")`, and the interactive query loop's inline equivalent in
/// `ao-engine-tools-runner`) into the typed payload carried by
/// [`crate::context::UserEvent::FormPosted`].
///
/// Degrades to an empty-fields spec (title cleared) rather than failing
/// outright on a parse error — this only ever runs on JSON this same crate's
/// `ask_user_question_form` tool just produced, so a mismatch here means a
/// shape drift bug upstream, not bad external input; the event should still
/// fire live rather than leave the client waiting.
pub fn parse_form_spec_payload(form_id: &str, spec: &Value) -> crate::context::FormSpecPayload {
    serde_json::from_value(spec.clone()).unwrap_or_else(|e| {
        tracing::warn!(
            form_id,
            error = %e,
            "form_posted spec failed to parse into typed payload; emitting empty spec"
        );
        crate::context::FormSpecPayload {
            form_id: form_id.to_string(),
            title: String::new(),
            intro: None,
            fields: Vec::new(),
        }
    })
}

/// Serialize a metadata struct into the `HashMap<String, Value>` shape the
/// transcript store expects. Panics only if `T` serializes to a non-object,
/// which cannot happen for any of the `*Meta` structs in this module.
pub(crate) fn to_meta_map<T: Serialize>(meta: &T) -> HashMap<String, Value> {
    match serde_json::to_value(meta) {
        Ok(Value::Object(m)) => m.into_iter().collect(),
        _ => HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{EventSink, RunnerContext, UserEvent};
    use crate::output::ToolOutput;
    use ao_persistence::thread_store::ThreadStore;
    use ao_persistence::paths::DataRoot;
    use ao_persistence::snapshot::SnapshotStore;
    use ao_persistence::transcript::TranscriptStore;
    use ao_protocol::error::AoError;
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn make_store(dir: &TempDir) -> TranscriptStore {
        TranscriptStore::new(DataRoot::new(dir.path()))
    }

    async fn make_snapshot_store(dir: &TempDir) -> SnapshotStore {
        let root = DataRoot::new(dir.path());
        root.ensure_directories().await.unwrap();
        SnapshotStore::load(root).await.unwrap()
    }

    /// Records every emitted [`UserEvent`] so tests can assert on the live
    /// notification side of `wire_posted_async_form`.
    #[derive(Default)]
    struct CapturingSink {
        events: Mutex<Vec<UserEvent>>,
    }

    #[async_trait]
    impl EventSink for CapturingSink {
        async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    fn posted_output(form_id: &str) -> ToolOutput {
        ToolOutput::Structured(json!({
            "posted": true,
            "form_id": form_id,
            "spec": {
                "form_id": form_id,
                "title": "Rate this",
                "intro": null,
                "fields": [],
            },
        }))
    }

    #[tokio::test]
    async fn wire_posted_async_form_persists_sets_pending_and_emits() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        let out = posted_output("form-xyz");
        let returned = wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out).await;
        assert_eq!(returned.as_deref(), Some("form-xyz"));

        // 1. A form_request transcript entry is persisted, spec included.
        let entries = transcripts.read_all("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, FORM_REQUEST);
        let meta = entries[0].metadata.as_ref().unwrap();
        assert_eq!(meta["form_id"], json!("form-xyz"));
        assert_eq!(meta["mode"], json!("async"));
        assert_eq!(meta["spec"]["title"], json!("Rate this"));

        // 2. The agent snapshot carries the pending-form pointer + spec, on
        // the default thread (ctx carries no thread_id here).
        let snap = snapshots.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1);
        assert_eq!(agent.pending_forms[0].form_id, "form-xyz");
        assert_eq!(agent.pending_forms[0].thread_id, None);

        // 3. A live FormPosted event is emitted, spec included — non-null and
        // matching the async spec the tool produced — so a live client can
        // render the card straight from this event.
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            UserEvent::FormPosted { form_id, spec } => {
                assert_eq!(form_id, "form-xyz");
                assert_eq!(spec.form_id, "form-xyz");
                assert_eq!(spec.title, "Rate this");
                assert!(spec.fields.is_empty());
            }
            other => panic!("expected FormPosted, got {other:?}"),
        }
    }

    /// A form posted on a non-default thread must land its `form_request`
    /// entry in THAT thread's own transcript file, not the agent's
    /// default-thread file — otherwise a later "is this form still the last
    /// thing in its thread" read (keyed off `thread.transcript_path`) never
    /// finds it.
    #[tokio::test]
    async fn wire_posted_async_form_routes_to_non_default_thread_file() {
        let dir = TempDir::new().unwrap();
        let root = ao_persistence::paths::DataRoot::new(dir.path());
        root.ensure_directories().await.unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let threads = Arc::new(ThreadStore::load(root).await.unwrap());
        let sink = Arc::new(CapturingSink::default());

        let fresh = threads.build_fresh_thread("agent-1", Some("Spike".to_string()));
        let fresh = threads.create(fresh).await.unwrap();

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_thread_store(threads.clone())
            .with_thread(fresh.id.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        let out = posted_output("form-thread");
        let returned = wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out).await;
        assert_eq!(returned.as_deref(), Some("form-thread"));

        // The thread's own file carries the entry...
        let thread_path = PathBuf::from(&fresh.transcript_path);
        let thread_entries = transcripts.read_all_at(&thread_path).await.unwrap();
        assert_eq!(thread_entries.len(), 1);
        assert_eq!(thread_entries[0].event_type, FORM_REQUEST);
        assert_eq!(
            thread_entries[0].metadata.as_ref().unwrap()["form_id"],
            json!("form-thread")
        );

        // ...and the agent's default-thread file stays empty.
        assert!(transcripts.read_all("agent-1").await.unwrap().is_empty());

        // The pending-form pointer is still upserted correctly by thread id.
        let snap = snapshots.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1);
        assert_eq!(agent.pending_forms[0].thread_id.as_deref(), Some(fresh.id.as_str()));
    }

    #[tokio::test]
    async fn wire_posted_async_form_ignores_non_form_tool() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        // Same posted-shaped payload, but a different tool name → no-op.
        let out = posted_output("form-xyz");
        let returned = wire_posted_async_form(&ctx, "Read", &out).await;
        assert!(returned.is_none());

        assert!(transcripts.read_all("agent-1").await.unwrap().is_empty());
        let snap = snapshots.get().await;
        assert!(snap
            .agents
            .get("agent-1")
            .map(|a| a.pending_forms.is_empty())
            .unwrap_or(true));
        assert!(sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wire_posted_async_form_ignores_non_posted_output() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        // A sync answer payload (no `posted` flag) must not be treated as a post.
        let sync_answer =
            ToolOutput::Structured(json!({ "form_id": "form-xyz", "answers": {} }));
        assert!(wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &sync_answer)
            .await
            .is_none());

        // A plain text output is likewise ignored.
        let text = ToolOutput::Text("ok".to_string());
        assert!(wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &text)
            .await
            .is_none());

        assert!(transcripts.read_all("agent-1").await.unwrap().is_empty());
        assert!(sink.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn form_request_round_trips_event_type_and_metadata() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);

        let meta = FormRequestMeta {
            form_id: "test-form-id".to_string(),
            spec: json!({ "title": "Rate this", "fields": [] }),
            mode: "async".to_string(),
        };
        let entry = form_request_entry("agent-1", meta, false);
        store.append("agent-1", &entry).await.unwrap();

        let entries = store.read_all("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        let read = &entries[0];
        assert_eq!(read.event_type, FORM_REQUEST);
        let m = read.metadata.as_ref().unwrap();
        assert_eq!(m["form_id"], json!("test-form-id"));
        assert_eq!(m["mode"], json!("async"));
        assert_eq!(m["spec"]["title"], json!("Rate this"));
        assert!(!read.hidden_from_user);
    }

    #[test]
    fn form_answer_meta_serializes_correctly() {
        let meta = FormAnswerMeta {
            form_id: "f1".to_string(),
            values: [("q1".to_string(), json!("yes"))].into_iter().collect(),
            spec: None,
        };
        let map = to_meta_map(&meta);
        assert_eq!(map["form_id"], json!("f1"));
        assert_eq!(map["values"]["q1"], json!("yes"));
        assert!(!map.contains_key("spec"), "spec must be omitted, not written as null, when absent");
    }

    #[test]
    fn form_answer_spec_snapshot_extracts_inner_spec() {
        let pending_spec = json!({
            "form_id": "form-abc",
            "spec": {
                "form_id": "form-abc",
                "title": "Quick check",
                "intro": null,
                "fields": [{ "id": "q1", "kind": "text", "label": "All good?", "required": false }],
            },
            "mode": "async",
        });
        let snapshot = form_answer_spec_snapshot(&pending_spec).expect("fields present, must snapshot");
        assert_eq!(snapshot["title"], json!("Quick check"));
        assert_eq!(snapshot["fields"][0]["id"], json!("q1"));
    }

    #[test]
    fn form_answer_spec_snapshot_none_when_fields_missing() {
        assert!(form_answer_spec_snapshot(&json!({})).is_none());
        assert!(form_answer_spec_snapshot(&json!({ "spec": { "title": "No fields" } })).is_none());
    }

    #[test]
    fn form_dismissed_meta_serializes_correctly() {
        let meta = FormDismissedMeta { form_id: "f2".to_string() };
        let map = to_meta_map(&meta);
        assert_eq!(map["form_id"], json!("f2"));
    }

    #[test]
    fn event_type_constants() {
        assert_eq!(FORM_REQUEST, "form_request");
        assert_eq!(FORM_ANSWER, "form_answer");
        assert_eq!(FORM_DISMISSED, "form_dismissed");
    }

    /// Project-scoped run: transcript and snapshot must land under `project_{pid}`,
    /// not under the agent's personal key. The agent's personal transcript must
    /// remain empty.
    #[tokio::test]
    async fn wire_posted_async_form_project_scope_isolates_from_agent_transcript() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_project("proj-42".to_string())
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        let out = posted_output("form-project");
        let returned = wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out).await;
        assert_eq!(returned.as_deref(), Some("form-project"));

        // Project transcript carries the form_request entry.
        let project_entries = transcripts.read_all("project_proj-42").await.unwrap();
        assert_eq!(project_entries.len(), 1);
        assert_eq!(project_entries[0].event_type, FORM_REQUEST);
        let meta = project_entries[0].metadata.as_ref().unwrap();
        assert_eq!(meta["form_id"], json!("form-project"));
        assert_eq!(meta["mode"], json!("async"));

        // Agent's personal transcript must be empty — form state lives in project scope.
        let agent_entries = transcripts.read_all("agent-1").await.unwrap();
        assert!(agent_entries.is_empty(), "agent personal transcript must not receive form_request");

        // Pending-form pointer is on the project snapshot key, not the agent key.
        let snap = snapshots.get().await;
        let project_snap = snap.agents.get("project_proj-42").unwrap();
        assert_eq!(project_snap.pending_forms.len(), 1);
        assert_eq!(project_snap.pending_forms[0].form_id, "form-project");
        assert!(
            snap.agents.get("agent-1").map(|a| a.pending_forms.is_empty()).unwrap_or(true),
            "agent personal snapshot must not carry pending_forms"
        );

        // FormPosted event is still emitted (event routing is handled by the event_sink
        // which is already pointed at the project channel by the MCP route setup).
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], UserEvent::FormPosted { form_id, .. } if form_id == "form-project"));
    }

    /// Non-project run (agent_id scoped): transcript and snapshot must still land
    /// under the agent's own key — regression guard.
    #[tokio::test]
    async fn wire_posted_async_form_non_project_scope_unchanged() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-99", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        let out = posted_output("form-agent");
        let returned = wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out).await;
        assert_eq!(returned.as_deref(), Some("form-agent"));

        let agent_entries = transcripts.read_all("agent-99").await.unwrap();
        assert_eq!(agent_entries.len(), 1);
        assert_eq!(agent_entries[0].event_type, FORM_REQUEST);

        let snap = snapshots.get().await;
        assert_eq!(
            snap.agents
                .get("agent-99")
                .and_then(|a| a.pending_forms.first())
                .map(|f| f.form_id.as_str()),
            Some("form-agent")
        );
        assert!(snap.agents.get("project_proj-42").is_none());
    }

    fn spec_with_fields(title: &str, fields: Value) -> Value {
        json!({ "form_id": "f", "spec": { "title": title, "intro": null, "fields": fields }, "mode": "async" })
    }

    #[test]
    fn form_answer_content_renders_title_and_question_answer_pairs() {
        let spec = spec_with_fields(
            "Deploy checklist",
            json!([
                { "id": "q1", "kind": "text", "label": "Any concerns?", "required": false },
            ]),
        );
        let values = [("q1".to_string(), json!("Nope, ship it"))].into_iter().collect();

        let content = form_answer_content(&spec, &values);

        assert!(content.contains("Deploy checklist"), "must include the form title: {content}");
        assert!(content.contains("Any concerns?"), "must include the question label: {content}");
        assert!(content.contains("Nope, ship it"), "must include the answer text: {content}");
    }

    /// A checkbox/radio answer arrives as a raw option id — the content must
    /// render the human label the spec gave that option, not the bare id.
    #[test]
    fn form_answer_content_maps_selection_ids_to_option_labels() {
        let spec = spec_with_fields(
            "Pick one",
            json!([
                {
                    "id": "q1",
                    "kind": "radio",
                    "label": "Which environment?",
                    "required": true,
                    "options": [
                        { "id": "opt-prod", "label": "Production" },
                        { "id": "opt-stg", "label": "Staging" },
                    ],
                },
            ]),
        );
        let values = [("q1".to_string(), json!("opt-prod"))].into_iter().collect();

        let content = form_answer_content(&spec, &values);

        assert!(content.contains("Which environment?"));
        assert!(content.contains("Production"), "must render the option's label, not its id: {content}");
        assert!(!content.contains("opt-prod"), "must not leak the raw option id: {content}");
    }

    /// No readable spec (empty/malformed) must never produce blank content —
    /// falls back to the raw field_id: value pairs from `values`.
    #[test]
    fn form_answer_content_degrades_gracefully_without_spec() {
        let values = [("q1".to_string(), json!("yes"))].into_iter().collect();

        let content = form_answer_content(&json!({}), &values);

        assert!(!content.is_empty(), "must never be blank even with no spec");
        assert!(content.contains("q1"));
        assert!(content.contains("yes"));
    }

    #[test]
    fn form_withdrawn_content_includes_the_withdrawn_question_title() {
        let spec = spec_with_fields("Ship it or not?", json!([]));

        let content = form_withdrawn_content(&spec);

        assert!(content.contains("Ship it or not?"), "must name the withdrawn question: {content}");
        assert!(content.contains("withdrawn"));
    }

    #[test]
    fn form_withdrawn_content_degrades_gracefully_without_title() {
        let content = form_withdrawn_content(&json!({}));

        assert!(!content.is_empty());
        assert!(content.contains("withdrawn"));
    }

    /// End-to-end coverage of change B: posting form B into a thread that
    /// already has form A pending must (a) still drop A's record — unchanged
    /// behavior — and (b) append exactly one `form_withdrawn` entry naming
    /// A's question. Posting into a thread with no prior pending form (form
    /// A's own post, here) must append none.
    #[tokio::test]
    async fn wire_posted_async_form_appends_withdrawn_line_when_replacing_pending_form() {
        let dir = TempDir::new().unwrap();
        let transcripts = Arc::new(make_store(&dir));
        let snapshots = Arc::new(make_snapshot_store(&dir).await);
        let sink = Arc::new(CapturingSink::default());

        let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
            .with_transcript_store(transcripts.clone())
            .with_snapshot_store(snapshots.clone())
            .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>);

        let form_a = ToolOutput::Structured(json!({
            "posted": true,
            "form_id": "form-a",
            "spec": { "form_id": "form-a", "title": "Question A", "intro": null, "fields": [] },
        }));
        wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &form_a).await;

        // Form A's own post replaces nothing — no withdrawn line yet.
        let entries_after_a = transcripts.read_all("agent-1").await.unwrap();
        assert!(
            entries_after_a.iter().all(|e| e.event_type != FORM_WITHDRAWN),
            "first post on a thread must not produce a withdrawn line"
        );

        let form_b = ToolOutput::Structured(json!({
            "posted": true,
            "form_id": "form-b",
            "spec": { "form_id": "form-b", "title": "Question B", "intro": null, "fields": [] },
        }));
        wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &form_b).await;

        // (a) A's record is gone; only B is pending.
        let snap = snapshots.get().await;
        let pending = &snap.agents.get("agent-1").unwrap().pending_forms;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].form_id, "form-b");

        // (b) Exactly one withdrawn line, naming A's question.
        let entries = transcripts.read_all("agent-1").await.unwrap();
        let withdrawn: Vec<_> = entries.iter().filter(|e| e.event_type == FORM_WITHDRAWN).collect();
        assert_eq!(withdrawn.len(), 1, "exactly one withdrawn line for the one replaced form");
        assert!(
            withdrawn[0].content.contains("Question A"),
            "withdrawn line must name the dropped question: {}",
            withdrawn[0].content
        );
        assert!(!withdrawn[0].content.is_empty());
    }
}
