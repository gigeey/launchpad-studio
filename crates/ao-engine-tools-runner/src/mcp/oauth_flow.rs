//! OAuth 2.0 authorization-code + PKCE flow engine for MCP servers.
//!
//! Covers the standard authorization flow for network-accessible MCP servers
//! that protect their endpoints with OAuth 2.0 bearer tokens:
//!
//! 1. **Discovery** — RFC 9728 protected-resource metadata →
//!    RFC 8414 authorization-server metadata.
//! 2. **Client registration** — a pre-configured `client_id` (with an optional
//!    `client_secret` for confidential clients such as GitHub OAuth Apps) is
//!    used when present; otherwise RFC 7591 Dynamic Client Registration
//!    provisions a public client on the fly.
//! 3. **Authorization** — PKCE S256 + CSRF state, localhost callback HTTP
//!    server; the authorization URL is returned to the caller (never
//!    auto-opened) for delivery to the user or model.
//! 4. **Token exchange** — exchanges the received authorization code for
//!    access + refresh tokens, persists them via the token store.
//! 5. **Proactive refresh** — [`OAuthEngine::current_access_token`] silently
//!    refreshes when the stored token expires within five minutes and a refresh
//!    token is available. Reads on this path always bypass the credential
//!    store's process-lifetime cache (see [`McpTokenStore::get_fresh`]) and
//!    refreshes are single-flighted per server key, so two racing callers can
//!    never present the same (single-use) refresh token to a provider that
//!    rotates on every use — doing so is what triggers reuse detection and
//!    gets the whole grant revoked. A provider reporting `invalid_grant`
//!    surfaces as the distinct, terminal [`OAuthError::GrantRevoked`] rather
//!    than a generic/retryable refresh failure.
//!
//! Known gaps, not yet implemented:
//! - RFC 7009 token revocation
//! - Step-up auth (403 `insufficient_scope` re-authorization)
//! - Enterprise XAA path (RFC 8693 + RFC 7523)

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;
use tracing::{debug, warn};

use ao_engine_tools_provider_config::mcp_servers::McpAuthConfig;
use ao_engine_tools_provider_config::mcp_token_store::{McpTokenRecord, McpTokenStore};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Deadline for the full interactive authorization flow (user must complete
/// the browser redirect within this window).
const FLOW_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Tokens expiring within this window trigger a proactive silent refresh.
const PROACTIVE_REFRESH_MINUTES: u64 = 5;

/// Loopback address used for the OAuth redirect listener.
const CALLBACK_BIND_HOST: &str = "127.0.0.1";

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors produced by the OAuth flow engine.
#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("HTTPS required for non-localhost URL: {0}")]
    InsecureUrl(String),

    #[error("metadata discovery failed: {0}")]
    Discovery(String),

    #[error("dynamic client registration failed: {0}")]
    Registration(String),

    #[error("authorization flow error: {0}")]
    Authorization(String),

    #[error("token exchange failed: {0}")]
    TokenExchange(String),

    #[error("token refresh failed: {0}")]
    TokenRefresh(String),

    /// The authorization server reported `error: "invalid_grant"` — RFC
    /// 6749 §5.2 defines this as "the provided authorization grant ... is
    /// invalid, expired, revoked, [or] does not match the redirect URI".
    /// Providers that rotate refresh tokens on every use (Notion among
    /// them) return exactly this when they detect reuse of an
    /// already-rotated-out refresh token, and respond by revoking the
    /// entire grant — not just failing the one refresh.
    ///
    /// Distinct from [`OAuthError::TokenRefresh`] (an unclassified/possibly
    /// transient refresh failure) so callers can tell "retry might work"
    /// from "the user must re-authorize from scratch; retrying will not
    /// help" — diagnosed straight from the token endpoint's structured
    /// error body, not inferred from a generic HTTP failure.
    #[error("OAuth grant revoked ({description}) — re-authorization is required")]
    GrantRevoked { description: String },

    #[error("CSRF state mismatch in callback — possible replay attack")]
    StateMismatch,

    #[error("authorization server returned error '{error}': {description}")]
    AuthServerError { error: String, description: String },

    #[error("interactive flow timed out after {0} seconds")]
    Timeout(u64),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("token store error: {0}")]
    TokenStore(String),
}

// ── RFC 8414 authorization-server metadata ────────────────────────────────────

/// Subset of RFC 8414 §2 authorization-server metadata.
///
/// Unknown fields are ignored; only fields actively used by the flow engine
/// are captured.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

// ── RFC 9728 protected-resource metadata ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

// ── PKCE parameters ───────────────────────────────────────────────────────────

struct PkceParams {
    code_verifier: String,
    code_challenge: String,
    /// CSRF state token.
    state: String,
}

// ── Public outcome ────────────────────────────────────────────────────────────

/// Handle returned by [`OAuthEngine::begin_authorization_flow`].
///
/// The caller must present `auth_url` to the user (e.g. via the auth
/// pseudo-tool). Awaiting `wait` yields `Ok(())` once the user has completed
/// the browser redirect, tokens have been exchanged, and the credential has
/// been persisted to the token store.
pub struct AuthFlowHandle {
    /// The authorization URL the user must open in their browser.
    pub auth_url: String,
    /// Resolves when the callback arrives and the token exchange completes.
    pub wait: tokio::task::JoinHandle<Result<(), OAuthError>>,
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// OAuth 2.0 PKCE flow engine for MCP server authorization.
///
/// Construct with [`OAuthEngine::new`]. All async methods are cancel-safe;
/// the background callback task respects a hard [`FLOW_TIMEOUT_SECS`] deadline.
pub struct OAuthEngine {
    http_client: reqwest::Client,
}

impl OAuthEngine {
    /// Create a new engine using the provided HTTP client.
    pub fn new(http_client: reqwest::Client) -> Self {
        Self { http_client }
    }

    /// Discover authorization-server metadata for `server_url`.
    ///
    /// When `metadata_url_override` is `Some`, fetch that URL directly and
    /// skip the discovery chain. Otherwise:
    ///
    /// 1. `GET {server_url}/.well-known/oauth-protected-resource` (RFC 9728)
    /// 2. Follow `authorization_servers[0]` →
    ///    `GET {as_origin}/.well-known/oauth-authorization-server` (RFC 8414)
    /// 3. If step 1 or 2 fails, fall back to
    ///    `GET {server_url}/.well-known/oauth-authorization-server` directly.
    pub async fn discover_metadata(
        &self,
        server_url: &str,
        metadata_url_override: Option<&str>,
    ) -> Result<AuthServerMetadata, OAuthError> {
        let base = server_url.trim_end_matches('/');

        if let Some(url) = metadata_url_override {
            require_https_or_localhost(url)?;
            return fetch_auth_server_metadata(&self.http_client, url).await;
        }

        require_https_or_localhost(base)?;

        // Step 1 — RFC 9728 protected-resource metadata.
        //
        // RFC 9728 §3.1 / RFC 8414 §3.1 mandate inserting the well-known
        // segment between the authority and the path (e.g.
        // `https://host/.well-known/oauth-protected-resource/mcp`), not
        // appending it after the path. Strict servers (e.g. GitHub) only
        // serve the inserted form; lenient ones (e.g. Linear) also answer the
        // appended form. Try the spec-correct URL first, then the appended
        // form for backward compatibility.
        for pr_url in well_known_candidates(base, "oauth-protected-resource") {
            debug!("mcp oauth: trying protected-resource discovery at {pr_url}");
            let pr = match fetch_protected_resource(&self.http_client, &pr_url).await {
                Ok(pr) => pr,
                Err(_) => continue,
            };
            let Some(as_origin) = pr.authorization_servers.first() else {
                continue;
            };
            require_https_or_localhost(as_origin)?;
            for as_meta_url in
                well_known_candidates(as_origin, "oauth-authorization-server")
            {
                debug!("mcp oauth: following auth-server chain to {as_meta_url}");
                if let Ok(meta) =
                    fetch_auth_server_metadata(&self.http_client, &as_meta_url).await
                {
                    return Ok(meta);
                }
            }
            // Try the issuer URL verbatim as a last resort for this origin.
            if let Ok(meta) =
                fetch_auth_server_metadata(&self.http_client, as_origin).await
            {
                return Ok(meta);
            }
        }

        // Fallback — RFC 8414 directly at the server origin.
        let mut last_err: Option<OAuthError> = None;
        for direct_url in well_known_candidates(base, "oauth-authorization-server") {
            debug!("mcp oauth: discovery chain fallback at {direct_url}");
            match fetch_auth_server_metadata(&self.http_client, &direct_url).await {
                Ok(meta) => return Ok(meta),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            OAuthError::Discovery("no discovery endpoint produced metadata".into())
        }))
    }

    /// Register a new OAuth client via Dynamic Client Registration (RFC 7591).
    ///
    /// Returns `(client_id, client_secret)`. `client_secret` is `None` when
    /// the server uses public clients.
    pub async fn register_dynamic_client(
        &self,
        registration_endpoint: &str,
        server_url: &str,
        redirect_uri: &str,
    ) -> Result<(String, Option<String>), OAuthError> {
        require_https_or_localhost(registration_endpoint)?;

        let body = serde_json::json!({
            "client_name": "Launchpad Studio MCP Client",
            "client_uri": server_url,
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });

        let resp = self
            .http_client
            .post(registration_endpoint)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(OAuthError::Registration(format!("HTTP {status}: {text}")));
        }

        #[derive(Deserialize)]
        struct DcrResponse {
            client_id: String,
            #[serde(default)]
            client_secret: Option<String>,
        }

        let dcr: DcrResponse = resp.json().await?;
        Ok((dcr.client_id, dcr.client_secret))
    }

    /// Begin the interactive authorization flow for an MCP server.
    ///
    /// Steps performed synchronously before returning:
    /// - Authorization-server discovery
    /// - Dynamic Client Registration (if no `client_id` is pre-configured)
    /// - Callback HTTP server binding
    /// - PKCE verifier / challenge and CSRF state generation
    /// - Authorization URL construction
    ///
    /// The caller receives the authorization URL immediately. The background
    /// task (awaitable via [`AuthFlowHandle::wait`]) listens for the browser
    /// redirect, validates state, exchanges the code for tokens, and persists
    /// the credential via the token store. The entire background task has a
    /// hard [`FLOW_TIMEOUT_SECS`] deadline.
    ///
    /// **No browser is opened automatically.** Delivering the URL is the
    /// caller's responsibility.
    pub async fn begin_authorization_flow(
        &self,
        server_key: &str,
        server_url: &str,
        auth_config: &McpAuthConfig,
        token_store: Arc<McpTokenStore>,
    ) -> Result<AuthFlowHandle, OAuthError> {
        // Discovery
        let metadata = self
            .discover_metadata(server_url, auth_config.metadata_url.as_deref())
            .await?;

        // Bind callback listener
        let bind_addr = match auth_config.callback_port_hint {
            Some(p) => format!("{CALLBACK_BIND_HOST}:{p}"),
            None => format!("{CALLBACK_BIND_HOST}:0"),
        };
        let listener = TcpListener::bind(&bind_addr).await?;
        let local_port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{local_port}/callback");

        // Client credentials (pre-configured or dynamic registration)
        let (client_id, client_secret) = resolve_client_credentials(
            &self.http_client,
            &metadata,
            auth_config,
            server_url,
            &redirect_uri,
        )
        .await?;

        // PKCE + CSRF
        let pkce = generate_pkce_params();
        let auth_url = build_authorization_url(
            &metadata.authorization_endpoint,
            &client_id,
            &redirect_uri,
            &pkce,
        );

        // Move owned values into background task
        let expected_state = pkce.state;
        let code_verifier = pkce.code_verifier;
        let token_endpoint = metadata.token_endpoint;
        let server_key_owned = server_key.to_owned();
        let http = self.http_client.clone();

        let wait = tokio::spawn(async move {
            let inner = run_callback_flow(
                listener,
                expected_state,
                code_verifier,
                token_endpoint,
                client_id,
                client_secret,
                redirect_uri,
                server_key_owned,
                token_store,
                http,
            );
            match timeout(Duration::from_secs(FLOW_TIMEOUT_SECS), inner).await {
                Ok(result) => result,
                Err(_) => Err(OAuthError::Timeout(FLOW_TIMEOUT_SECS)),
            }
        });

        Ok(AuthFlowHandle { auth_url, wait })
    }

    /// Return the current valid access token for `server_key`.
    ///
    /// If the stored token is expiring within [`PROACTIVE_REFRESH_MINUTES`]
    /// and a refresh token + token endpoint are stored, silently refreshes
    /// and persists the updated credential before returning.
    ///
    /// Every read this function does — both the initial check and the
    /// double-check below — goes through [`McpTokenStore::get_fresh`], which
    /// bypasses the keychain-backed process cache. This is the auth-decision
    /// choke point every connect/reconnect/post-auth path funnels through
    /// (low frequency — nowhere near as hot as provider-API-key reads), so
    /// paying for a fresh read here is cheap insurance against presenting a
    /// refresh token another process already rotated out.
    ///
    /// Refreshes are single-flighted per `server_key`: concurrent callers
    /// serialize on a lock, and the waiter re-checks the stored credential
    /// after acquiring it. If the winner already refreshed while the waiter
    /// was blocked, the waiter uses that freshly-stored token and skips its
    /// own exchange — this is what prevents two callers from ever presenting
    /// the same (single-use) refresh token to the provider.
    ///
    /// Returns `Ok(None)` when no credential is stored for the server — this
    /// is a clean, unambiguous "never authorized (or credential removed)"
    /// signal, distinct from `Err(_)` (a genuine store/refresh failure).
    /// Callers must preserve that distinction rather than treating both the
    /// same way (e.g. falling through to an unauthenticated connect for
    /// either outcome without recording which one happened) — that
    /// conflation is what let a stale in-process connector handle get
    /// misdiagnosed as a bad credential in the past.
    ///
    /// `Err(OAuthError::GrantRevoked { .. })` is a fourth, distinct outcome
    /// on top of "never authorized" / "stale cache" / "transient failure":
    /// the provider told us — via a structured `invalid_grant` response, not
    /// a generic HTTP failure — that this grant is dead and re-authorization
    /// is required. This function never retries in that case; the caller
    /// must surface it as terminal rather than looping.
    pub async fn current_access_token(
        &self,
        server_key: &str,
        token_store: &McpTokenStore,
    ) -> Result<Option<String>, OAuthError> {
        let record = match token_store
            .get_fresh(server_key)
            .map_err(|e| OAuthError::TokenStore(e.to_string()))?
        {
            Some(r) => r,
            None => return Ok(None),
        };

        if !record.is_expiring_within(PROACTIVE_REFRESH_MINUTES) {
            return Ok(Some(record.access_token.clone()));
        }

        // Single-flight: serialize refreshes per server key so two callers
        // racing to refresh (e.g. an initial connect and a concurrent
        // reconnect) can never both present the same refresh token — that's
        // exactly what trips reuse detection on providers that rotate
        // refresh tokens every use and respond by revoking the whole grant.
        let lock = refresh_lock_for(server_key);
        let _guard = lock.lock().await;

        // Double-check after acquiring the lock: while we waited, another
        // caller may have already refreshed and stored a newer credential.
        // If its access token is no longer expiring, use it and skip our own
        // exchange rather than presenting the (now rotated-out) refresh
        // token we saw before the lock.
        let record = match token_store
            .get_fresh(server_key)
            .map_err(|e| OAuthError::TokenStore(e.to_string()))?
        {
            Some(r) => r,
            None => return Ok(None),
        };

        if !record.is_expiring_within(PROACTIVE_REFRESH_MINUTES) {
            debug!(server_key, "token was refreshed by a concurrent caller while waiting for the refresh lock");
            return Ok(Some(record.access_token.clone()));
        }

        // Attempt proactive refresh when both a refresh_token and token_endpoint
        // are available.
        match (&record.refresh_token, &record.token_endpoint) {
            (Some(rt), Some(te)) => {
                debug!(server_key, "proactive token refresh (expires within {PROACTIVE_REFRESH_MINUTES} min)");
                match refresh_tokens(
                    &self.http_client,
                    te,
                    rt,
                    &record.client_id,
                    record.client_secret.as_deref(),
                )
                .await
                {
                    Ok(refreshed) => {
                        token_store
                            .set(server_key, &refreshed)
                            .map_err(|e| OAuthError::TokenStore(e.to_string()))?;
                        Ok(Some(refreshed.access_token))
                    }
                    // Terminal state — propagate as-is rather than folding
                    // into TokenRefresh, and do not retry.
                    Err(e @ OAuthError::GrantRevoked { .. }) => Err(e),
                    Err(e) => Err(OAuthError::TokenRefresh(e.to_string())),
                }
            }
            _ => {
                warn!(
                    server_key,
                    "token is expiring but no refresh_token or token_endpoint stored; \
                     returning current token"
                );
                Ok(Some(record.access_token.clone()))
            }
        }
    }
}

/// Per-`server_key` locks serializing OAuth token refreshes across every
/// caller in the process (initial connect, reconnect, post-auth promotion —
/// each constructs its own [`OAuthEngine`], so the lock registry lives here
/// at module scope rather than on the engine instance).
static REFRESH_LOCKS: OnceLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();

fn refresh_lock_for(server_key: &str) -> Arc<AsyncMutex<()>> {
    let registry = REFRESH_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(locks.entry(server_key.to_owned()).or_insert_with(|| Arc::new(AsyncMutex::new(()))))
}

// ── HTTPS guard ───────────────────────────────────────────────────────────────

/// Enforce HTTPS for all non-loopback URLs.
///
/// Plain HTTP is permitted only for loopback addresses so that local
/// development servers and integration tests work without TLS.
pub(crate) fn require_https_or_localhost(url: &str) -> Result<(), OAuthError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if lower.starts_with("http://127.")
        || lower.starts_with("http://localhost")
        || lower.starts_with("http://[::1]")
    {
        return Ok(());
    }
    if lower.starts_with("http://") {
        return Err(OAuthError::InsecureUrl(url.to_owned()));
    }
    Err(OAuthError::InsecureUrl(url.to_owned()))
}

// ── PKCE / CSRF generation ────────────────────────────────────────────────────

/// Generate PKCE `code_verifier`, `code_challenge` (S256), and CSRF `state`.
///
/// `code_verifier`: 32 OS-random bytes encoded as base64url (no padding) —
/// yields a 43-character ASCII string within RFC 7636's 43–128 char range.
///
/// `code_challenge`: `BASE64URL(SHA-256(ASCII(code_verifier)))`.
///
/// `state`: 32 OS-random bytes encoded as base64url (no padding).
fn generate_pkce_params() -> PkceParams {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).expect("OS random source failed");
    let code_verifier = URL_SAFE_NO_PAD.encode(raw);

    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(digest);

    let mut state_raw = [0u8; 32];
    getrandom::getrandom(&mut state_raw).expect("OS random source failed");
    let state = URL_SAFE_NO_PAD.encode(state_raw);

    PkceParams { code_verifier, code_challenge, state }
}

// ── Discovery helpers ─────────────────────────────────────────────────────────

/// Build the candidate well-known metadata URLs for `base`, most-correct first.
///
/// `well_known` is the suffix after `.well-known/` (e.g.
/// `oauth-protected-resource` or `oauth-authorization-server`).
///
/// 1. **Spec-correct (RFC 8414 §3.1 / RFC 9728 §3.1):** insert
///    `/.well-known/{well_known}` between the authority and the path —
///    `https://host[:port]/.well-known/{well_known}{path}`.
/// 2. **Compatibility fallback:** append `/.well-known/{well_known}` after the
///    full path — `{base}/.well-known/{well_known}`.
///
/// When the URL has no path component the two forms are identical, so only one
/// candidate is returned.
fn well_known_candidates(base: &str, well_known: &str) -> Vec<String> {
    let base = base.trim_end_matches('/');
    let mut candidates = Vec::with_capacity(2);

    if let Some(inserted) = well_known_inserted(base, well_known) {
        candidates.push(inserted);
    }

    let appended = format!("{base}/.well-known/{well_known}");
    if !candidates.contains(&appended) {
        candidates.push(appended);
    }

    candidates
}

/// Construct the spec-correct well-known URL by inserting the well-known
/// segment between the authority and the (preserved) path. Returns `None` if
/// `base` cannot be parsed or has no host.
fn well_known_inserted(base: &str, well_known: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base).ok()?;
    let host = parsed.host_str()?;
    let authority = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let path = parsed.path().trim_end_matches('/');
    Some(format!(
        "{}://{authority}/.well-known/{well_known}{path}",
        parsed.scheme()
    ))
}

async fn fetch_protected_resource(
    client: &reqwest::Client,
    url: &str,
) -> Result<ProtectedResourceMetadata, OAuthError> {
    let resp = client.get(url).header("Accept", "application/json").send().await?;
    if !resp.status().is_success() {
        return Err(OAuthError::Discovery(format!("HTTP {} at {url}", resp.status())));
    }
    Ok(resp.json().await?)
}

async fn fetch_auth_server_metadata(
    client: &reqwest::Client,
    url: &str,
) -> Result<AuthServerMetadata, OAuthError> {
    let resp = client.get(url).header("Accept", "application/json").send().await?;
    if !resp.status().is_success() {
        return Err(OAuthError::Discovery(format!("HTTP {} at {url}", resp.status())));
    }
    Ok(resp.json().await?)
}

// ── Client credential resolution ─────────────────────────────────────────────

/// Resolve the `(client_id, client_secret)` pair for the flow.
///
/// A pre-configured `client_id` short-circuits Dynamic Client Registration —
/// the configured `client_secret` (if any) rides along for confidential
/// clients that authenticate at the token endpoint, while public PKCE clients
/// leave it `None`. Only when no `client_id` is configured do we fall back to
/// RFC 7591 DCR, which provisions a public client.
async fn resolve_client_credentials(
    http: &reqwest::Client,
    metadata: &AuthServerMetadata,
    auth_config: &McpAuthConfig,
    server_url: &str,
    redirect_uri: &str,
) -> Result<(String, Option<String>), OAuthError> {
    if let Some(client_id) = &auth_config.client_id {
        return Ok((client_id.clone(), auth_config.client_secret.clone()));
    }

    let reg_endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        OAuthError::Registration(
            "no client_id configured and the authorization server does not \
             advertise a Dynamic Client Registration endpoint"
                .into(),
        )
    })?;

    let engine = OAuthEngine::new(http.clone());
    engine.register_dynamic_client(reg_endpoint, server_url, redirect_uri).await
}

// ── Authorization URL construction ───────────────────────────────────────────

fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceParams,
) -> String {
    let params: &[(&str, &str)] = &[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge_method", "S256"),
        ("code_challenge", &pkce.code_challenge),
        ("state", &pkce.state),
    ];

    let sep = if authorization_endpoint.contains('?') { '&' } else { '?' };
    let qs: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{authorization_endpoint}{sep}{qs}")
}

// ── Callback flow ─────────────────────────────────────────────────────────────

/// Accept exactly one TCP connection on `listener`, parse the OAuth callback
/// query parameters, validate CSRF state, exchange the code for tokens, and
/// persist the credential.
async fn run_callback_flow(
    listener: TcpListener,
    expected_state: String,
    code_verifier: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
    server_key: String,
    token_store: Arc<McpTokenStore>,
    http: reqwest::Client,
) -> Result<(), OAuthError> {
    let (mut stream, _peer) = listener.accept().await?;

    // Read the incoming HTTP request — the first line carries path + query.
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let raw = String::from_utf8_lossy(&buf[..n]);

    let first_line = raw.lines().next().unwrap_or("");
    // e.g. "GET /callback?code=...&state=... HTTP/1.1"
    let path_query = first_line.split_whitespace().nth(1).unwrap_or("");
    let query_str = path_query.splitn(2, '?').nth(1).unwrap_or("");
    let params = parse_query(query_str);

    // Surface any OAuth error_code the server sent in the redirect.
    if let Some(err) = params.get("error") {
        let description =
            params.get("error_description").cloned().unwrap_or_default();
        send_html_response(&mut stream, false, "Authorization failed").await.ok();
        return Err(OAuthError::AuthServerError {
            error: err.clone(),
            description,
        });
    }

    // CSRF state validation
    let received_state = params.get("state").cloned().unwrap_or_default();
    if received_state != expected_state {
        send_html_response(&mut stream, false, "Authorization failed: invalid state")
            .await
            .ok();
        return Err(OAuthError::StateMismatch);
    }

    let code = params.get("code").cloned().ok_or_else(|| {
        OAuthError::Authorization("callback request missing 'code' parameter".into())
    })?;

    // Token exchange — never log `code` or the resulting tokens.
    let record = exchange_code_for_tokens(
        &http,
        &token_endpoint,
        &code,
        &code_verifier,
        &client_id,
        client_secret.as_deref(),
        &redirect_uri,
    )
    .await?;

    token_store
        .set(&server_key, &record)
        .map_err(|e| OAuthError::TokenStore(e.to_string()))?;

    send_html_response(&mut stream, true, "Authorization complete").await.ok();
    Ok(())
}

// ── Token exchange ────────────────────────────────────────────────────────────

async fn exchange_code_for_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    code_verifier: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
) -> Result<McpTokenRecord, OAuthError> {
    let pairs = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    let form_body = encode_form(&pairs);

    let mut req = http
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded");

    if let Some(secret) = client_secret {
        req = req.basic_auth(client_id, Some(secret));
    }

    let resp = req.body(form_body).send().await?;
    parse_token_response(resp, token_endpoint, client_id, client_secret, None).await
}

// ── Token refresh ─────────────────────────────────────────────────────────────

async fn refresh_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<McpTokenRecord, OAuthError> {
    let pairs = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let form_body = encode_form(&pairs);

    let mut req = http
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded");

    if let Some(secret) = client_secret {
        req = req.basic_auth(client_id, Some(secret));
    }

    let resp = req.body(form_body).send().await?;
    parse_token_response(resp, token_endpoint, client_id, client_secret, Some(refresh_token))
        .await
}

/// RFC 6749 §5.2 token-endpoint error body. Parsed only to detect
/// `invalid_grant`; every other error code keeps flowing through the
/// generic [`OAuthError::TokenExchange`] path unchanged.
#[derive(Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Parse a token endpoint response and build a [`McpTokenRecord`].
///
/// `existing_refresh_token` is used when the new response omits a
/// `refresh_token` field — the stored refresh token is then carried forward.
async fn parse_token_response(
    resp: reqwest::Response,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    existing_refresh_token: Option<&str>,
) -> Result<McpTokenRecord, OAuthError> {
    if !resp.status().is_success() {
        let status = resp.status();
        // Read body for diagnostics — never log the actual tokens
        let body = resp.text().await.unwrap_or_default();

        // `invalid_grant` is diagnosed straight from the structured error
        // body (never inferred from the raw HTTP status/text), so it is
        // trustworthy enough to treat as terminal — see `OAuthError::GrantRevoked`.
        if let Ok(err_body) = serde_json::from_str::<OAuthErrorBody>(&body) {
            if err_body.error == "invalid_grant" {
                return Err(OAuthError::GrantRevoked {
                    description: err_body
                        .error_description
                        .unwrap_or_else(|| "no description provided".to_owned()),
                });
            }
        }

        return Err(OAuthError::TokenExchange(format!("HTTP {status}: {body}")));
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<u64>,
        #[serde(default)]
        scope: Option<String>,
    }

    let raw: TokenResponse = resp.json().await?;

    let expires_at = raw.expires_in.map(|secs| {
        Utc::now() + chrono::Duration::seconds(secs as i64)
    });

    // Prefer a newly-issued refresh token; fall back to the caller-supplied one.
    let refresh_token =
        raw.refresh_token.or_else(|| existing_refresh_token.map(str::to_owned));

    Ok(McpTokenRecord {
        access_token: raw.access_token,
        refresh_token,
        expires_at,
        scope: raw.scope,
        client_id: client_id.to_owned(),
        client_secret: client_secret.map(str::to_owned),
        token_endpoint: Some(token_endpoint.to_owned()),
    })
}

// ── HTTP callback page ────────────────────────────────────────────────────────

/// Send a minimal HTML response to the user's browser after the OAuth redirect.
///
/// The `message` string is HTML-escaped before insertion to prevent XSS.
async fn send_html_response(
    stream: &mut tokio::net::TcpStream,
    success: bool,
    message: &str,
) -> Result<(), OAuthError> {
    let title = if success { "Authorization complete" } else { "Authorization failed" };
    let safe_msg = html_escape(message);
    let body = format!(
        "<!DOCTYPE html><html><head><title>{title}</title></head>\
         <body><h1>{title}</h1><p>{safe_msg}</p>\
         <p>You may close this tab.</p></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// Escape `<`, `>`, `&`, `"`, `'` to prevent XSS in the callback HTML page.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

// ── URL encoding helpers ──────────────────────────────────────────────────────

/// Percent-encode a string per RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

/// URL-encode a slice of `(key, value)` pairs into an
/// `application/x-www-form-urlencoded` body.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse a URL query string into a `key → value` map.
///
/// Values are percent-decoded. Duplicate keys keep the last value seen.
/// Keys with no `=` are ignored.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in query.split('&') {
        let mut kv = part.splitn(2, '=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

/// Percent-decode a URL-encoded string (`%XX` sequences and `+` → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex_str, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    // `chrono::Duration` is imported below under the bare name `Duration`
    // (the existing convention throughout this test module, used for token
    // expiry offsets); the std duration needed for mock-server response
    // delays is aliased to avoid colliding with it.
    use std::time::Duration as StdDuration;

    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::{routing, Json, Router};
    use chrono::Duration;
    use serde_json::json;

    // ── Well-known URL construction ───────────────────────────────────────────

    #[test]
    fn well_known_inserts_segment_between_authority_and_path() {
        // RFC 8414 §3.1 / RFC 9728 §3.1: the well-known segment goes between
        // the authority and the path, not appended after it. This is the form
        // GitHub's MCP server (api.githubcopilot.com/mcp) requires.
        let candidates =
            well_known_candidates("https://api.githubcopilot.com/mcp", "oauth-protected-resource");
        assert_eq!(
            candidates[0],
            "https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp",
            "spec-correct inserted form must be tried first"
        );
        assert!(
            candidates.contains(
                &"https://api.githubcopilot.com/mcp/.well-known/oauth-protected-resource"
                    .to_owned()
            ),
            "appended form retained as compatibility fallback"
        );
    }

    #[test]
    fn well_known_handles_nested_issuer_path() {
        // GitHub's authorization server issuer is github.com/login/oauth.
        let candidates = well_known_candidates(
            "https://github.com/login/oauth",
            "oauth-authorization-server",
        );
        assert_eq!(
            candidates[0],
            "https://github.com/.well-known/oauth-authorization-server/login/oauth"
        );
    }

    #[test]
    fn well_known_collapses_to_single_candidate_without_path() {
        // No path component → inserted and appended forms are identical.
        let candidates =
            well_known_candidates("https://mcp.example.com", "oauth-authorization-server");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0],
            "https://mcp.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn well_known_preserves_explicit_port() {
        let candidates =
            well_known_candidates("https://host.example:8443/mcp", "oauth-protected-resource");
        assert_eq!(
            candidates[0],
            "https://host.example:8443/.well-known/oauth-protected-resource/mcp"
        );
    }

    // ── Environment guard ─────────────────────────────────────────────────────

    /// Open a file-backed token store rooted at the guard's data dir.
    ///
    /// Takes the crate-wide [`crate::test_env::DataDirGuard`] as a witness:
    /// the guard has already pinned `LAUNCHPAD_STUDIO_DATA_DIR` to a private
    /// tempdir under the crate-wide env lock, so the store lands there.
    /// The file-fallback flag is left set for the whole test binary on
    /// purpose — unit tests must never touch the real OS keychain.
    fn open_file_store(_guard: &crate::test_env::DataDirGuard) -> Arc<McpTokenStore> {
        std::env::set_var("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");
        Arc::new(McpTokenStore::open().expect("open store"))
    }

    // ── Mock authorization server ─────────────────────────────────────────────

    /// What `/token` returns instead of the normal success response.
    #[derive(Clone, Copy)]
    enum TokenErrorMode {
        /// RFC 6749 `invalid_grant` — models Notion's refresh-token-reuse
        /// revocation response.
        InvalidGrant,
    }

    /// Shared state for the in-process mock authorization server.
    #[derive(Clone)]
    struct MockServerState {
        base_url: Arc<String>,
        /// When `Some`, `/register` returns this client_id immediately.
        preset_client_id: Arc<Option<String>>,
        /// When true, `/.well-known/oauth-protected-resource` returns 404.
        no_protected_resource: bool,
        /// Records the `Authorization` header seen at `/token`, if any.
        captured_token_auth: Arc<Mutex<Option<String>>>,
        /// Total number of requests received at `/token` — used to prove
        /// single-flight (exactly one exchange for N concurrent callers) and
        /// no-retry (exactly one exchange before a terminal error) behavior.
        token_call_count: Arc<AtomicUsize>,
        /// When `Some`, `/token` returns this error instead of succeeding.
        token_error: Arc<Option<TokenErrorMode>>,
        /// When `Some`, `/token` sleeps this long before responding — lets
        /// tests force two concurrent refresh attempts to actually overlap.
        token_delay: Arc<Option<StdDuration>>,
    }

    struct MockAuthServer {
        addr: SocketAddr,
        _handle: tokio::task::JoinHandle<()>,
        /// The `Authorization` header observed at the token endpoint.
        captured_token_auth: Arc<Mutex<Option<String>>>,
        /// See [`MockServerState::token_call_count`].
        token_call_count: Arc<AtomicUsize>,
    }

    impl MockAuthServer {
        async fn start_with(
            preset_client_id: Option<&str>,
            no_protected_resource: bool,
        ) -> Self {
            Self::start_full(preset_client_id, no_protected_resource, None, None).await
        }

        async fn start_with_token_error(token_error: TokenErrorMode) -> Self {
            Self::start_full(None, false, Some(token_error), None).await
        }

        async fn start_with_token_delay(delay: StdDuration) -> Self {
            Self::start_full(None, false, None, Some(delay)).await
        }

        async fn start_full(
            preset_client_id: Option<&str>,
            no_protected_resource: bool,
            token_error: Option<TokenErrorMode>,
            token_delay: Option<StdDuration>,
        ) -> Self {
            let listener =
                tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://127.0.0.1:{}", addr.port());

            let captured_token_auth = Arc::new(Mutex::new(None));
            let token_call_count = Arc::new(AtomicUsize::new(0));
            let state = MockServerState {
                base_url: Arc::new(base_url),
                preset_client_id: Arc::new(preset_client_id.map(str::to_owned)),
                no_protected_resource,
                captured_token_auth: Arc::clone(&captured_token_auth),
                token_call_count: Arc::clone(&token_call_count),
                token_error: Arc::new(token_error),
                token_delay: Arc::new(token_delay),
            };

            let app = Router::new()
                .route(
                    "/.well-known/oauth-protected-resource",
                    routing::get(handle_protected_resource),
                )
                .route(
                    "/.well-known/oauth-authorization-server",
                    routing::get(handle_as_metadata),
                )
                .route("/register", routing::post(handle_register))
                .route("/authorize", routing::get(|| async { "not used in tests" }))
                .route("/token", routing::post(handle_token))
                .with_state(state);

            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            Self { addr, _handle: handle, captured_token_auth, token_call_count }
        }

        async fn start() -> Self {
            Self::start_with(None, false).await
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.addr.port())
        }
    }

    async fn handle_protected_resource(
        State(state): State<MockServerState>,
    ) -> impl IntoResponse {
        if state.no_protected_resource {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        Json(json!({
            "resource": *state.base_url,
            "authorization_servers": [*state.base_url],
        }))
        .into_response()
    }

    async fn handle_as_metadata(
        State(state): State<MockServerState>,
    ) -> impl IntoResponse {
        Json(json!({
            "issuer": *state.base_url,
            "authorization_endpoint": format!("{}/authorize", *state.base_url),
            "token_endpoint": format!("{}/token", *state.base_url),
            "registration_endpoint": format!("{}/register", *state.base_url),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        }))
    }

    async fn handle_register(State(state): State<MockServerState>) -> impl IntoResponse {
        let client_id = state
            .preset_client_id
            .as_deref()
            .unwrap_or("dyn_client_abc")
            .to_owned();
        Json(json!({ "client_id": client_id }))
    }

    async fn handle_token(
        State(state): State<MockServerState>,
        headers: axum::http::HeaderMap,
    ) -> impl IntoResponse {
        if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
            *state.captured_token_auth.lock().unwrap() =
                Some(auth.to_str().unwrap_or_default().to_owned());
        }
        state.token_call_count.fetch_add(1, Ordering::SeqCst);

        if let Some(delay) = *state.token_delay {
            tokio::time::sleep(delay).await;
        }

        if matches!(*state.token_error, Some(TokenErrorMode::InvalidGrant)) {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_grant",
                    "error_description": "Refresh token reuse detected",
                })),
            );
        }

        (
            axum::http::StatusCode::OK,
            Json(json!({
                "access_token": "mock_access_token_xyz",
                "refresh_token": "mock_refresh_token_xyz",
                "expires_in": 3600,
                "scope": "mcp:tools",
            })),
        )
    }

    // ── Helper: extract redirect_uri and state from an auth URL ──────────────

    fn extract_auth_params(auth_url: &str) -> (String, String) {
        let qs = auth_url.splitn(2, '?').nth(1).unwrap_or("");
        let params = parse_query(qs);
        let redirect_uri = params
            .get("redirect_uri")
            .cloned()
            .expect("redirect_uri in auth URL");
        let state = params.get("state").cloned().expect("state in auth URL");
        (redirect_uri, state)
    }

    // ── Tests: HTTPS guard ────────────────────────────────────────────────────

    #[test]
    fn https_guard_accepts_https() {
        assert!(require_https_or_localhost("https://example.com/mcp").is_ok());
    }

    #[test]
    fn https_guard_accepts_localhost() {
        assert!(require_https_or_localhost("http://localhost:8080/mcp").is_ok());
    }

    #[test]
    fn https_guard_accepts_loopback_ip() {
        assert!(require_https_or_localhost("http://127.0.0.1:8080/mcp").is_ok());
        assert!(require_https_or_localhost("http://[::1]:8080/mcp").is_ok());
    }

    #[test]
    fn https_guard_rejects_plain_http() {
        let err = require_https_or_localhost("http://example.com/mcp")
            .expect_err("should reject plain http");
        assert!(matches!(err, OAuthError::InsecureUrl(_)));
    }

    // ── Tests: URL helpers ────────────────────────────────────────────────────

    #[test]
    fn percent_encode_leaves_unreserved_chars() {
        assert_eq!(percent_encode("abc-_~."), "abc-_~.");
    }

    #[test]
    fn percent_encode_encodes_slash_and_colon() {
        let encoded = percent_encode("http://example.com/path");
        assert!(encoded.contains("%3A"), "colon not encoded: {encoded}");
        assert!(encoded.contains("%2F"), "slash not encoded: {encoded}");
    }

    #[test]
    fn percent_decode_handles_percent_sequences() {
        assert_eq!(percent_decode("http%3A%2F%2Flocalhost"), "http://localhost");
    }

    #[test]
    fn percent_decode_handles_plus_as_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn parse_query_round_trips_simple() {
        let m = parse_query("code=abc123&state=xyz");
        assert_eq!(m.get("code").map(String::as_str), Some("abc123"));
        assert_eq!(m.get("state").map(String::as_str), Some("xyz"));
    }

    #[test]
    fn parse_query_decodes_redirect_uri() {
        let qs = "redirect_uri=http%3A%2F%2Flocalhost%3A12345%2Fcallback";
        let m = parse_query(qs);
        assert_eq!(
            m.get("redirect_uri").map(String::as_str),
            Some("http://localhost:12345/callback")
        );
    }

    // ── Tests: PKCE generation ────────────────────────────────────────────────

    #[test]
    fn pkce_verifier_length_in_spec_range() {
        let p = generate_pkce_params();
        // RFC 7636: code_verifier must be 43–128 URL-safe chars
        assert!(
            p.code_verifier.len() >= 43 && p.code_verifier.len() <= 128,
            "len={}", p.code_verifier.len()
        );
    }

    #[test]
    fn pkce_challenge_is_correct_s256() {
        let p = generate_pkce_params();
        let digest = Sha256::digest(p.code_verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(p.code_challenge, expected);
    }

    #[test]
    fn pkce_each_call_generates_unique_params() {
        let a = generate_pkce_params();
        let b = generate_pkce_params();
        assert_ne!(a.code_verifier, b.code_verifier);
        assert_ne!(a.state, b.state);
    }

    // ── Tests: HTML escaping ──────────────────────────────────────────────────

    #[test]
    fn html_escape_prevents_xss() {
        let out = html_escape("<script>alert('xss')</script>");
        assert!(!out.contains('<'), "unescaped <");
        assert!(!out.contains('>'), "unescaped >");
        assert!(out.contains("&lt;"));
        assert!(out.contains("&gt;"));
        assert!(out.contains("&#39;"));
    }

    // ── Integration: full PKCE happy path ────────────────────────────────────

    #[tokio::test]
    async fn pkce_happy_path_with_dcr() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();
        let engine = OAuthEngine::new(reqwest::Client::new());

        let auth_config = McpAuthConfig {
            metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
            ..Default::default()
        };
        let server_key = "test_happy_path";

        let handle = engine
            .begin_authorization_flow(server_key, &base, &auth_config, store.clone())
            .await
            .expect("begin_authorization_flow");

        // Simulate the browser redirect: send the callback request.
        let (redirect_uri, state) = extract_auth_params(&handle.auth_url);
        let callback_url =
            format!("{redirect_uri}?code=test_auth_code&state={state}");

        // Fire the callback in a separate task so it doesn't block.
        tokio::spawn(async move {
            reqwest::get(&callback_url).await.ok();
        });

        handle.wait.await.expect("join").expect("flow completed");

        let record = store.get(server_key).expect("store get").expect("record present");
        assert_eq!(record.access_token, "mock_access_token_xyz");
        assert_eq!(record.refresh_token.as_deref(), Some("mock_refresh_token_xyz"));
        assert_eq!(record.client_id, "dyn_client_abc");
        assert!(record.token_endpoint.is_some(), "token_endpoint persisted");
    }

    // ── Integration: state mismatch rejected ─────────────────────────────────

    #[tokio::test]
    async fn state_mismatch_is_rejected() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();
        let engine = OAuthEngine::new(reqwest::Client::new());

        let auth_config = McpAuthConfig {
            metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
            ..Default::default()
        };

        let handle = engine
            .begin_authorization_flow("test_state_mismatch", &base, &auth_config, store)
            .await
            .expect("begin_authorization_flow");

        let (redirect_uri, _correct_state) = extract_auth_params(&handle.auth_url);
        // Send a deliberately wrong state value
        let callback_url =
            format!("{redirect_uri}?code=test_code&state=WRONG_STATE_VALUE");

        tokio::spawn(async move {
            reqwest::get(&callback_url).await.ok();
        });

        let result = handle.wait.await.expect("join");
        assert!(
            matches!(result, Err(OAuthError::StateMismatch)),
            "expected StateMismatch, got: {result:?}"
        );
    }

    // ── Integration: expired-token proactive refresh ──────────────────────────

    #[tokio::test]
    async fn expired_token_is_proactively_refreshed() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();

        // Store an already-expired token record pointing at the mock token endpoint.
        let server_key = "test_refresh";
        let expired_record = McpTokenRecord {
            access_token: "old_expired_token".to_owned(),
            refresh_token: Some("old_refresh_token".to_owned()),
            expires_at: Some(Utc::now() - Duration::minutes(10)), // expired
            scope: Some("mcp:tools".to_owned()),
            client_id: "test_client".to_owned(),
            client_secret: None,
            token_endpoint: Some(format!("{base}/token")),
        };
        store.set(server_key, &expired_record).expect("set");

        let engine = OAuthEngine::new(reqwest::Client::new());
        let access_token = engine
            .current_access_token(server_key, &store)
            .await
            .expect("current_access_token")
            .expect("Some token");

        assert_eq!(access_token, "mock_access_token_xyz", "got fresh token from refresh");

        // Verify the new record is persisted
        let new_record = store.get(server_key).expect("get").expect("present");
        assert_eq!(new_record.access_token, "mock_access_token_xyz");
        assert!(new_record.expires_at.is_some(), "expiry persisted from refresh response");
    }

    // ── Integration: non-expired token returned without refresh ──────────────

    #[tokio::test]
    async fn non_expiring_token_returned_directly() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server_key = "test_no_refresh";
        let fresh_record = McpTokenRecord {
            access_token: "still_valid_token".to_owned(),
            refresh_token: Some("rt".to_owned()),
            expires_at: Some(Utc::now() + Duration::hours(2)),
            scope: None,
            client_id: "c1".to_owned(),
            client_secret: None,
            token_endpoint: Some("http://localhost:9999/token".to_owned()),
        };
        store.set(server_key, &fresh_record).expect("set");

        let engine = OAuthEngine::new(reqwest::Client::new());
        let token = engine
            .current_access_token(server_key, &store)
            .await
            .expect("ok")
            .expect("some");

        assert_eq!(token, "still_valid_token");
    }

    // ── Integration: no stored credential returns None ────────────────────────

    #[tokio::test]
    async fn no_credential_returns_none() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let engine = OAuthEngine::new(reqwest::Client::new());
        let result = engine
            .current_access_token("nonexistent_server", &store)
            .await
            .expect("ok");
        assert!(result.is_none());
    }

    // ── Integration: invalid_grant maps to GrantRevoked, no retry ────────────

    #[tokio::test]
    async fn invalid_grant_response_surfaces_as_grant_revoked_and_is_not_retried() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start_with_token_error(TokenErrorMode::InvalidGrant).await;
        let base = server.base_url();

        let server_key = "test_grant_revoked";
        let expired_record = McpTokenRecord {
            access_token: "old_expired_token".to_owned(),
            refresh_token: Some("reused_refresh_token".to_owned()),
            expires_at: Some(Utc::now() - Duration::minutes(10)),
            scope: Some("mcp:tools".to_owned()),
            client_id: "test_client".to_owned(),
            client_secret: None,
            token_endpoint: Some(format!("{base}/token")),
        };
        store.set(server_key, &expired_record).expect("seed expired credential with refresh token");

        let engine = OAuthEngine::new(reqwest::Client::new());
        let err = engine
            .current_access_token(server_key, &store)
            .await
            .expect_err("invalid_grant must surface as an error, not a token");

        match err {
            OAuthError::GrantRevoked { description } => {
                assert!(description.contains("reuse"), "description: {description}");
            }
            other => panic!("expected GrantRevoked, got {other:?}"),
        }

        assert_eq!(
            server.token_call_count.load(Ordering::SeqCst),
            1,
            "a revoked grant must not be retried"
        );

        // Nothing was successfully refreshed, so the stale record on disk
        // must be left untouched rather than overwritten with garbage.
        let still_stored = store.get(server_key).expect("get").expect("present");
        assert_eq!(still_stored.access_token, "old_expired_token");
    }

    // ── Integration: concurrent refreshes single-flight to one exchange ──────

    #[tokio::test]
    async fn concurrent_refreshes_single_flight_into_one_token_exchange() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        // A response delay forces the two spawned callers below to actually
        // overlap: without it, the second call could simply lose the race to
        // the first, observe the already-refreshed record on its very first
        // (pre-lock) read, and never contend for the lock at all — which
        // would exercise a different, less interesting path than genuine
        // concurrent refreshers.
        let server = MockAuthServer::start_with_token_delay(StdDuration::from_millis(80)).await;
        let base = server.base_url();

        let server_key = "test_single_flight";
        let expired_record = McpTokenRecord {
            access_token: "old_expired_token".to_owned(),
            refresh_token: Some("old_refresh_token".to_owned()),
            expires_at: Some(Utc::now() - Duration::minutes(10)),
            scope: Some("mcp:tools".to_owned()),
            client_id: "test_client".to_owned(),
            client_secret: None,
            token_endpoint: Some(format!("{base}/token")),
        };
        store.set(server_key, &expired_record).expect("seed");

        let engine = Arc::new(OAuthEngine::new(reqwest::Client::new()));

        let (e1, s1, k1) = (Arc::clone(&engine), store.clone(), server_key.to_owned());
        let (e2, s2, k2) = (Arc::clone(&engine), store.clone(), server_key.to_owned());

        let task1 = tokio::spawn(async move { e1.current_access_token(&k1, &s1).await });
        let task2 = tokio::spawn(async move { e2.current_access_token(&k2, &s2).await });
        let (r1, r2) = tokio::join!(task1, task2);

        let token1 = r1.expect("join").expect("ok").expect("some token");
        let token2 = r2.expect("join").expect("ok").expect("some token");

        assert_eq!(token1, token2, "both concurrent callers must observe the same fresh token");
        assert_eq!(
            server.token_call_count.load(Ordering::SeqCst),
            1,
            "two concurrent refreshes for the same server_key must exchange exactly once"
        );
    }

    // ── Integration: discovery chain fallback ────────────────────────────────

    #[tokio::test]
    async fn discovery_falls_back_when_protected_resource_absent() {
        // Start a mock server that has NO /.well-known/oauth-protected-resource
        // but DOES have /.well-known/oauth-authorization-server.
        let server =
            MockAuthServer::start_with(Some("pre_registered_client"), true).await;
        let base = server.base_url();

        let engine = OAuthEngine::new(reqwest::Client::new());
        // No metadata_url_override — must run the full discovery chain.
        let metadata = engine
            .discover_metadata(&base, None)
            .await
            .expect("discovery succeeds via RFC 8414 fallback");

        assert!(
            metadata.authorization_endpoint.contains("/authorize"),
            "got: {}",
            metadata.authorization_endpoint
        );
    }

    // ── Integration: discovery via RFC 9728 chain ─────────────────────────────

    #[tokio::test]
    async fn discovery_follows_protected_resource_chain() {
        // The mock server returns /.well-known/oauth-protected-resource pointing
        // to itself, which then serves /.well-known/oauth-authorization-server.
        let server = MockAuthServer::start().await;
        let base = server.base_url();

        let engine = OAuthEngine::new(reqwest::Client::new());
        let metadata = engine
            .discover_metadata(&base, None)
            .await
            .expect("discovery via RFC 9728 chain");

        assert!(metadata.issuer.contains("127.0.0.1"));
        assert!(
            metadata.registration_endpoint.is_some(),
            "registration endpoint present"
        );
    }

    // ── Integration: pre-configured client_id skips DCR ──────────────────────

    #[tokio::test]
    async fn preconfigured_client_id_skips_dcr() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();
        let engine = OAuthEngine::new(reqwest::Client::new());

        // Provide a pre-configured client_id — DCR endpoint should NOT be called.
        let auth_config = McpAuthConfig {
            client_id: Some("my_preconfigured_client".to_owned()),
            metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
            ..Default::default()
        };

        let handle = engine
            .begin_authorization_flow(
                "test_preconfigured",
                &base,
                &auth_config,
                store.clone(),
            )
            .await
            .expect("begin");

        assert!(
            handle.auth_url.contains("client_id=my_preconfigured_client"),
            "auth URL uses preconfigured client_id: {}",
            handle.auth_url
        );

        // Simulate callback to clean up the background task
        let (redirect_uri, state) = extract_auth_params(&handle.auth_url);
        let callback_url = format!("{redirect_uri}?code=xyz&state={state}");
        tokio::spawn(async move {
            reqwest::get(&callback_url).await.ok();
        });
        handle.wait.await.expect("join").expect("flow ok");

        let record =
            store.get("test_preconfigured").expect("get").expect("present");
        assert_eq!(record.client_id, "my_preconfigured_client");
    }

    // ── Integration: confidential pre-registered client (client_secret) ───────

    #[tokio::test]
    async fn preconfigured_confidential_client_authenticates_at_token_endpoint() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;

        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();
        let engine = OAuthEngine::new(reqwest::Client::new());

        // A confidential client: client_id + client_secret, no DCR.
        let auth_config = McpAuthConfig {
            client_id: Some("conf_client".to_owned()),
            client_secret: Some("s3cr3t".to_owned()),
            metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
            ..Default::default()
        };

        let handle = engine
            .begin_authorization_flow("test_conf", &base, &auth_config, store.clone())
            .await
            .expect("begin");

        assert!(
            handle.auth_url.contains("client_id=conf_client"),
            "auth URL uses the configured client_id: {}",
            handle.auth_url
        );

        let (redirect_uri, state) = extract_auth_params(&handle.auth_url);
        let callback_url = format!("{redirect_uri}?code=xyz&state={state}");
        tokio::spawn(async move {
            reqwest::get(&callback_url).await.ok();
        });
        handle.wait.await.expect("join").expect("flow ok");

        // The token-endpoint request must carry HTTP Basic auth derived from
        // client_id:client_secret — that's what confidential clients require
        // and what a public PKCE client would omit.
        let captured = server.captured_token_auth.lock().unwrap().clone();
        let auth = captured.expect("token endpoint saw an Authorization header");
        let b64 = auth
            .strip_prefix("Basic ")
            .unwrap_or_else(|| panic!("expected Basic auth, got: {auth}"));
        let decoded = String::from_utf8(STANDARD.decode(b64).expect("base64"))
            .expect("utf8 credentials");
        assert_eq!(decoded, "conf_client:s3cr3t");

        // The secret is persisted on the record so proactive refresh can
        // re-authenticate later.
        let record = store.get("test_conf").expect("get").expect("present");
        assert_eq!(record.client_id, "conf_client");
        assert_eq!(record.client_secret.as_deref(), Some("s3cr3t"));
    }

    // ── Integration: authorization server error in callback ───────────────────

    #[tokio::test]
    async fn auth_server_error_in_callback_surfaces_as_error() {
        let guard = crate::test_env::DataDirGuard::new();
        let store = open_file_store(&guard);

        let server = MockAuthServer::start().await;
        let base = server.base_url();
        let engine = OAuthEngine::new(reqwest::Client::new());

        let auth_config = McpAuthConfig {
            metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
            ..Default::default()
        };

        let handle = engine
            .begin_authorization_flow("test_as_error", &base, &auth_config, store)
            .await
            .expect("begin");

        let (redirect_uri, _state) = extract_auth_params(&handle.auth_url);
        // Simulate auth server sending an error redirect
        let error_callback = format!(
            "{redirect_uri}?error=access_denied&error_description=User+denied+access"
        );
        tokio::spawn(async move {
            reqwest::get(&error_callback).await.ok();
        });

        let result = handle.wait.await.expect("join");
        assert!(
            matches!(result, Err(OAuthError::AuthServerError { ref error, .. }) if error == "access_denied"),
            "expected AuthServerError(access_denied), got: {result:?}"
        );
    }
}
