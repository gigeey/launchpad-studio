mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{
    AskQuestionError, EngineTool, FormAction, FormAnswer, FormField, FormFieldKind,
    FormFieldPayload, FormOption, FormRequest, FormResponse, LoadPolicy, RunnerContext, ToolOutput,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

pub struct AskUserQuestionWithForm;

#[async_trait]
impl EngineTool for AskUserQuestionWithForm {
    fn name(&self) -> &str {
        "AskUserQuestionWithForm"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn mutates_filesystem(&self) -> bool {
        false
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // A second post on an already-occupied slot used to be rejected
        // outright. The owner has since locked a newest-wins invariant
        // instead: a newcomer form now always proceeds, and the slot
        // handover (plus the visible `form_withdrawn` trace it leaves for
        // whatever it displaced) happens further down this call's write
        // path — [`ao_persistence::snapshot::SnapshotStore::set_pending_form`]
        // for the sync branch (via `LiveFormBridge::persist_pending`) and
        // [`ao_engine_tools_core::form_events::persist_posted_form`] for the
        // async branch, called by the runner once this returns `posted: true`.

        let title = match input.get("title").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => {
                if t.len() > 200 {
                    return Ok(ToolOutput::error("'title' exceeds 200-character limit", true));
                }
                t.to_string()
            }
            _ => return Ok(ToolOutput::error("title must be a non-empty string", true)),
        };

        let intro = match input.get("intro").and_then(|v| v.as_str()) {
            Some(s) => {
                if s.len() > 1000 {
                    return Ok(ToolOutput::error("'intro' exceeds 1000-character limit", true));
                }
                Some(s.to_string())
            }
            None => None,
        };

        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("sync");

        if mode != "sync" && mode != "async" {
            return Ok(ToolOutput::error(
                "mode must be \"sync\" or \"async\"",
                true,
            ));
        }

        let questions = match input.get("questions").and_then(|v| v.as_array()) {
            Some(q) => q,
            None => return Ok(ToolOutput::error("questions must be an array", true)),
        };

        if questions.is_empty() || questions.len() > 8 {
            return Ok(ToolOutput::error(
                "questions must have between 1 and 8 items",
                true,
            ));
        }

        let mut fields = Vec::with_capacity(questions.len());
        for q in questions {
            match parse_field(q) {
                Ok(f) => fields.push(f),
                Err(msg) => return Ok(ToolOutput::error(&msg, true)),
            }
        }

        let mut seen_ids: HashSet<&str> = HashSet::new();
        for field in &fields {
            if !seen_ids.insert(field.id.as_str()) {
                return Ok(ToolOutput::error(
                    &format!(
                        "duplicate field id '{}': each field id must be unique",
                        field.id
                    ),
                    true,
                ));
            }
        }

        if mode == "async" {
            let form_id = Uuid::new_v4().to_string();
            // Serialize fields in the flat wire shape the UI consumes (string `kind`
            // discriminant + hoisted extras), matching the synchronous FormRequest
            // path. Emitting the raw tagged-enum shape here renders labels but drops
            // every input control, since the renderer keys on a string `kind`.
            let payload_fields: Vec<FormFieldPayload> =
                fields.iter().map(FormFieldPayload::from).collect();
            let spec = json!({
                "form_id": form_id,
                "title": title,
                "intro": intro,
                "fields": payload_fields,
            });
            return Ok(ToolOutput::Structured(json!({
                "posted": true,
                "form_id": form_id,
                "spec": spec,
            })));
        }

        let request = FormRequest {
            id: String::new(),
            agent_id: ctx.agent_id.clone(),
            session_id: ctx.session_id.clone(),
            title,
            intro,
            fields,
        };

        resolve_sync_form(request, ctx, sync_form_timeout()).await
    }
}

/// Default wall-clock deadline for a synchronous form call (`mode: "sync"`)
/// awaiting an operator's answer. Overridable via the
/// `AO_SYNC_FORM_TIMEOUT_SECS` env var.
///
/// Deliberately NOT bounded below the process supervisor's overall run
/// budget (`AgentProfile::timeout_seconds` — default 300s, hard-capped at
/// 3600s by `ao_protocol::agent::MAX_TIMEOUT_SECONDS`): a human filling out a
/// form can reasonably take longer than a typical agent turn budget. That's
/// safe anyway, because `ctx.form_bridge.ask_form` (`LiveFormBridge::ask_form`
/// in production) holds a suspension guard for the call's *entire* lifetime —
/// from the instant its oneshot is registered until it returns by any exit
/// path — and the supervisor's overall-timeout loop
/// (`ao_process::default_supervisor`, Branch 2) excludes every slice where
/// that counter is nonzero from the budget it enforces. This deadline only
/// ever elapses while that same `ask_form` call is still suspended, because
/// it races the call directly (see [`resolve_sync_form`]) — so no matter how
/// long it lets the wait run, that time stays invisible to the process-level
/// timeout.
const DEFAULT_SYNC_FORM_TIMEOUT_SECS: u64 = 1800;

/// Public so `ao-server`'s startup path can compare it against the MCP
/// session TTL — see [`check_sync_form_timeout_vs_session_ttl`].
pub fn sync_form_timeout() -> Duration {
    std::env::var("AO_SYNC_FORM_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_SYNC_FORM_TIMEOUT_SECS))
}

/// Detects the cross-config trap described on [`DEFAULT_SYNC_FORM_TIMEOUT_SECS`]:
/// this tool's own doc comment tells operators raising `AO_SYNC_FORM_TIMEOUT_SECS`
/// past the default is safe and intended, but that's only true while it stays
/// below the MCP session TTL (`LAUNCHPAD_MCP_SESSION_TTL_SECS`, swept by
/// `ao-server`'s background session sweep). Once the sync-form deadline reaches
/// or exceeds the session TTL, `ctx.cancel` fires from session expiry first on
/// every call — the form is reclassified from "still waiting" to "cancelled"
/// and never reaches its own timeout branch (see [`resolve_sync_form`]'s
/// `ctx.cancel` arm). Returns an operator-facing warning message when
/// misconfigured this way, `None` when `sync_form_timeout` is safely below
/// `session_ttl` (the default: 1800s < 3600s).
///
/// Deliberately returns a message rather than logging directly, so the
/// startup call site controls how/where it's emitted and this stays unit
/// testable without a tracing subscriber.
pub fn check_sync_form_timeout_vs_session_ttl(
    sync_form_timeout: Duration,
    session_ttl: Duration,
) -> Option<String> {
    if sync_form_timeout < session_ttl {
        return None;
    }
    Some(format!(
        "sync-form misconfiguration: AO_SYNC_FORM_TIMEOUT_SECS={} >= LAUNCHPAD_MCP_SESSION_TTL_SECS={} \
         — sync forms will be cancelled by session expiry before they can time out; raise \
         LAUNCHPAD_MCP_SESSION_TTL_SECS above AO_SYNC_FORM_TIMEOUT_SECS",
        sync_form_timeout.as_secs(),
        session_ttl.as_secs(),
    ))
}

/// Why a synchronous form call stopped waiting. Kept separate from
/// [`AskQuestionError`] — that enum describes why `ask_form` itself resolved
/// (the bridge's own concern); `TimedOut` describes why the *caller* gave up
/// on it, which is this tool's concern alone and never something the bridge
/// itself produces.
enum SyncFormOutcome {
    Answered(Result<FormResponse, AskQuestionError>),
    TimedOut,
}

/// Race `ctx.form_bridge.ask_form(request)` against cancellation and this
/// call's own `timeout`, and map whichever wins to a [`ToolOutput`].
///
/// Whichever branch does NOT win causes the other two futures to be dropped
/// without ever being polled to completion. For `ask_form` specifically,
/// that's the documented "future dropped by an outer `tokio::select!`" exit
/// path its `PendingFormClearGuard` and `FormSuspensionGuard` locals (in
/// `ao-engine-tools-runner`'s `prompt_bridge` module) are built to survive —
/// both guards run their cleanup from `Drop`, so the pending-form snapshot
/// entry and the `form_suspended` signal are resolved/cleared exactly the
/// same way whether the form was answered, cancelled, or — new here —
/// timed out. No half-state is reachable from any of the three branches.
async fn resolve_sync_form(
    request: FormRequest,
    ctx: &RunnerContext,
    timeout: Duration,
) -> Result<ToolOutput, AoError> {
    let outcome = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => SyncFormOutcome::Answered(Err(AskQuestionError::Cancelled)),
        _ = tokio::time::sleep(timeout) => SyncFormOutcome::TimedOut,
        r = ctx.form_bridge.ask_form(request) => SyncFormOutcome::Answered(r),
    };

    Ok(match outcome {
        SyncFormOutcome::Answered(Ok(response)) => {
            ToolOutput::Structured(response_to_json(response))
        }
        SyncFormOutcome::Answered(Err(AskQuestionError::Cancelled)) => {
            ToolOutput::error("cancelled", false)
        }
        SyncFormOutcome::Answered(Err(AskQuestionError::NoOperator)) => {
            ToolOutput::error("no operator available to present form", true)
        }
        SyncFormOutcome::TimedOut => form_timed_out_output(timeout),
    })
}

/// Structured (never [`ToolOutput::Error`]) payload for a form whose
/// deadline elapsed with no operator response. Deliberately shaped so the
/// model can tell it apart from every other outcome at a glance: it carries
/// neither `"answers"` (an answered form) nor `"action"` (a
/// Cancel/Regenerate/Other button click) nor the generic
/// `message`/`recoverable` shape of `ToolOutput::Error` — which reads as
/// "the tool call itself failed, retry is plausible", not what happened
/// here. `prompt::DESCRIPTION` spells out the required reaction: abort,
/// don't guess.
fn form_timed_out_output(timeout: Duration) -> ToolOutput {
    ToolOutput::Structured(json!({
        "outcome": "form_timed_out",
        "timeout_secs": timeout.as_secs(),
    }))
}

/// Human-readable string for each [`FormAction`] as surfaced to the agent.
fn action_str(action: FormAction) -> &'static str {
    match action {
        FormAction::Cancel => "cancel",
        FormAction::Regenerate => "regenerate",
        FormAction::Other => "other",
    }
}

fn is_valid_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_field(q: &Value) -> Result<FormField, String> {
    let id = q
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("field missing 'id'")?;
    if id.len() > 64 {
        return Err(format!(
            "field id '{}…' exceeds 64-character limit",
            &id[..id.len().min(20)]
        ));
    }
    if !is_valid_id(id) {
        return Err(format!(
            "field id '{id}' must contain only letters, digits, underscores, and hyphens"
        ));
    }
    let id = id.to_string();

    let label = q
        .get("label")
        .and_then(|v| v.as_str())
        .ok_or("field missing 'label'")?;
    if label.len() > 300 {
        return Err(format!("field '{id}': 'label' exceeds 300-character limit"));
    }
    let label = label.to_string();

    let description = q.get("description").and_then(|v| v.as_str()).map(String::from);
    if let Some(desc) = &description {
        if desc.len() > 500 {
            return Err(format!(
                "field '{id}': 'description' exceeds 500-character limit"
            ));
        }
    }

    let required = q
        .get("required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let type_str = q
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or("field missing 'type'")?;

    let kind = match type_str {
        t @ ("checkbox" | "radio") => {
            if q.get("max_files").and_then(|v| v.as_i64()).is_some() {
                return Err(format!(
                    "field '{id}': 'max_files' is only valid for file fields"
                ));
            }
            if q.get("accept").and_then(|v| v.as_str()).is_some() {
                return Err(format!(
                    "field '{id}': 'accept' is only valid for file fields"
                ));
            }
            let options = parse_options(q, &id)?;
            if t == "checkbox" {
                FormFieldKind::Checkbox { options }
            } else {
                FormFieldKind::Radio { options }
            }
        }
        t @ ("text" | "textarea") => {
            if q.get("options").and_then(|v| v.as_array()).is_some() {
                return Err(format!(
                    "field '{id}': 'options' is only valid for checkbox and radio fields"
                ));
            }
            if q.get("max_files").and_then(|v| v.as_i64()).is_some() {
                return Err(format!(
                    "field '{id}': 'max_files' is only valid for file fields"
                ));
            }
            if q.get("accept").and_then(|v| v.as_str()).is_some() {
                return Err(format!(
                    "field '{id}': 'accept' is only valid for file fields"
                ));
            }
            let placeholder = q.get("placeholder").and_then(|v| v.as_str());
            if let Some(ph) = placeholder {
                if ph.len() > 200 {
                    return Err(format!(
                        "field '{id}': 'placeholder' exceeds 200-character limit"
                    ));
                }
            }
            let placeholder = placeholder.map(String::from);
            if t == "text" {
                FormFieldKind::Text { placeholder }
            } else {
                FormFieldKind::Textarea { placeholder }
            }
        }
        "file" => {
            if q.get("options").and_then(|v| v.as_array()).is_some() {
                return Err(format!(
                    "field '{id}': 'options' is only valid for checkbox and radio fields"
                ));
            }
            let accept = q.get("accept").and_then(|v| v.as_str());
            if let Some(acc) = accept {
                if acc.len() > 200 {
                    return Err(format!(
                        "field '{id}': 'accept' exceeds 200-character limit"
                    ));
                }
            }
            FormFieldKind::File {
                max_files: q
                    .get("max_files")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 10) as u8,
                accept: accept.map(String::from),
            }
        }
        other => return Err(format!("unknown field type '{other}'")),
    };

    Ok(FormField {
        id,
        kind,
        label,
        description,
        required,
    })
}

fn parse_options(q: &Value, field_id: &str) -> Result<Vec<FormOption>, String> {
    let arr = q
        .get("options")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "checkbox/radio field requires an 'options' array".to_string())?;

    if arr.is_empty() || arr.len() > 12 {
        return Err("options must have between 1 and 12 items".to_string());
    }

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut options = Vec::with_capacity(arr.len());

    for o in arr {
        let opt_id = o
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("option missing 'id'")?;
        if opt_id.len() > 64 {
            return Err(format!(
                "field '{field_id}': option id '{}…' exceeds 64-character limit",
                &opt_id[..opt_id.len().min(20)]
            ));
        }
        if !is_valid_id(opt_id) {
            return Err(format!(
                "field '{field_id}': option id '{opt_id}' must contain only letters, digits, underscores, and hyphens"
            ));
        }
        if !seen_ids.insert(opt_id.to_string()) {
            return Err(format!(
                "field '{field_id}': duplicate option id '{opt_id}': each option id must be unique"
            ));
        }
        let opt_id = opt_id.to_string();

        let opt_label = o
            .get("label")
            .and_then(|v| v.as_str())
            .ok_or("option missing 'label'")?;
        if opt_label.len() > 200 {
            return Err(format!(
                "field '{field_id}': option '{opt_id}' 'label' exceeds 200-character limit"
            ));
        }
        let opt_label = opt_label.to_string();

        let description = o.get("description").and_then(|v| v.as_str()).map(String::from);
        if let Some(desc) = &description {
            if desc.len() > 400 {
                return Err(format!(
                    "field '{field_id}': option '{opt_id}' 'description' exceeds 400-character limit"
                ));
            }
        }

        options.push(FormOption {
            id: opt_id,
            label: opt_label,
            description,
        });
    }

    Ok(options)
}

fn response_to_json(response: FormResponse) -> Value {
    // An action click carries no answers — surface the action (and optional
    // note) instead of an empty answers map, so the agent sees clearly that
    // the operator didn't submit and can react rather than treat this like a
    // normal (if empty) answer.
    if let Some(action) = response.action {
        return json!({
            "form_id": response.form_id,
            "action": action_str(action),
            "note": response.note,
        });
    }

    let answers: Map<String, Value> = response
        .answers
        .into_iter()
        .map(|(field_id, answer)| {
            let v = match answer {
                FormAnswer::Text(text) => json!({ "kind": "text", "value": text }),
                FormAnswer::Selections(vals) => json!({ "kind": "selections", "values": vals }),
                FormAnswer::Files(ids) => json!({ "kind": "files", "attachment_ids": ids }),
            };
            (field_id, v)
        })
        .collect();

    json!({
        "form_id": response.form_id,
        "answers": answers,
    })
}
