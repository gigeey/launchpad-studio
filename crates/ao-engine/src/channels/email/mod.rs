//! Email's implementation of the channel-agnostic
//! [`crate::channels::ChannelTransport`] trait — an IMAP `UNSEEN` poll loop,
//! mirroring [`crate::telegram::transport::TelegramTransport`]'s shape.
//!
//! [`EmailTransport`] owns the [`ChannelSecretStore`] lookup for the
//! binding's password; [`imap_seam`] carries the actual IMAP I/O (behind the
//! [`imap_seam::MailSource`] seam so the poll loop is testable without a live
//! server) and message parsing; [`ingest`] formats an accepted message into
//! the text the agent sees; [`security`] decides whether a message is
//! accepted at all. Unlike Telegram, email has no automatic outbound relay —
//! a reply goes out through the `SendEmail` tool call instead, so this
//! transport only ever pushes messages in.

pub mod imap_seam;
pub mod ingest;
pub mod security;

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use ao_engine_tools_provider_config::{ChannelSecretStore, ChannelSecretStoreError, EMAIL_PASSWORD_SECRET_ROLE};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::conversation_registry::ConversationKey;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::thread::{ChannelBridgeOrigin, Thread};

use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::{submit_inbound_message, ChannelRunContext, ChannelTransport};
use crate::event_bus::EventBus;
use imap_seam::{FetchedEmail, ImapMailSource, MailSource};
use ingest::build_ingest_text;
use security::{evaluate_sender, EmailMessageMeta};

/// Fallback poll interval when a binding's `poll_secs` is unset (`0`).
const DEFAULT_POLL_SECS: u32 = 15;

/// Fixed pause after a failed IMAP poll before retrying. Keeps one unhealthy
/// inbox (bad credentials, network blip) from hammering the server or
/// spinning the task hot.
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Email's [`ChannelTransport`] implementation. One instance serves every
/// email binding on every agent — bindings are distinguished by
/// `ChannelRunContext::binding_id`, mirroring how `TelegramTransport` serves
/// every Telegram binding through one Bot API client.
pub struct EmailTransport {
    /// Opened lazily, the first time a fingerprint or spawn call actually
    /// needs a password — see [`Self::secret_store`]. An install with no
    /// email agents configured never touches the OS keychain.
    secret_store: OnceLock<ChannelSecretStore>,
}

impl EmailTransport {
    pub fn new() -> Self {
        Self { secret_store: OnceLock::new() }
    }

    fn secret_store(&self) -> Result<&ChannelSecretStore, ChannelSecretStoreError> {
        if let Some(store) = self.secret_store.get() {
            return Ok(store);
        }
        let store = ChannelSecretStore::open()?;
        // Mirrors `TelegramTransport::token_store`'s race handling: at most
        // one caller's `store` wins `set`, everyone reads it back via `get`,
        // and a losing `set` is just a discarded value, not an error.
        let _ = self.secret_store.set(store);
        Ok(self.secret_store.get().expect("secret store was just initialized above"))
    }

    /// Resolves the binding's shared IMAP/SMTP password, logging and
    /// returning `None` on any store failure or absence rather than
    /// propagating an error — callers treat "no password" as "not runnable
    /// yet", not a hard failure.
    fn resolve_password(&self, agent_id: &str, binding_id: &str) -> Option<String> {
        match self.secret_store() {
            Ok(store) => match store.get(agent_id, binding_id, EMAIL_PASSWORD_SECRET_ROLE) {
                Ok(password) => password,
                Err(e) => {
                    warn!(agent_id = %agent_id, binding_id = %binding_id, "EmailTransport: failed to read password: {e}");
                    None
                }
            },
            Err(e) => {
                warn!(agent_id = %agent_id, binding_id = %binding_id, "EmailTransport: failed to open secret store: {e}");
                None
            }
        }
    }
}

impl Default for EmailTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelTransport for EmailTransport {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Email
    }

    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String> {
        let ChannelKindConfig::Email { .. } = &binding.kind_config else {
            return None;
        };
        // Password is redacted from the fingerprint's Debug-derived tail by
        // construction: `kind_config`'s Debug output never includes it (the
        // password lives in the secret store, not on `ChannelKindConfig`).
        let password = self.resolve_password(&agent.id, &binding.binding_id)?;
        Some(format!("{password}|{:?}", binding.kind_config))
    }

    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()> {
        let password = self.resolve_password(&ctx.agent_id, &ctx.binding_id);

        tokio::spawn(async move {
            let Some(password) = password else {
                warn!(
                    agent_id = %ctx.agent_id,
                    binding_id = %ctx.binding_id,
                    "EmailTransport: password unavailable at spawn time, not starting poll task"
                );
                return;
            };
            run_email_poll_loop(ctx, password, cancel).await;
        })
    }

    /// Email has no outbound auto-relay to invalidate — a reply goes out
    /// through the `SendEmail` tool call, not a `thread_id -> reply
    /// target` mapping the way Telegram/Discord's do (see this module's
    /// doc comment). Deliberate no-op so a reader sees this was
    /// considered and intentionally skipped, not simply forgotten.
    fn invalidate_thread(&self, _thread_id: &str) {}

    /// Same reasoning as [`Self::invalidate_thread`]: no automatic
    /// outbound relay exists for email, so there is no observer task to
    /// spawn.
    fn spawn_outbound_observer(
        self: Arc<Self>,
        _persistence: Arc<PersistenceLayer>,
        _lease_gate: Arc<LeaseGate>,
        _event_bus: Arc<EventBus>,
        _shutdown_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        None
    }
}

/// IMAP poll loop for a single email binding. Runs until `cancel` fires.
/// Re-reads the agent's profile every iteration (mirroring
/// `TelegramTransport::run_bot_poll_loop`) so a mid-flight config or
/// allow-list change takes effect on the next poll without a supervisor
/// restart.
async fn run_email_poll_loop(ctx: ChannelRunContext, password: String, cancel: CancellationToken) {
    let mut mail_source: Option<ImapMailSource> = None;
    // In-session dedup safety net on top of the server's own `\Seen` flag —
    // see `FetchedEmail`'s doc for why both exist.
    let mut seen_uids: HashSet<u32> = HashSet::new();

    loop {
        let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                debug!(agent_id = %ctx.agent_id, "EmailTransport: agent no longer exists, stopping poll task");
                return;
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "EmailTransport: failed to re-read agent profile: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "EmailTransport: binding removed, stopping poll task");
            return;
        };
        if !binding.enabled {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "EmailTransport: binding disabled, stopping poll task");
            return;
        }
        // No `bridge_thread_id` readiness check here anymore: Email mints a
        // fresh per-conversation thread on demand for every distinct
        // `sender + normalized subject` pair it sees (see
        // `resolve_email_conversation_thread`) instead of routing every
        // conversation through one eagerly-provisioned thread, so this
        // binding has nothing to wait on before it can start polling. A
        // binding provisioned before this change may still carry a legacy
        // `bridge_thread_id` — its thread stays viewable, but no new inbound
        // message is ever routed there again (migration leaves it
        // as-is, never reassigned).
        let ChannelKindConfig::Email { address, imap_host, imap_port, poll_secs, require_auth_results, .. } =
            &binding.kind_config
        else {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "EmailTransport: binding kind_config is not Email, stopping poll task");
            return;
        };
        let poll_interval =
            Duration::from_secs(if *poll_secs == 0 { DEFAULT_POLL_SECS } else { *poll_secs } as u64);
        let allowed_senders = match ctx
            .persistence
            .linked_senders
            .get_or_backfill(&ctx.agent_id, &ctx.binding_id, &binding.allowed_senders)
            .await
        {
            Ok(senders) => senders,
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "EmailTransport: failed to read linked senders: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        let require_auth_results = *require_auth_results;

        if mail_source.is_none() {
            mail_source =
                Some(ImapMailSource::new(imap_host.clone(), *imap_port, address.clone(), password.clone()));
        }
        let source = mail_source.as_mut().expect("just ensured Some above");

        let fetched = tokio::select! {
            _ = cancel.cancelled() => return,
            result = source.fetch_unseen() => result,
        };

        let emails = match fetched {
            Ok(emails) => {
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                emails
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "EmailTransport: poll failed: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };

        for email in emails {
            // Build the ingest text up front so the delivery closure can
            // borrow it across the awaited hand-off; `ingest_one_email` only
            // ever invokes the closure for a message that clears dedup and the
            // security check.
            let ingest_text = build_ingest_text(&email);
            let auto_title_candidate = auto_title_candidate_from_subject(email.subject.as_deref());
            let _outcome = ingest_one_email(
                &mut *source,
                &mut seen_uids,
                &email,
                &allowed_senders,
                require_auth_results,
                &ctx.agent_id,
                &ctx.binding_id,
                || async {
                    let Some(thread_id) = resolve_email_conversation_thread(
                        &ctx,
                        &email,
                        auto_title_candidate.as_deref(),
                        Utc::now(),
                    )
                    .await
                    else {
                        return Err(AoError::Internal(
                            "EmailTransport: failed to resolve a per-conversation bridge thread".to_string(),
                        ));
                    };
                    submit_inbound_message(
                        &ctx,
                        &profile,
                        &thread_id,
                        ChannelKind::Email,
                        &email.from_address,
                        &email.from_address,
                        email.from_display.as_deref(),
                        &ingest_text,
                        auto_title_candidate,
                    )
                    .await
                },
            )
            .await;
        }

        if wait_or_cancelled(&cancel, poll_interval).await {
            return;
        }
    }
}

/// Resolves the conversation→thread registry row for one inbound
/// email, lazily minting a fresh Launchpad bridge thread on first contact —
/// email's analogue of `discord::runner::resolve_discord_conversation_thread`
/// / `telegram::transport::resolve_telegram_conversation_thread`, but
/// **inbound-routing-only**: unlike those two, this never touches
/// `LeaseGate` (no `mark_active`, no `conversation_gc::run_gc_and_release_leases`
/// call). Email has no automatic outbound relay for `LeaseGate` to gate —
/// `SendEmail` replies via the model's own `in_reply_to_message_id`, not a
/// thread-id-keyed relay (see this module's doc comment and
/// [`EmailTransport::invalidate_thread`]) — so there is nothing here for
/// `LeaseGate` to protect. The registry's own idle/cap eviction still runs
/// (via [`ao_persistence::ConversationRegistryStore::get_or_create`]'s
/// internal GC pass), it just never needs to release a `LeaseGate` entry
/// that was never set in the first place.
///
/// Keyed on `lower(sender) + "::" + normalized_subject`:
/// sender is non-negotiable — subject alone would merge two strangers who
/// both send e.g. "Hi" into one thread, leaking each other's content.
/// `normalized_subject` is [`auto_title_candidate_from_subject`]'s output
/// (already trimmed, with a single leading `Re:`/`Fwd:`/`Fw:` stripped), so
/// a reply lands in the same thread as its original by construction — full
/// `References`/`In-Reply-To` header linking is deferred.
///
/// Returns `None` (logged) only on a persistence failure — the caller
/// ([`ingest_one_email`], via its `deliver` closure's `Err`) leaves such a
/// message `\Unseen` so it is retried on a later poll rather than mis-routed.
async fn resolve_email_conversation_thread(
    ctx: &ChannelRunContext,
    email: &FetchedEmail,
    normalized_subject: Option<&str>,
    now: DateTime<Utc>,
) -> Option<String> {
    let key =
        ConversationKey::new(format!("{}::{}", email.from_address.to_lowercase(), normalized_subject.unwrap_or("")));
    let mut minted_thread: Option<Thread> = None;
    let mint = || {
        let mut thread = ctx.persistence.threads.build_fresh_thread(&ctx.agent_id, None);
        thread.channel_origin = Some(ChannelBridgeOrigin { kind: ChannelKind::Email, binding_id: ctx.binding_id.clone() });
        let id = thread.id.clone();
        minted_thread = Some(thread);
        id
    };

    let row = match ctx
        .persistence
        .conversation_registry
        .get_or_create(&ctx.agent_id, &ctx.binding_id, key, now, mint)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "EmailTransport: failed to read the conversation registry: {e}");
            return None;
        }
    };

    if let Some(thread) = minted_thread {
        if let Err(e) = ctx.persistence.threads.create(thread.clone()).await {
            warn!(agent_id = %ctx.agent_id, "EmailTransport: failed to create a per-conversation bridge thread: {e}");
            return None;
        }
        ctx.event_bus
            .emit(
                &format!("thread:{}", thread.id),
                &ctx.agent_id,
                Some(thread.id.clone()),
                AgentEventPayload::ThreadCreated { thread },
            )
            .await;
    }

    Some(row.thread_id)
}

/// The three ways [`ingest_one_email`] can end. Returned so the poll loop (for
/// logging) and the tests can tell the paths apart by value rather than by
/// observing side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestOutcome {
    /// Passed the security check and was handed off; now marked `\Seen`.
    Delivered,
    /// Skipped by in-session dedup or rejected by the security check; left
    /// `\Unseen` so it can be re-polled.
    Dropped,
    /// Passed the security check but the hand-off failed; left `\Unseen` to
    /// retry on a later poll.
    DeliveryFailed,
}

/// Runs one fetched message through the full inbound path: in-session dedup,
/// the allow-list / auth-results security check, hand-off via `deliver`, and —
/// **only on a successful hand-off** — marking it `\Seen` on the server via
/// [`MailSource::mark_seen`].
///
/// Marking `\Seen` last is the entire point of this channel's read-flag
/// handling: a message dropped by dedup or the security check, or one whose
/// hand-off fails, is never marked read, so it stays `\Unseen` and is
/// re-polled instead of being silently lost from the user's inbox. A
/// `mark_seen` failure *after* a good hand-off is logged but not fatal — the
/// message is already delivered, so worst case it reappears on the next poll
/// and the in-session dedup set absorbs it (delivery beats flag accuracy).
async fn ingest_one_email<M, D, Fut>(
    source: &mut M,
    seen_uids: &mut HashSet<u32>,
    email: &FetchedEmail,
    allowed_senders: &[String],
    require_auth_results: bool,
    agent_id: &str,
    binding_id: &str,
    deliver: D,
) -> IngestOutcome
where
    M: MailSource,
    D: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), AoError>>,
{
    // Insert regardless of the message's fate: the in-session dedup set exists
    // so a still-`\Unseen` message a poll declined isn't re-evaluated every
    // cycle within this process's lifetime.
    if !seen_uids.insert(email.uid) {
        return IngestOutcome::Dropped;
    }
    if let Err(reason) = evaluate_meta(email, allowed_senders, require_auth_results) {
        debug!(
            agent_id = %agent_id,
            binding_id = %binding_id,
            from = %email.from_address,
            reason = ?reason,
            "EmailTransport: dropping inbound message"
        );
        return IngestOutcome::Dropped;
    }

    if let Err(e) = deliver().await {
        warn!(agent_id = %agent_id, "EmailTransport: failed to deliver inbound message: {e}");
        // Leave the message `\Unseen` so a later poll retries the hand-off.
        return IngestOutcome::DeliveryFailed;
    }

    // Hand-off confirmed: now, and only now, commit the read flag.
    if let Err(e) = source.mark_seen(email.uid).await {
        warn!(
            agent_id = %agent_id,
            binding_id = %binding_id,
            uid = email.uid,
            "EmailTransport: delivered inbound message but failed to mark it \\Seen: {e}"
        );
    }
    IngestOutcome::Delivered
}

/// Turns a raw `Subject:` header into the `auto_title_candidate` passed to
/// [`submit_inbound_message`] — the email's own natural, human-written title,
/// as opposed to its body text. Trims surrounding whitespace and strips a
/// single leading `Re:`/`Fwd:`/`Fw:` prefix (case-insensitive) so a reply
/// thread titles from the original subject rather than an accumulating
/// "Re: Re: Re: ..." chain. Returns `None` for a missing or
/// empty/whitespace-only subject, which [`submit_inbound_message`] treats as
/// "no candidate" — the thread then falls back to the channel-kind label in
/// the UI, which is correct, not a bug.
fn auto_title_candidate_from_subject(subject: Option<&str>) -> Option<String> {
    let trimmed = subject?.trim();
    if trimmed.is_empty() {
        return None;
    }
    const REPLY_FORWARD_PREFIXES: [&str; 3] = ["re:", "fwd:", "fw:"];
    for prefix in REPLY_FORWARD_PREFIXES {
        if let Some(rest) = trimmed.get(..prefix.len()) {
            if rest.eq_ignore_ascii_case(prefix) {
                let stripped = trimmed[prefix.len()..].trim_start();
                return if stripped.is_empty() { None } else { Some(stripped.to_string()) };
            }
        }
    }
    Some(trimmed.to_string())
}

fn evaluate_meta(
    email: &FetchedEmail,
    allowed_senders: &[String],
    require_auth_results: bool,
) -> Result<(), security::DenyReason> {
    let meta = EmailMessageMeta {
        from_address: &email.from_address,
        authentication_results: &email.authentication_results,
        auto_submitted: email.auto_submitted.as_deref(),
        precedence: email.precedence.as_deref(),
        list_unsubscribe_present: email.list_unsubscribe_present,
        x_auto_response_suppress_present: email.x_auto_response_suppress_present,
    };
    evaluate_sender(&meta, allowed_senders, require_auth_results)
}

/// Sleeps for `dur` unless `cancel` fires first. Returns `true` if cancelled.
async fn wait_or_cancelled(cancel: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use imap_seam::MailSourceError;

    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, AgentRunnerMode, RunComplete, RunnerDispatcher};
    use crate::channels::connection_state::ConnectionStateRegistry;
    use crate::instance_registry::InstanceRegistry;
    use crate::queue_manager::QueueManagerRegistry;

    /// Fake [`MailSource`] that records every `mark_seen` call instead of
    /// touching a live IMAP server, so a test can assert exactly which UIDs
    /// (if any) got their `\Seen` flag set. `fetch_unseen` is unused here —
    /// these tests drive [`ingest_one_email`] directly, and the peek-fetch
    /// behavior is covered by `imap_seam`'s own const tests.
    #[derive(Default)]
    struct RecordingMailSource {
        mark_seen_calls: Vec<u32>,
    }

    #[async_trait]
    impl MailSource for RecordingMailSource {
        async fn fetch_unseen(&mut self) -> Result<Vec<FetchedEmail>, MailSourceError> {
            Ok(Vec::new())
        }

        async fn mark_seen(&mut self, uid: u32) -> Result<(), MailSourceError> {
            self.mark_seen_calls.push(uid);
            Ok(())
        }
    }

    /// Builds a plain, non-bulk message so the security check's automated/bulk
    /// filter doesn't reject it before the allow-list check under test.
    fn make_email(uid: u32, from: &str) -> FetchedEmail {
        FetchedEmail {
            uid,
            from_address: from.to_string(),
            from_display: None,
            to: vec!["agent@example.org".to_string()],
            cc: vec![],
            subject: Some("hello".to_string()),
            date: None,
            message_id: None,
            authentication_results: vec![],
            auto_submitted: None,
            precedence: None,
            list_unsubscribe_present: false,
            x_auto_response_suppress_present: false,
            body_text: "hi".to_string(),
        }
    }

    #[tokio::test]
    async fn accepted_message_is_marked_seen_exactly_once_for_its_own_uid() {
        let mut source = RecordingMailSource::default();
        let mut seen = HashSet::new();
        let email = make_email(42, "allowed@example.com");
        let allowed = vec!["allowed@example.com".to_string()];

        let outcome = ingest_one_email(
            &mut source,
            &mut seen,
            &email,
            &allowed,
            false,
            "agent",
            "binding",
            || async { Ok::<(), AoError>(()) },
        )
        .await;

        assert_eq!(outcome, IngestOutcome::Delivered);
        assert_eq!(
            source.mark_seen_calls,
            vec![42],
            "an accepted + delivered message must be marked \\Seen exactly once, for its own UID"
        );
    }

    #[tokio::test]
    async fn message_dropped_by_empty_allow_list_is_never_marked_seen() {
        let mut source = RecordingMailSource::default();
        let mut seen = HashSet::new();
        let email = make_email(7, "someone@example.com");
        // Fail-closed: an empty allow-list rejects every sender.
        let allowed: Vec<String> = vec![];

        let outcome = ingest_one_email(
            &mut source,
            &mut seen,
            &email,
            &allowed,
            false,
            "agent",
            "binding",
            || async { Ok::<(), AoError>(()) },
        )
        .await;

        assert_eq!(outcome, IngestOutcome::Dropped);
        assert!(
            source.mark_seen_calls.is_empty(),
            "a message dropped by the empty allow-list must stay \\Unseen so it is re-polled"
        );
    }

    #[tokio::test]
    async fn message_from_a_disallowed_sender_is_never_marked_seen() {
        let mut source = RecordingMailSource::default();
        let mut seen = HashSet::new();
        let email = make_email(8, "stranger@example.com");
        let allowed = vec!["trusted@example.com".to_string()];

        let outcome = ingest_one_email(
            &mut source,
            &mut seen,
            &email,
            &allowed,
            false,
            "agent",
            "binding",
            || async { Ok::<(), AoError>(()) },
        )
        .await;

        assert_eq!(outcome, IngestOutcome::Dropped);
        assert!(source.mark_seen_calls.is_empty(), "a disallowed sender must stay \\Unseen");
    }

    #[tokio::test]
    async fn message_whose_hand_off_fails_is_never_marked_seen() {
        let mut source = RecordingMailSource::default();
        let mut seen = HashSet::new();
        let email = make_email(9, "allowed@example.com");
        let allowed = vec!["allowed@example.com".to_string()];

        let outcome = ingest_one_email(
            &mut source,
            &mut seen,
            &email,
            &allowed,
            false,
            "agent",
            "binding",
            || async { Err::<(), AoError>(AoError::Internal("hand-off failed".to_string())) },
        )
        .await;

        assert_eq!(outcome, IngestOutcome::DeliveryFailed);
        assert!(
            source.mark_seen_calls.is_empty(),
            "a message that passed security but failed hand-off must stay \\Unseen so it is retried"
        );
    }

    #[tokio::test]
    async fn dedup_suppressed_message_short_circuits_without_marking_seen() {
        let mut source = RecordingMailSource::default();
        let mut seen = HashSet::new();
        // Simulate this UID already having been processed this session.
        seen.insert(5);
        let email = make_email(5, "allowed@example.com");
        let allowed = vec!["allowed@example.com".to_string()];

        let outcome = ingest_one_email(
            &mut source,
            &mut seen,
            &email,
            &allowed,
            false,
            "agent",
            "binding",
            || async { Ok::<(), AoError>(()) },
        )
        .await;

        assert_eq!(outcome, IngestOutcome::Dropped);
        assert!(
            source.mark_seen_calls.is_empty(),
            "a message suppressed by the in-session dedup set must not be (re-)marked \\Seen"
        );
    }

    // --- `auto_title_candidate_from_subject` ---

    #[test]
    fn auto_title_candidate_trims_and_passes_through_a_plain_subject() {
        assert_eq!(
            auto_title_candidate_from_subject(Some("  Please help with the deploy  ")),
            Some("Please help with the deploy".to_string())
        );
    }

    #[test]
    fn auto_title_candidate_is_none_for_a_missing_or_blank_subject() {
        assert_eq!(auto_title_candidate_from_subject(None), None);
        assert_eq!(auto_title_candidate_from_subject(Some("")), None);
        assert_eq!(auto_title_candidate_from_subject(Some("   ")), None);
    }

    #[test]
    fn auto_title_candidate_strips_a_single_leading_reply_or_forward_prefix_case_insensitively() {
        assert_eq!(
            auto_title_candidate_from_subject(Some("Re: quarterly numbers")),
            Some("quarterly numbers".to_string())
        );
        assert_eq!(
            auto_title_candidate_from_subject(Some("RE: quarterly numbers")),
            Some("quarterly numbers".to_string())
        );
        assert_eq!(
            auto_title_candidate_from_subject(Some("Fwd: quarterly numbers")),
            Some("quarterly numbers".to_string())
        );
        assert_eq!(
            auto_title_candidate_from_subject(Some("fw:quarterly numbers")),
            Some("quarterly numbers".to_string())
        );
        // Only the one leading prefix is stripped, not a whole Re:-chain.
        assert_eq!(
            auto_title_candidate_from_subject(Some("Re: Re: quarterly numbers")),
            Some("Re: quarterly numbers".to_string())
        );
    }

    #[test]
    fn auto_title_candidate_leaves_an_unprefixed_subject_untouched() {
        assert_eq!(
            auto_title_candidate_from_subject(Some("reminder: submit your timesheet")),
            Some("reminder: submit your timesheet".to_string())
        );
    }

    // --- Auto-title wiring through `submit_inbound_message`: the SUBJECT
    // drives the title, never the body, and only the first inbound message
    // on a bridge thread can ever set it. Mirrors the structure of
    // `slack::runner::tests::fresh_thread_creation_sets_auto_title_and_a_later_message_does_not_overwrite_it`. ---

    const TITLE_TEST_AGENT_ID: &str = "agent-email-title-test";
    const TITLE_TEST_BINDING_ID: &str = "email";

    struct TitleTestHarness {
        persistence: Arc<ao_persistence::PersistenceLayer>,
        event_bus: Arc<EventBus>,
        queue_registry: Arc<QueueManagerRegistry>,
        connection_state: Arc<ConnectionStateRegistry>,
        lease_gate: Arc<LeaseGate>,
        _tmp: tempfile::TempDir,
    }

    impl TitleTestHarness {
        fn ctx(&self) -> ChannelRunContext {
            ChannelRunContext {
                agent_id: TITLE_TEST_AGENT_ID.to_string(),
                binding_id: TITLE_TEST_BINDING_ID.to_string(),
                persistence: Arc::clone(&self.persistence),
                queue_registry: Arc::clone(&self.queue_registry),
                connection_state: Arc::clone(&self.connection_state),
                lease_gate: Arc::clone(&self.lease_gate),
                event_bus: Arc::clone(&self.event_bus),
            }
        }
    }

    /// A stub [`AgentRunner`] that completes immediately without spawning a
    /// real process — same pattern `channels::mod`'s, `slack::runner`'s and
    /// `discord::runner`'s own test modules use so `submit_inbound_message`'s
    /// real `QueueManagerRegistry` path can run inside a unit test.
    struct TitleStubRunner;

    #[async_trait]
    impl AgentRunner for TitleStubRunner {
        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }

        async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
            let run_id = req.pre_registered_run_id.unwrap_or_else(|| "test-run".to_string());
            let rc = RunComplete {
                run_id,
                output_text: "ok".to_string(),
                workflow_followups: vec![],
                end_reason: RunEndReason::Completed,
            };
            let _ = req.run_complete_tx.send(rc.clone()).await;
            Ok(rc)
        }
    }

    async fn make_title_test_harness(agent: AgentProfile) -> TitleTestHarness {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let persistence =
            Arc::new(ao_persistence::PersistenceLayer::init_with_root(data_root).await.expect("init persistence"));
        persistence.agents.create(&agent).await.expect("create agent");

        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(InstanceRegistry::new());
        let runner: Arc<dyn AgentRunner> = Arc::new(TitleStubRunner);
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(Arc::clone(&runner), runner));
        let queue_registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            instance_registry,
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));
        let connection_state = Arc::new(ConnectionStateRegistry::new());
        let lease_gate = Arc::new(LeaseGate::new());

        TitleTestHarness { persistence, event_bus, queue_registry, connection_state, lease_gate, _tmp: tmp }
    }

    fn make_title_test_agent() -> AgentProfile {
        AgentProfile {
            id: TITLE_TEST_AGENT_ID.to_string(),
            name: "Email Title Test Agent".to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: Default::default(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: Default::default(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    /// Same field shape as [`make_email`] above, but with a caller-chosen
    /// subject and a body that's deliberately unrelated to it, so a test
    /// asserting the derived title came from the subject can't pass by
    /// accident if the wiring regressed to reading the body instead.
    fn make_email_with_subject(uid: u32, subject: Option<&str>) -> FetchedEmail {
        FetchedEmail {
            uid,
            from_address: "sender@example.com".to_string(),
            from_display: None,
            to: vec!["agent@example.org".to_string()],
            cc: vec![],
            subject: subject.map(str::to_string),
            date: None,
            message_id: None,
            authentication_results: vec![],
            auto_submitted: None,
            precedence: None,
            list_unsubscribe_present: false,
            x_auto_response_suppress_present: false,
            body_text: "this is the body text, not the subject".to_string(),
        }
    }

    /// Delivers `email` through the same call `run_email_poll_loop` makes:
    /// `submit_inbound_message` with `auto_title_candidate_from_subject`
    /// applied to the parsed `Subject:` header.
    async fn deliver_for_title_test(
        ctx: &ChannelRunContext,
        agent: &AgentProfile,
        bridge_thread_id: &str,
        email: &FetchedEmail,
    ) {
        let ingest_text = build_ingest_text(email);
        submit_inbound_message(
            ctx,
            agent,
            bridge_thread_id,
            ChannelKind::Email,
            &email.from_address,
            &email.from_address,
            email.from_display.as_deref(),
            &ingest_text,
            auto_title_candidate_from_subject(email.subject.as_deref()),
        )
        .await
        .expect("inbound message delivers");
    }

    #[tokio::test]
    async fn fresh_email_thread_auto_titles_from_subject_not_body_and_a_later_email_does_not_overwrite_it() {
        let harness = make_title_test_harness(make_title_test_agent()).await;
        let ctx = harness.ctx();
        let agent = ctx.persistence.agents.get(TITLE_TEST_AGENT_ID).await.unwrap().expect("agent exists");

        let fresh = ctx.persistence.threads.build_fresh_thread(TITLE_TEST_AGENT_ID, None);
        let thread = ctx.persistence.threads.create(fresh).await.expect("create bridge thread");

        let first = make_email_with_subject(1, Some("Please help with the deploy"));
        deliver_for_title_test(&ctx, &agent, &thread.id, &first).await;

        let after_first = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert_eq!(
            after_first.auto_title.as_deref(),
            Some("Please help with the deploy"),
            "auto_title must come from the Subject header, not the body text"
        );
        assert!(after_first.title.is_none(), "a fresh bridge thread must stay renamable (title unset)");

        let second = make_email_with_subject(2, Some("totally unrelated follow-up subject"));
        deliver_for_title_test(&ctx, &agent, &thread.id, &second).await;

        let after_second = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert_eq!(
            after_second.auto_title.as_deref(),
            Some("Please help with the deploy"),
            "a later email on the same thread must never overwrite the auto_title set from the first one"
        );
    }

    #[tokio::test]
    async fn empty_subject_email_yields_no_auto_title() {
        let harness = make_title_test_harness(make_title_test_agent()).await;
        let ctx = harness.ctx();
        let agent = ctx.persistence.agents.get(TITLE_TEST_AGENT_ID).await.unwrap().expect("agent exists");

        let fresh = ctx.persistence.threads.build_fresh_thread(TITLE_TEST_AGENT_ID, None);
        let thread = ctx.persistence.threads.create(fresh).await.expect("create bridge thread");

        let email = make_email_with_subject(1, Some("   "));
        deliver_for_title_test(&ctx, &agent, &thread.id, &email).await;

        let after = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert!(
            after.auto_title.is_none(),
            "a blank subject must never set auto_title — the FE falls back to the channel-kind label"
        );
    }

    // --- `resolve_email_conversation_thread`: the per-conversation routing
    // key is `lower(sender) + "::" + normalized_subject`. ---

    /// Same field shape as [`make_email_with_subject`], but with a
    /// caller-chosen sender and body too, so the isolate-across test can put
    /// a distinguishing secret in the body (what actually ends up in the
    /// transcript via `build_ingest_text`) while two different senders share
    /// the exact same subject.
    fn make_email_with_sender_and_subject(uid: u32, from_address: &str, subject: Option<&str>, body_text: &str) -> FetchedEmail {
        FetchedEmail {
            uid,
            from_address: from_address.to_string(),
            from_display: None,
            to: vec!["agent@example.org".to_string()],
            cc: vec![],
            subject: subject.map(str::to_string),
            date: None,
            message_id: None,
            authentication_results: vec![],
            auto_submitted: None,
            precedence: None,
            list_unsubscribe_present: false,
            x_auto_response_suppress_present: false,
            body_text: body_text.to_string(),
        }
    }

    /// Runs `email` through the exact same two-step call
    /// `run_email_poll_loop`'s delivery closure makes: resolve the
    /// per-conversation thread, then hand off through `submit_inbound_message`.
    /// Returns the resolved thread id so a test can assert on it directly.
    async fn deliver_email_for_resolve_test(ctx: &ChannelRunContext, agent: &AgentProfile, email: &FetchedEmail) -> String {
        let auto_title_candidate = auto_title_candidate_from_subject(email.subject.as_deref());
        let ingest_text = build_ingest_text(email);
        let thread_id = resolve_email_conversation_thread(ctx, email, auto_title_candidate.as_deref(), Utc::now())
            .await
            .expect("resolve succeeds");
        submit_inbound_message(
            ctx,
            agent,
            &thread_id,
            ChannelKind::Email,
            &email.from_address,
            &email.from_address,
            email.from_display.as_deref(),
            &ingest_text,
            auto_title_candidate,
        )
        .await
        .expect("inbound message delivers");
        thread_id
    }

    /// ISOLATE-ACROSS: two
    /// strangers emailing the exact same subject must never land in the same
    /// thread — subject-only keying would leak one stranger's content to the
    /// other. Sender is the non-negotiable part of the key.
    #[tokio::test]
    async fn isolate_across_different_senders_with_the_same_subject_mint_distinct_threads_with_no_shared_context() {
        let harness = make_title_test_harness(make_title_test_agent()).await;
        let ctx = harness.ctx();
        let agent = ctx.persistence.agents.get(TITLE_TEST_AGENT_ID).await.unwrap().expect("agent exists");

        let joans_secret = "joans-secret-token-13579";
        let joan = make_email_with_sender_and_subject(1, "joan@example.com", Some("Hi"), joans_secret);
        let joan_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &joan).await;

        let mathew = make_email_with_sender_and_subject(2, "mathew@example.com", Some("Hi"), "hey, what's up?");
        let mathew_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &mathew).await;

        assert_ne!(
            joan_thread_id, mathew_thread_id,
            "two different senders emailing the exact same subject must never share a thread"
        );

        let mathews_thread =
            ctx.persistence.threads.get(&mathew_thread_id).await.expect("read thread").expect("thread exists");
        let mathews_transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&mathews_thread.transcript_path))
            .await
            .expect("read mathew's transcript");
        assert!(
            !mathews_transcript.iter().any(|entry| entry.content.contains(joans_secret)),
            "mathew's thread must never carry joan's message content"
        );
    }

    /// SHARE-WITHIN: repeated emails from the same sender
    /// with the same subject resolve to the same thread, and only the first
    /// message's subject ever seeds `auto_title`.
    #[tokio::test]
    async fn share_within_same_sender_and_subject_reuses_the_same_thread_and_sets_auto_title_once() {
        let harness = make_title_test_harness(make_title_test_agent()).await;
        let ctx = harness.ctx();
        let agent = ctx.persistence.agents.get(TITLE_TEST_AGENT_ID).await.unwrap().expect("agent exists");

        let first =
            make_email_with_sender_and_subject(1, "sender@example.com", Some("Please help with the deploy"), "first message body");
        let first_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &first).await;

        let second = make_email_with_sender_and_subject(
            2,
            "sender@example.com",
            Some("Please help with the deploy"),
            "totally unrelated follow-up body",
        );
        let second_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &second).await;

        assert_eq!(
            first_thread_id, second_thread_id,
            "the same sender + subject must reuse the same per-conversation thread"
        );

        let thread = ctx.persistence.threads.get(&first_thread_id).await.expect("read thread").expect("thread exists");
        assert_eq!(
            thread.auto_title.as_deref(),
            Some("Please help with the deploy"),
            "a later email on the same thread must never overwrite the auto_title set from the first one"
        );
    }

    /// A `Re:`/`Fwd:` reply from the same sender must resolve to the same
    /// thread as the original — proves the subject normalizer
    /// (`auto_title_candidate_from_subject`) is actually wired into the
    /// conversation key, not just into auto-titling.
    #[tokio::test]
    async fn a_reply_from_the_same_sender_with_a_re_prefixed_subject_matches_the_original_thread() {
        let harness = make_title_test_harness(make_title_test_agent()).await;
        let ctx = harness.ctx();
        let agent = ctx.persistence.agents.get(TITLE_TEST_AGENT_ID).await.unwrap().expect("agent exists");

        let original =
            make_email_with_sender_and_subject(1, "sender@example.com", Some("quarterly numbers"), "original message");
        let original_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &original).await;

        let reply =
            make_email_with_sender_and_subject(2, "sender@example.com", Some("Re: quarterly numbers"), "reply body");
        let reply_thread_id = deliver_email_for_resolve_test(&ctx, &agent, &reply).await;

        assert_eq!(
            original_thread_id, reply_thread_id,
            "a Re: reply from the same sender must match the original thread via the subject normalizer"
        );
    }
}
