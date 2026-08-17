//! `POST /prompt-refine` — one-shot model rewrite of an assignment's
//! Instruction text, run against the requesting assignment's *owning
//! agent* (its own provider/model), not a global default. Mode-aware: the
//! caller passes which trigger kind (cron / webhook / poll_connector) the
//! instruction belongs to, since webhook and poll_connector instructions
//! double as `{dot.path}`/`{__raw__}` payload templates (see
//! `ao_protocol::webhook_template::render_prompt_template`) while a cron
//! instruction has no payload to reference at all. No tools, no agent loop
//! — a single [`ProviderClient::complete`] call, same shape as
//! [`ao_engine_tools_runner::reflection::ProviderReflectionProposer::propose`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use ao_engine::AppState;
use ao_engine_tools_runner::provider::{CompletionEvent, CompletionRequest};
use ao_engine_tools_runner::{ContentBlock, Message};

/// Which trigger kind the instruction being refined belongs to. Determines
/// what guidance is appended to [`PROMPT_REFINE_SYSTEM_PROMPT`] — in
/// particular, whether `{dot.path}`/`{__raw__}` placeholders are in play and
/// must be preserved verbatim.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefineTemplateMode {
    Cron,
    Webhook,
    PollConnector,
}

impl RefineTemplateMode {
    fn guidance(self) -> &'static str {
        match self {
            RefineTemplateMode::Cron => "\
This instruction runs on a recurring (or one-shot) schedule with no event \
payload attached. Write it as a plain, self-contained instruction — there \
are no `{dot.path}` placeholders available in this mode, so don't invent \
any.",
            RefineTemplateMode::Webhook => "\
This instruction fires in response to an inbound webhook event and is \
rendered against that event's JSON payload before being handed to the \
agent: `{dot.path}` tokens (e.g. `{pull_request.title}`) are replaced with \
the value at that path in the payload, and the special token `{__raw__}` \
expands to the full payload as JSON. You MUST preserve every placeholder \
token exactly as written — do not rename, remove, add, or otherwise alter \
any `{...}` token.",
            RefineTemplateMode::PollConnector => "\
This instruction fires when a polled connector detects a new result, but \
unlike a webhook there is no per-event JSON payload to template against — \
no `{dot.path}` placeholders are available. Write it as a clear, \
self-contained instruction describing what the agent should do with \
whatever new item the poll turned up.",
        }
    }
}

/// System prompt instructing the model to rewrite an assignment's
/// Instruction text into a clearer version of the same request. Mode-specific
/// guidance (placeholder rules, if any) is appended by the handler below.
const PROMPT_REFINE_SYSTEM_PROMPT: &str = "\
You improve the instruction text for a scheduled or triggered agent \
assignment. Rewrite the instruction the user gives you so it is a clearer, \
more specific, more actionable version of the same request. Everything \
about the wording, structure, and level of detail is yours to improve — but \
do not change what the instruction asks for.

Respond with ONLY the rewritten instruction text. No preamble, no quotes, no \
commentary, no markdown code fences.";

#[derive(Debug, Deserialize)]
pub struct RefinePromptTemplateRequest {
    pub agent_id: String,
    pub prompt_template: String,
    /// Optional for backward compatibility with callers that predate mode
    /// awareness — defaults to [`RefineTemplateMode::Webhook`], matching this
    /// endpoint's original (webhook-only) behavior.
    #[serde(default)]
    pub mode: Option<RefineTemplateMode>,
}

#[derive(Debug, Serialize)]
pub struct RefinePromptTemplateResponse {
    pub refined_template: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// `POST /prompt-refine` — see [`RefinePromptTemplateRequest`].
pub async fn refine_prompt_template(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefinePromptTemplateRequest>,
) -> Response {
    let profile = match state.persistence.agents.get(&req.agent_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("Agent '{}' does not exist", req.agent_id),
            );
        }
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let Some(provider) = ao_engine::build_prompt_refine_provider(&profile) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "This agent has no provider configured — add an API key in Settings.",
        );
    };

    let mode = req.mode.unwrap_or(RefineTemplateMode::Webhook);

    // Awareness only: this appends catalog TEXT to the system prompt so the
    // refiner can reference the agent's real tools/skills/workflows/prefs by
    // name. The completion request below still carries no `tools` field and
    // runs no agent loop.
    let execution_environment =
        ao_engine::system_prompt_composer::refine_context::build_execution_environment(
            &state, &profile,
        )
        .await;

    let request = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: req.prompt_template }],
        }],
        system_prompt: Some(format!(
            "{}\n\n{}\n\n{}",
            PROMPT_REFINE_SYSTEM_PROMPT,
            mode.guidance(),
            execution_environment
        )),
        tools: vec![],
        ..Default::default()
    };

    let mut stream = match provider.complete(request, CancellationToken::new()).await {
        Ok(stream) => stream,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, format!("provider error: {e}")),
    };

    let mut text = String::new();
    loop {
        match stream.recv().await {
            None => break,
            Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
            Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                return error_response(StatusCode::BAD_GATEWAY, format!("provider error: {e}"));
            }
        }
    }

    Json(RefinePromptTemplateResponse { refined_template: text.trim().to_string() }).into_response()
}
