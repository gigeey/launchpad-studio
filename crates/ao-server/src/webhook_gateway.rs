//! Signature verification and secret resolution for the named-route webhook
//! gateway (`POST /webhooks/{route_name}`, wired in
//! `crate::routes::webhooks`).
//!
//! Kept separate from the axum handler so the security-critical logic —
//! HMAC verification, the `INSECURE_NO_AUTH` loopback gate, delivery-id
//! header preference — is plain, synchronous, unit-testable code with no
//! HTTP framework or persistence layer in the loop. The handler calls
//! [`verify_request_signature`] unconditionally on every request; there is
//! no separate authorization layer upstream of it that a direct call to the
//! handler could bypass, which is what makes this "both at startup and
//! per-request": [`resolve_route_secret_ref`] and [`verify_request_signature`]
//! are the one seam both `crate::routes::webhooks::validate_routes_at_startup`
//! and the live handler call.

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use ao_protocol::assignment::{Assignment, AssignmentTrigger};

type HmacSha256 = Hmac<Sha256>;

/// The one documented HMAC bypass. A route whose resolved secret equals this
/// exact string skips signature verification entirely — but only when the
/// server is bound to loopback (see [`is_loopback_host`]). Never a default;
/// an operator must set it explicitly, and only for local testing.
pub const INSECURE_NO_AUTH_SENTINEL: &str = "INSECURE_NO_AUTH";

/// A generic-scheme signature is rejected once its `X-Webhook-Timestamp` is
/// further than this many seconds from wall-clock time, in either
/// direction. Bounds how long a captured (timestamp, signature, body)
/// triple can be replayed.
const REPLAY_WINDOW_SECS: i64 = 300;

/// Header carrying a GitHub-style signature: `sha256=<hex hmac>` over the
/// raw body.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Headers for the generic timestamped scheme: `X-Webhook-Signature` is
/// `sha256=<hex hmac>` over `"{timestamp}.{raw_body}"`, `X-Webhook-Timestamp`
/// is the signed unix-seconds timestamp.
const GENERIC_SIGNATURE_HEADER: &str = "X-Webhook-Signature";
const GENERIC_TIMESTAMP_HEADER: &str = "X-Webhook-Timestamp";

/// Delivery-id headers in preference order — the first present, non-empty
/// one wins. Matches GitHub (`X-GitHub-Delivery`), Svix-family providers
/// (`svix-id`), and the generic fallback (`X-Request-ID`).
const DELIVERY_ID_HEADERS: &[&str] = &["X-GitHub-Delivery", "svix-id", "X-Request-ID"];

/// Event-type headers in preference order — the first present, non-empty
/// one wins. `X-GitHub-Event` carries GitHub's event name (e.g.
/// `"pull_request"`); `X-Event-Type` is the generic fallback for senders
/// that aren't GitHub. Matched against a route's `events` allowlist (see
/// [`ao_protocol::webhook_filter::event_type_allowed`]).
const EVENT_TYPE_HEADERS: &[&str] = &["X-GitHub-Event", "X-Event-Type"];

/// Vault scope every webhook-route secret is looked up under, independent of
/// which agent(s) own the assignments sharing that route. A route's secret
/// is a property of the route name, not of any one assignment.
pub const WEBHOOK_SECRET_VAULT_SCOPE: &str = "__webhook_gateway__";
/// Secret role for a webhook route's HMAC signing key within
/// [`WEBHOOK_SECRET_VAULT_SCOPE`].
pub const WEBHOOK_HMAC_SECRET_ROLE: &str = "hmac_secret";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// The route resolved to no secret at all (or an empty one). Fail
    /// closed — a route with no secret configured rejects every request.
    NoSecretConfigured,
    /// The resolved secret is the [`INSECURE_NO_AUTH_SENTINEL`], but the
    /// server is not bound to loopback. The bypass never applies outside a
    /// loopback bind, regardless of what an operator configured.
    InsecureSentinelRequiresLoopback,
    /// Neither a GitHub nor a generic-scheme signature header was present.
    MissingSignature,
    /// A generic-scheme signature was present but its timestamp header was
    /// missing or not a valid integer.
    MalformedTimestamp,
    /// A generic-scheme timestamp fell outside [`REPLAY_WINDOW_SECS`] of now.
    TimestampOutOfWindow,
    /// A signature header was present and well-formed but did not match.
    InvalidSignature,
}

/// Verifies `raw_body` against whichever signature scheme `headers` carries,
/// using `secret` as the route's resolved HMAC key. `bind_is_loopback` gates
/// the [`INSECURE_NO_AUTH_SENTINEL`] escape hatch.
///
/// Fails closed: an empty secret, an unrecognized/missing signature header,
/// or a mismatched digest all return `Err`. The GitHub header is checked
/// first, then the generic timestamped scheme; a request carrying neither is
/// [`SignatureError::MissingSignature`].
pub fn verify_request_signature(
    headers: &HeaderMap,
    raw_body: &[u8],
    secret: &str,
    bind_is_loopback: bool,
    now_unix: i64,
) -> Result<(), SignatureError> {
    if secret.is_empty() {
        return Err(SignatureError::NoSecretConfigured);
    }
    if secret == INSECURE_NO_AUTH_SENTINEL {
        return if bind_is_loopback {
            Ok(())
        } else {
            Err(SignatureError::InsecureSentinelRequiresLoopback)
        };
    }

    if let Some(github_sig) = header_str(headers, GITHUB_SIGNATURE_HEADER) {
        let expected = format!("sha256={}", hmac_hex(secret.as_bytes(), raw_body));
        return if constant_time_eq(github_sig.as_bytes(), expected.as_bytes()) {
            Ok(())
        } else {
            Err(SignatureError::InvalidSignature)
        };
    }

    if let Some(generic_sig) = header_str(headers, GENERIC_SIGNATURE_HEADER) {
        let ts_str = header_str(headers, GENERIC_TIMESTAMP_HEADER)
            .ok_or(SignatureError::MalformedTimestamp)?;
        let ts: i64 = ts_str.parse().map_err(|_| SignatureError::MalformedTimestamp)?;
        if (now_unix - ts).abs() > REPLAY_WINDOW_SECS {
            return Err(SignatureError::TimestampOutOfWindow);
        }
        let mut signed = Vec::with_capacity(ts_str.len() + 1 + raw_body.len());
        signed.extend_from_slice(ts_str.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(raw_body);
        let expected = format!("sha256={}", hmac_hex(secret.as_bytes(), &signed));
        return if constant_time_eq(generic_sig.as_bytes(), expected.as_bytes()) {
            Ok(())
        } else {
            Err(SignatureError::InvalidSignature)
        };
    }

    Err(SignatureError::MissingSignature)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty())
}

fn hmac_hex(secret: &[u8], message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(message);
    format!("{:x}", mac.finalize().into_bytes())
}

/// Byte-wise constant-time equality. Hand-rolled rather than pulling in the
/// `subtle` crate (present only transitively today) for one XOR-accumulate
/// loop — the standard technique for defeating timing attacks on a
/// fixed-length secret compare.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The first non-empty delivery id found across [`DELIVERY_ID_HEADERS`], in
/// preference order.
pub fn extract_delivery_id(headers: &HeaderMap) -> Option<String> {
    DELIVERY_ID_HEADERS.iter().find_map(|name| header_str(headers, name)).map(str::to_string)
}

/// The first non-empty event type found across [`EVENT_TYPE_HEADERS`], in
/// preference order. `None` when neither header is present — a route with a
/// non-empty `events` allowlist fails closed against that (see
/// [`ao_protocol::webhook_filter::event_type_allowed`]).
pub fn extract_event_type(headers: &HeaderMap) -> Option<String> {
    EVENT_TYPE_HEADERS.iter().find_map(|name| header_str(headers, name)).map(str::to_string)
}

/// The secret-store key this route resolves to, or `None` if no assignment
/// sharing `route_name` carries one. Prefers an explicit non-empty
/// `secret_ref`; when that's absent (or blank), falls back to `route_name`
/// itself — this is what makes "the editor sets the secret under the route
/// name" (`crate::routes::webhooks::set_webhook_route_secret`) a real,
/// enforced equivalence rather than a convention the frontend has to
/// maintain by hand. When more than one assignment shares a route and their
/// resolved keys disagree, the first (in store order) wins — route-sharing
/// assignments are expected to agree on the route's secret; this only picks
/// a deterministic winner rather than erroring, since the gateway must still
/// answer requests one way or another.
pub fn resolve_route_secret_ref(route_assignments: &[Assignment]) -> Option<&str> {
    route_assignments.iter().find_map(|a| match &a.trigger {
        AssignmentTrigger::Webhook { secret_ref, route_name, .. } => secret_ref
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| route_name.as_deref().filter(|r| !r.is_empty())),
        _ => None,
    })
}

/// Hostnames/IP literals that only ever accept connections originating on
/// the same machine. Anything else — including an unset/empty host, which
/// usually means "bind every interface" — is treated as non-loopback for
/// safety-rail purposes.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "::1", "ip6-localhost", "ip6-loopback"];

/// True when `host` binds only to the local machine. Used to gate the
/// [`INSECURE_NO_AUTH_SENTINEL`] escape hatch — it is not "loopback routes
/// skip HMAC," it is "the one documented bypass requires loopback."
pub fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return false;
    }
    LOOPBACK_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(trimmed))
}

/// Environment variable naming the address `ao-server` binds to (see
/// `main.rs`). Read here too so the gateway's `INSECURE_NO_AUTH` loopback
/// gate always reflects the address the process actually bound, rather than
/// duplicating that decision from a cached/stale value.
pub const BIND_HOST_ENV_VAR: &str = "AO_BIND_HOST";

/// Default bind host when [`BIND_HOST_ENV_VAR`] is unset — matches
/// `main.rs`'s pre-existing hardcoded bind.
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";

/// Whether this process's HTTP listener is bound to loopback, resolved fresh
/// from [`BIND_HOST_ENV_VAR`] on every call (cheap — one env lookup) so a
/// direct call from a test or another code path always sees the same answer
/// the actual bind used, never a value cached at some earlier point.
pub fn server_bind_is_loopback() -> bool {
    let host = std::env::var(BIND_HOST_ENV_VAR).unwrap_or_else(|_| DEFAULT_BIND_HOST.to_string());
    is_loopback_host(&host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn github_signature_valid_passes() {
        let secret = "shh";
        let body = b"{\"action\":\"opened\"}";
        let sig = format!("sha256={}", hmac_hex(secret.as_bytes(), body));
        let h = headers(&[(GITHUB_SIGNATURE_HEADER, &sig)]);
        assert_eq!(verify_request_signature(&h, body, secret, true, 0), Ok(()));
    }

    #[test]
    fn github_signature_tampered_body_fails() {
        let secret = "shh";
        let sig = format!("sha256={}", hmac_hex(secret.as_bytes(), b"original"));
        let h = headers(&[(GITHUB_SIGNATURE_HEADER, &sig)]);
        assert_eq!(
            verify_request_signature(&h, b"tampered", secret, true, 0),
            Err(SignatureError::InvalidSignature)
        );
    }

    #[test]
    fn github_signature_wrong_secret_fails() {
        let sig = format!("sha256={}", hmac_hex(b"right-secret", b"body"));
        let h = headers(&[(GITHUB_SIGNATURE_HEADER, &sig)]);
        assert_eq!(
            verify_request_signature(&h, b"body", "wrong-secret", true, 0),
            Err(SignatureError::InvalidSignature)
        );
    }

    #[test]
    fn missing_signature_header_fails() {
        let h = headers(&[]);
        assert_eq!(
            verify_request_signature(&h, b"body", "shh", true, 0),
            Err(SignatureError::MissingSignature)
        );
    }

    #[test]
    fn empty_secret_fails_closed() {
        let h = headers(&[]);
        assert_eq!(
            verify_request_signature(&h, b"body", "", true, 0),
            Err(SignatureError::NoSecretConfigured)
        );
    }

    #[test]
    fn generic_scheme_valid_passes() {
        let secret = "shh";
        let ts = "1000";
        let body = b"payload";
        let mut signed = Vec::new();
        signed.extend_from_slice(ts.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(body);
        let sig = format!("sha256={}", hmac_hex(secret.as_bytes(), &signed));
        let h = headers(&[(GENERIC_SIGNATURE_HEADER, &sig), (GENERIC_TIMESTAMP_HEADER, ts)]);
        assert_eq!(verify_request_signature(&h, body, secret, true, 1000), Ok(()));
    }

    #[test]
    fn generic_scheme_replayed_timestamp_fails() {
        let secret = "shh";
        let ts = "1000";
        let body = b"payload";
        let mut signed = Vec::new();
        signed.extend_from_slice(ts.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(body);
        let sig = format!("sha256={}", hmac_hex(secret.as_bytes(), &signed));
        let h = headers(&[(GENERIC_SIGNATURE_HEADER, &sig), (GENERIC_TIMESTAMP_HEADER, ts)]);
        // now_unix far outside the replay window relative to ts=1000.
        assert_eq!(
            verify_request_signature(&h, body, secret, true, 10_000),
            Err(SignatureError::TimestampOutOfWindow)
        );
    }

    #[test]
    fn insecure_sentinel_allowed_on_loopback() {
        let h = headers(&[]);
        assert_eq!(
            verify_request_signature(&h, b"body", INSECURE_NO_AUTH_SENTINEL, true, 0),
            Ok(())
        );
    }

    #[test]
    fn insecure_sentinel_refused_on_non_loopback() {
        let h = headers(&[]);
        assert_eq!(
            verify_request_signature(&h, b"body", INSECURE_NO_AUTH_SENTINEL, false, 0),
            Err(SignatureError::InsecureSentinelRequiresLoopback)
        );
    }

    #[test]
    fn delivery_id_preference_order() {
        let h = headers(&[("svix-id", "svix-1"), ("X-Request-ID", "req-1")]);
        assert_eq!(extract_delivery_id(&h), Some("svix-1".to_string()));

        let h = headers(&[("X-GitHub-Delivery", "gh-1"), ("svix-id", "svix-1")]);
        assert_eq!(extract_delivery_id(&h), Some("gh-1".to_string()));

        let h = headers(&[("X-Request-ID", "req-1")]);
        assert_eq!(extract_delivery_id(&h), Some("req-1".to_string()));

        let h = headers(&[]);
        assert_eq!(extract_delivery_id(&h), None);
    }

    #[test]
    fn event_type_preference_order() {
        let h = headers(&[("X-GitHub-Event", "pull_request"), ("X-Event-Type", "generic-event")]);
        assert_eq!(extract_event_type(&h), Some("pull_request".to_string()));

        let h = headers(&[("X-Event-Type", "generic-event")]);
        assert_eq!(extract_event_type(&h), Some("generic-event".to_string()));

        let h = headers(&[]);
        assert_eq!(extract_event_type(&h), None);
    }

    #[test]
    fn loopback_host_recognition() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host(""));
        assert!(!is_loopback_host("example.com"));
    }
}
