//! `POST /webhooks/{route_name}` — the named-route shared-ingress webhook
//! gateway. Replaces the per-assignment `/assignments/{assignment_id}/trigger`
//! URL as the inbound surface for `Webhook`-triggered assignments; that
//! route (`crate::routes::assignments::trigger_assignment`) stays wired for
//! back-compat.
//!
//! A route name resolves to every assignment whose `Webhook.route_name`
//! matches — usually one, but nothing stops several assignments (even across
//! agents) from sharing an inbound URL. Every matching assignment gets its
//! own delivery-id dedup check and its own fire.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use ao_engine::queue_manager::NotificationDispatcher;
use ao_engine::webhook_dispatch::dispatch_webhook_route;
use ao_engine::AppState;
use ao_engine_tools_provider_config::ChannelSecretStore;
use ao_protocol::assignment::{Assignment, AssignmentTrigger, WebhookDeliverTarget};
use ao_protocol::error::AoError;
use ao_protocol::webhook_filter::{event_type_allowed, WebhookFilter};
use ao_protocol::webhook_template::render_prompt_template;

use crate::error::AppError;
use crate::routes::assignments::user_timezone;
use crate::webhook_gateway::{
    extract_delivery_id, extract_event_type, resolve_route_secret_ref, server_bind_is_loopback,
    verify_request_signature, SignatureError, WEBHOOK_HMAC_SECRET_ROLE, WEBHOOK_SECRET_VAULT_SCOPE,
};

/// `POST /webhooks/{route_name}`.
///
/// `body` is captured as raw [`Bytes`] — the last extractor in the handler
/// signature, per axum's body-consuming-extractor rule — so HMAC signing
/// covers exactly the bytes the sender transmitted, never a JSON
/// parse-then-reserialize round trip that could disagree with what was
/// actually signed.
pub async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    Path(route_name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let route_assignments = state.persistence.assignments.list_webhook_assignments_by_route(&route_name).await;
    if route_assignments.is_empty() {
        return error_response(StatusCode::NOT_FOUND, format!("unknown webhook route: {route_name}"));
    }

    let secret = match resolve_secret(&route_assignments) {
        Ok(secret) => secret,
        Err(resp) => return resp,
    };

    let now_unix = chrono::Utc::now().timestamp();
    if let Err(e) = verify_request_signature(&headers, &body, &secret, server_bind_is_loopback(), now_unix) {
        return signature_error_response(e);
    }

    let delivery_id = extract_delivery_id(&headers);
    let event_type = extract_event_type(&headers);
    let tz = user_timezone(&state).await;
    let dispatcher = Arc::clone(&state.queue_managers) as Arc<dyn NotificationDispatcher>;
    let payload_summary = String::from_utf8_lossy(&body).chars().take(500).collect::<String>();
    // A sender that isn't strict JSON (or sends an empty body) still resolves
    // to *some* Value so filters/template rendering degrade gracefully
    // (every field simply misses) rather than the whole request failing.
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    let tally = dispatch_webhook_route(
        &state.persistence,
        &dispatcher,
        &state.event_bus,
        &route_assignments,
        &route_name,
        event_type.as_deref(),
        &payload,
        &payload_summary,
        delivery_id.as_deref(),
        tz.as_deref(),
    )
    .await;

    (
        StatusCode::OK,
        Json(json!({
            "route": route_name,
            "matched": tally.matched,
            "fired": tally.fired,
            "delivered": tally.delivered,
            "filtered": tally.filtered,
            "deduped": tally.deduped,
        })),
    )
        .into_response()
}

/// Resolves the route's HMAC secret: the first matching assignment's
/// `secret_ref` (see [`resolve_route_secret_ref`]), looked up through
/// [`ChannelSecretStore`]. Fails closed — any missing piece (no
/// `secret_ref`, no store entry, an empty stored value) is the same
/// "route has no secret" rejection `verify_request_signature` would also
/// produce, surfaced here before that call so a store error doesn't read as
/// an invalid-signature response.
fn resolve_secret(route_assignments: &[Assignment]) -> Result<String, Response> {
    let secret_ref = resolve_route_secret_ref(route_assignments)
        .ok_or_else(|| signature_error_response(SignatureError::NoSecretConfigured))?;

    let store = ChannelSecretStore::open()
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("secret store unavailable: {e}")))?;

    match store.get(WEBHOOK_SECRET_VAULT_SCOPE, secret_ref, WEBHOOK_HMAC_SECRET_ROLE) {
        Ok(Some(secret)) if !secret.is_empty() => Ok(secret),
        Ok(_) => Err(signature_error_response(SignatureError::NoSecretConfigured)),
        Err(e) => Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("secret store error: {e}"))),
    }
}

fn signature_error_response(e: SignatureError) -> Response {
    let (status, message) = match e {
        SignatureError::NoSecretConfigured => (StatusCode::FORBIDDEN, "webhook route has no HMAC secret configured"),
        SignatureError::InsecureSentinelRequiresLoopback => (
            StatusCode::FORBIDDEN,
            "INSECURE_NO_AUTH is only allowed when the server is bound to loopback",
        ),
        SignatureError::MissingSignature => (StatusCode::UNAUTHORIZED, "missing webhook signature"),
        SignatureError::MalformedTimestamp => (StatusCode::UNAUTHORIZED, "missing or malformed webhook timestamp"),
        SignatureError::TimestampOutOfWindow => (StatusCode::UNAUTHORIZED, "webhook timestamp outside replay window"),
        SignatureError::InvalidSignature => (StatusCode::UNAUTHORIZED, "invalid webhook signature"),
    };
    error_response(status, message)
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Best-effort startup sweep: logs a warning for every distinct webhook
/// route among existing assignments that would currently fail closed (no
/// resolvable secret, or an `INSECURE_NO_AUTH` secret while the process is
/// bound to a non-loopback host). Never blocks server boot — unlike a static
/// config file, routes here are assignment rows a user can add or fix at
/// runtime through the UI without a restart, so refusing to start over one
/// misconfigured route would take every other route down with it too.
///
/// This is visibility only. [`handle_webhook`] is what actually enforces
/// the requirement on every request via the same [`resolve_secret`] /
/// [`verify_request_signature`] pair this sweep calls — there is no gate
/// upstream of the handler that a direct call could bypass.
pub async fn validate_routes_at_startup(state: &Arc<AppState>) {
    let named_routes = state.persistence.assignments.list_all_named_webhook_routes().await;
    let route_names: BTreeSet<&str> = named_routes
        .iter()
        .filter_map(|a| match &a.trigger {
            AssignmentTrigger::Webhook { route_name: Some(name), .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    if route_names.is_empty() {
        return;
    }

    let bind_is_loopback = server_bind_is_loopback();
    for route_name in route_names {
        let route_assignments = state.persistence.assignments.list_webhook_assignments_by_route(route_name).await;
        let secret = match resolve_secret(&route_assignments) {
            Ok(secret) => secret,
            Err(_) => {
                warn!("webhook route '{route_name}' has no resolvable HMAC secret; every request to it will be rejected until one is configured");
                continue;
            }
        };
        if secret == crate::webhook_gateway::INSECURE_NO_AUTH_SENTINEL && !bind_is_loopback {
            warn!("webhook route '{route_name}' is configured with INSECURE_NO_AUTH but the server is not bound to loopback; every request to it will be rejected");
        }
    }
}

/// Response for `GET /webhooks/{route_name}/secret`.
#[derive(Debug, Serialize)]
pub struct WebhookRouteSecretStatus {
    /// Whether a secret currently resolves for this route — i.e. whether a
    /// real inbound request would pass or fail closed at the HMAC check.
    /// Never the secret value itself: the store is write-only (see
    /// [`set_webhook_route_secret`]), so this is the only way the editor can
    /// tell "configured" apart from "every request will 403" once a secret
    /// has already been saved in an earlier session and can no longer be
    /// redisplayed.
    pub configured: bool,
}

/// `GET /webhooks/{route_name}/secret` — read-only existence check. Reuses
/// [`resolve_secret`], the exact same resolution [`handle_webhook`] uses on
/// a live request, so "configured" here can never disagree with what the
/// gateway itself would do.
pub async fn get_webhook_route_secret_status(
    State(state): State<Arc<AppState>>,
    Path(route_name): Path<String>,
) -> Json<WebhookRouteSecretStatus> {
    let route_assignments = state.persistence.assignments.list_webhook_assignments_by_route(&route_name).await;
    Json(WebhookRouteSecretStatus { configured: resolve_secret(&route_assignments).is_ok() })
}

/// Request body for `PUT /webhooks/{route_name}/secret`.
#[derive(Debug, Deserialize)]
pub struct SetWebhookRouteSecretRequest {
    pub secret: String,
}

/// `PUT /webhooks/{route_name}/secret` — write-only: stores the literal HMAC
/// signing secret for `route_name` under the same `ChannelSecretStore`
/// scope/role the gateway resolves at request time
/// ([`WEBHOOK_SECRET_VAULT_SCOPE`] / [`WEBHOOK_HMAC_SECRET_ROLE`]), keyed by
/// the route name itself. Never echoes the secret back in the response —
/// mirrors the existing per-channel secret endpoints (e.g.
/// `routes::channels::set_discord_channel_secret`). An assignment's
/// `secret_ref` is expected to carry this same `route_name` value so
/// [`resolve_route_secret_ref`] finds it at request time; the editor UI sets
/// both together when it saves the assignment.
pub async fn set_webhook_route_secret(
    Path(route_name): Path<String>,
    Json(body): Json<SetWebhookRouteSecretRequest>,
) -> Result<StatusCode, AppError> {
    let secret = body.secret.trim();
    if secret.is_empty() {
        return Err(AppError(AoError::ValidationError("secret must not be empty".to_string())));
    }

    let store = ChannelSecretStore::open()
        .map_err(|e| AppError(AoError::Internal(format!("secret store unavailable: {e}"))))?;
    store
        .set(WEBHOOK_SECRET_VAULT_SCOPE, &route_name, WEBHOOK_HMAC_SECRET_ROLE, secret)
        .map_err(|e| AppError(AoError::Internal(format!("secret store error: {e}"))))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Request body for `POST /webhook-test` — a stateless dry-run of the
/// filter/template pipeline backing the editor's "Send test webhook" button
/// Takes the route's in-progress `events`/`filters`/`prompt_template`/
/// `deliver` config directly rather than requiring an already-saved route,
/// so a user can try a draft before saving it. Push has nothing to poll, so
/// unlike a poll-side test this makes no live network call: it's pure
/// evaluation of a caller-supplied sample payload against the exact same
/// [`event_type_allowed`] + [`WebhookFilter::matches`] +
/// [`render_prompt_template`] functions `ao_engine::webhook_dispatch` calls
/// for a real inbound POST. No agent is ever spawned and no `github_comment`
/// is ever posted.
#[derive(Debug, Deserialize)]
pub struct TestWebhookRouteRequest {
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub filters: Option<WebhookFilter>,
    #[serde(default)]
    pub prompt_template: Option<String>,
    #[serde(default)]
    pub deliver: WebhookDeliverTarget,
    /// Simulated `X-GitHub-Event`/`X-Event-Type` header value. `None` behaves
    /// like a request that sent neither header.
    #[serde(default)]
    pub event_type: Option<String>,
    /// Sample inbound JSON body to evaluate the route's config against.
    #[serde(default)]
    pub payload: Value,
}

/// Result of a dry-run evaluation. See [`TestWebhookRouteRequest`].
#[derive(Debug, Serialize)]
pub struct TestWebhookRouteResponse {
    /// True if `events`/`filters` would let this sample payload through.
    pub matched: bool,
    /// The `deliver` target a matching payload would be routed to, echoed
    /// back so the UI doesn't need to track it separately.
    pub deliver: WebhookDeliverTarget,
    /// True only when `matched` and `deliver` is `Agent` — i.e. this sample
    /// would actually start an agent run were this a real request.
    pub would_start_agent: bool,
    /// The rendered `prompt_template` — exactly what the agent's instruction
    /// (or the `deliver_only`/`github_comment` payload) would contain.
    /// `null` when no template is set, or the sample was filtered out.
    pub rendered_instruction: Option<String>,
}

/// `POST /webhook-test` — see [`TestWebhookRouteRequest`].
pub async fn test_webhook_route(Json(req): Json<TestWebhookRouteRequest>) -> Json<TestWebhookRouteResponse> {
    let filters_match = req.filters.as_ref().map(|f| f.matches(&req.payload)).unwrap_or(true);
    let matched = event_type_allowed(&req.events, req.event_type.as_deref()) && filters_match;

    let rendered_instruction = if matched {
        req.prompt_template.as_deref().map(|tpl| render_prompt_template(tpl, &req.payload))
    } else {
        None
    };
    let would_start_agent = matched && req.deliver == WebhookDeliverTarget::Agent;

    Json(TestWebhookRouteResponse { matched, deliver: req.deliver, would_start_agent, rendered_instruction })
}
