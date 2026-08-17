//! Channel-agnostic abstraction over "a running inbound binding."
//!
//! [`ChannelTransport`] is the seam a messaging channel (Telegram, a future
//! IMAP-polled email inbox, ...) plugs into so the supervisor
//! ([`crate::telegram::ChannelBridge`]) never needs to know about any
//! channel's secrets (bot tokens, IMAP passwords, ...) or its inbound
//! transport shape (long-poll vs. gateway push vs. IMAP `UNSEEN` poll). The
//! supervisor only ever sees an opaque [`ChannelTransport::fingerprint`]
//! string for change detection and a [`ChannelTransport::spawn`] handle it
//! starts and cancels.
//!
//! [`submit_inbound_message`] is the shared delivery path every transport's
//! inbound loop calls once it has accepted a message: it writes the message
//! into the binding's dedicated bridge thread and submits it to the agent's
//! queue exactly like a typed chat turn, tagged with a channel-agnostic
//! [`MessageSource::Channel`].

pub mod connection_state;
pub mod discord;
pub mod email;
pub(crate) mod relay;
pub mod slack;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::message::QueuedMessage;
use ao_protocol::scheduled_task::MessageSource;
use ao_protocol::thread::derive_auto_title;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::event_bus::EventBus;
use crate::queue_manager::QueueManagerRegistry;
use connection_state::ConnectionStateRegistry;
use relay::lease_gate::LeaseGate;

/// Everything a [`ChannelTransport::spawn`] inbound loop needs to submit a
/// message through the normal agent pump. Deliberately carries no
/// channel-specific secrets or config — a transport already has those from
/// its own fields (e.g. a token store) — only the shared services and
/// identifiers every channel kind needs.
pub struct ChannelRunContext {
    pub agent_id: String,
    pub binding_id: String,
    pub persistence: Arc<PersistenceLayer>,
    pub queue_registry: Arc<QueueManagerRegistry>,
    /// Where a transport reports its own connect/backoff transitions — see
    /// [`ConnectionStateRegistry`]'s doc for who else
    /// writes to it and how `GET /agents/{id}/channels` reads it back.
    pub connection_state: Arc<ConnectionStateRegistry>,
    /// The same [`LeaseGate`] instance `ChannelBridge::reconcile` still
    /// marks active for Slack's placeholder `bridge_thread_id`. Discord and
    /// Telegram's inbound dispatch each use it directly to register every
    /// per-conversation thread id they resolve or mint (see
    /// `crate::channels::discord::runner::resolve_discord_conversation_thread`
    /// and `crate::telegram::transport::resolve_telegram_conversation_thread`),
    /// since those ids never pass through `reconcile` at all — Slack's own
    /// inbound dispatch (`crate::channels::slack::runner::resolve_bridge_thread`)
    /// registers its real per-conversation threads the same way, on top of
    /// `reconcile`'s unrelated placeholder marking. Email is exempt from
    /// `reconcile`'s `bridge_thread_id` requirement too (it also mints
    /// on-demand — `crate::channels::email::resolve_email_conversation_thread`)
    /// but deliberately never touches this field at all: email has no
    /// automatic outbound relay for `LeaseGate` to gate.
    pub(crate) lease_gate: Arc<LeaseGate>,
    /// Shared with [`crate::telegram::ChannelBridge`] itself — Discord,
    /// Telegram, and Slack's inbound dispatch each emit `ThreadCreated` on
    /// this bus the moment they mint a brand-new per-conversation thread
    /// (see `crate::channels::discord::runner::resolve_discord_conversation_thread`,
    /// `crate::telegram::transport::resolve_telegram_conversation_thread`,
    /// and `crate::channels::slack::runner::resolve_bridge_thread`), so an
    /// already-open SSE stream learns about it immediately rather than
    /// waiting for the next full thread-list refetch.
    pub event_bus: Arc<EventBus>,
}

/// One channel kind's inbound implementation — e.g. a Telegram long-poll
/// loop, or (later) an IMAP `UNSEEN` poll loop for email. The supervisor
/// holds one of these per registered [`ChannelKind`] and never touches a
/// transport's internals directly. `pub(crate)`, not `pub`: every
/// implementation lives inside this crate (Telegram/Discord/Email today),
/// and [`Self::spawn_outbound_observer`] takes the crate-internal
/// [`crate::channels::relay::lease_gate::LeaseGate`], which would otherwise
/// leak a `pub(crate)` type through a `pub` trait.
#[async_trait]
pub(crate) trait ChannelTransport: Send + Sync {
    /// Which [`ChannelKind`] this transport serves. Used by the supervisor
    /// to pick a transport for each binding it finds on an agent profile.
    fn kind(&self) -> ChannelKind;

    /// Folds `binding`'s config and its resolved secret(s) (bot token, IMAP
    /// password, ...) into an opaque change-detection string. Returns
    /// `None` when the binding isn't runnable yet — e.g. no secret is on
    /// file for this agent — so the supervisor skips it until it becomes
    /// resolvable. A fingerprint that differs from the currently-running
    /// task's tells the supervisor the binding was reconfigured or its
    /// secret rotated, and the task should be restarted.
    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String>;

    /// Runs this binding's inbound loop until `cancel` fires. Implementors
    /// should re-read the agent's profile as needed (mirroring the
    /// queue-manager pump's re-read-before-dispatch pattern) so a
    /// mid-flight config change takes effect on the transport's own cadence
    /// without a supervisor restart, and call [`submit_inbound_message`]
    /// for each inbound message they accept.
    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()>;

    /// Ends this transport's outbound-relay binding for `thread_id`
    /// outright — e.g. drops whatever `thread_id -> reply target` mapping
    /// this kind keeps, so a later run on this thread (a stray delegate
    /// completion, most commonly) has nothing left to relay to. Called by
    /// the supervisor whenever a binding is torn down — disabled,
    /// reconfigured, its lease lost, or removed — regardless of which
    /// kind's binding it was (see
    /// [`crate::telegram::bridge::ChannelBridge::invalidate_thread`]),
    /// which is why this lives on the trait rather than as a per-kind
    /// method the supervisor would have to know to call. A transport with
    /// no outbound relay to invalidate (email, which delivers replies via
    /// a `SendEmail` tool call instead of an automatic relay) implements
    /// this as a deliberate no-op rather than omitting it, so the absence
    /// is visible in the trait impl instead of silently missing.
    fn invalidate_thread(&self, thread_id: &str);

    /// Starts this transport's outbound-relay observer — the process-wide
    /// task that relays a finished turn's reply back out over this
    /// channel kind — if it has one. Returns `None` for a transport with
    /// nothing to spawn (email, again, since it has no automatic relay);
    /// the supervisor still starts/stops that transport's per-binding
    /// inbound task independently either way. Takes `self: Arc<Self>`
    /// rather than `&self` because the spawned task must own a
    /// long-lived clone of the transport for its own lifetime, exactly
    /// like [`Self::spawn`]'s inbound task does via its own `Arc::clone`
    /// of transport-internal state.
    fn spawn_outbound_observer(
        self: Arc<Self>,
        persistence: Arc<PersistenceLayer>,
        lease_gate: Arc<LeaseGate>,
        event_bus: Arc<EventBus>,
        shutdown_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>>;
}

/// One transport implementation per [`ChannelKind`] the process knows how
/// to run. The supervisor looks up a binding's transport by `binding.kind`;
/// a kind with nothing registered is skipped (and logged once). `pub(crate)`
/// to match [`ChannelTransport`]'s own visibility — see that trait's doc.
#[derive(Default)]
pub(crate) struct ChannelTransportRegistry {
    transports: HashMap<ChannelKind, Arc<dyn ChannelTransport>>,
}

impl ChannelTransportRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register(&mut self, transport: Arc<dyn ChannelTransport>) {
        self.transports.insert(transport.kind(), transport);
    }

    pub(crate) fn get(&self, kind: ChannelKind) -> Option<&Arc<dyn ChannelTransport>> {
        self.transports.get(&kind)
    }

    /// Iterates every registered transport, regardless of kind — the seam
    /// the supervisor dispatches a per-kind operation over (invalidating a
    /// thread, starting an outbound observer) instead of naming each kind
    /// explicitly, so a newly registered kind is picked up automatically.
    pub(crate) fn values(&self) -> impl Iterator<Item = &Arc<dyn ChannelTransport>> {
        self.transports.values()
    }
}

/// Channel-agnostic outcome of a single outbound send (e.g. an email, or a
/// future Discord/WhatsApp reply). Every channel kind's send path normalizes
/// its own SDK/API error shape down to this so callers — chiefly agent tools
/// like `SendEmail` — get one consistent success/failure/retry contract
/// regardless of which channel actually carried the message.
#[derive(Debug, Clone, PartialEq)]
pub struct SendResult {
    pub success: bool,
    /// The channel's own id for the sent message (e.g. an email `Message-ID`),
    /// when the channel assigns one and the send succeeded.
    pub message_id: Option<String>,
    /// `None` on success; classifies the failure on error.
    pub error_kind: Option<SendErrorKind>,
    /// Whether retrying the same send might succeed — e.g. a transient network
    /// failure is retryable, a rejected recipient address is not.
    pub retryable: bool,
    /// A channel-suggested backoff before retrying, when known (e.g. from a
    /// rate-limit response). `None` when the channel gave no hint.
    pub retry_after: Option<Duration>,
}

impl SendResult {
    pub fn success(message_id: Option<String>) -> Self {
        Self {
            success: true,
            message_id,
            error_kind: None,
            retryable: false,
            retry_after: None,
        }
    }

    pub fn failure(error_kind: SendErrorKind, retryable: bool) -> Self {
        Self {
            success: false,
            message_id: None,
            error_kind: Some(error_kind),
            retryable,
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }
}

/// Coarse classification of an outbound send failure, shared across channel
/// kinds so a caller can decide whether to retry, surface a user-facing
/// error, or give up without needing to know which channel it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErrorKind {
    /// The message exceeds the channel's size/length limit.
    TooLong,
    /// The message or its addressing doesn't conform to the channel's
    /// required shape (e.g. a malformed recipient address).
    BadFormat,
    /// The channel rejected the send as unauthorized (bad/expired
    /// credentials, insufficient permissions).
    Forbidden,
    /// The target (recipient, chat, channel) doesn't exist or is unreachable.
    NotFound,
    /// The channel is throttling this sender; see `SendResult::retry_after`.
    RateLimited,
    /// A transient failure (network error, timeout, 5xx) — safe to retry.
    Transient,
    /// Doesn't fit any of the above.
    Unknown,
}

/// Builds the [`QueuedMessage`] for one inbound channel message, addressed
/// at the binding's dedicated bridge thread and tagged with
/// [`MessageSource::Channel`] so a later phase can resolve which binding
/// (and thus which transport) should carry the agent's reply back out.
pub fn build_channel_queued_message(
    binding_id: &str,
    bridge_thread_id: &str,
    kind: ChannelKind,
    conversation_id: &str,
    sender_id: &str,
    text: &str,
) -> QueuedMessage {
    QueuedMessage {
        message_id: uuid::Uuid::new_v4().to_string(),
        content: text.to_string(),
        queued_at: Utc::now(),
        attachments: vec![],
        source: Some(MessageSource::Channel {
            kind,
            binding_id: binding_id.to_string(),
            conversation_id: conversation_id.to_string(),
            sender_id: sender_id.to_string(),
        }),
        focus_path: None,
        thread_id: Some(bridge_thread_id.to_string()),
    }
}

/// Writes `text` to `bridge_thread_id`'s transcript (so it renders as a
/// normal chat bubble) and submits it to `agent`'s message queue. Dispatch
/// through [`QueueManagerRegistry::submit_message`] runs the agent exactly
/// as it would for a typed message — the assistant's reply lands in the
/// same thread through the existing transcript-write path, with no
/// channel-specific run handling required. Shared by every
/// [`ChannelTransport::spawn`] implementation so a new channel kind gets
/// this delivery path for free.
///
/// `sender_display_name`, when given, is prefixed onto the transcript
/// entry's content (`"alice: <text>"`) — without it, a bridge thread that
/// mixes messages from several senders (a shared Discord/Telegram channel,
/// or an email thread with more than one correspondent) reads back as one
/// merged, unattributed stream with no way to tell who said what. Each
/// transport passes whatever human-readable name it has on hand for the
/// sender, or `None` where it has none, in which case the content is
/// written exactly as before this parameter existed. The queued message
/// itself is left unprefixed either way — only the transcript's own record
/// of "who said what" needs the label.
///
/// `auto_title_candidate`, when given, is run through
/// [`derive_auto_title`] and — if that yields a non-blank title — written
/// via [`ao_persistence::ThreadStore::set_auto_title_if_unset`], which
/// no-ops once `title` or `auto_title` is already set. That fresh-only
/// guarantee is what makes this call idempotent no matter how many inbound
/// messages a bridge thread ever sees: only the first one that arrives with
/// a candidate can ever set the label. This function stays channel-agnostic
/// on purpose — it only truncates and gates, never inspects `kind` — so
/// each transport is responsible for cleaning its own wire markup out of
/// the candidate before calling in. A transport with no per-message title
/// concept of its own (Slack mints its bridge thread lazily and titles it
/// at creation instead; email's subject-based titling lands in a follow-up)
/// simply passes `None`.
#[allow(clippy::too_many_arguments)]
pub async fn submit_inbound_message(
    ctx: &ChannelRunContext,
    agent: &AgentProfile,
    bridge_thread_id: &str,
    kind: ChannelKind,
    conversation_id: &str,
    sender_id: &str,
    sender_display_name: Option<&str>,
    text: &str,
    auto_title_candidate: Option<String>,
) -> Result<(), AoError> {
    match ctx.persistence.threads.get(bridge_thread_id).await? {
        Some(thread) => {
            let entry = TranscriptEntry {
                ts: Utc::now(),
                role: TranscriptRole::System("user".to_string()),
                content: labeled_transcript_content(sender_display_name, text),
                event_type: "message".to_string(),
                metadata: None,
                hidden_from_user: false,
            };
            ctx.persistence
                .transcripts
                .append_at(&std::path::PathBuf::from(&thread.transcript_path), &entry)
                .await?;

            // Persistence alone leaves an already-open SSE stream unaware of
            // this inbound message — only the agent's own reply carries live
            // events (RunStarted/TextDelta/...), so without this the inbound
            // message itself would sit invisible until the thread is
            // reloaded from disk. Reuses the same bus-emit-a-transcript-entry
            // path `TimelineAdapter::queue_hidden_user_entry` uses for
            // injected mid-run entries; `hidden_from_user` is false here, so
            // it renders as a normal chat bubble.
            ctx.event_bus
                .emit(
                    &format!("thread:{}", thread.id),
                    &agent.id,
                    Some(thread.id.clone()),
                    AgentEventPayload::HiddenTranscriptEntry { entry: entry.clone() },
                )
                .await;

            if let Some(candidate) = auto_title_candidate {
                if let Some(title) = derive_auto_title(&candidate) {
                    if let Err(e) = ctx.persistence.threads.set_auto_title_if_unset(&thread.id, title).await {
                        warn!(
                            agent_id = %agent.id,
                            thread_id = %thread.id,
                            "ChannelBridge: failed to set auto_title from first inbound message: {e}"
                        );
                    }
                }
            }
        }
        None => {
            warn!(
                agent_id = %agent.id,
                thread_id = %bridge_thread_id,
                "ChannelBridge: bridge thread missing; queuing message without a transcript write"
            );
        }
    }

    let queued_message = build_channel_queued_message(
        &ctx.binding_id,
        bridge_thread_id,
        kind,
        conversation_id,
        sender_id,
        text,
    );
    ctx.queue_registry.submit_message(agent, queued_message).await
}

/// Prefixes `text` with `"{name}: "` when `sender_display_name` is present
/// and non-blank; returns `text` unchanged otherwise (including when it's
/// `Some("")`/whitespace-only — a blank name is the same as no name).
fn labeled_transcript_content(sender_display_name: Option<&str>, text: &str) -> String {
    match sender_display_name {
        Some(name) if !name.trim().is_empty() => format!("{name}: {text}"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;

    use ao_protocol::agent::{AgentProfile, AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::error::AoError;
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};
    use crate::instance_registry::InstanceRegistry;

    use super::*;

    #[test]
    fn build_channel_queued_message_targets_bridge_thread_and_tags_channel_source() {
        let msg = build_channel_queued_message(
            "telegram",
            "bridge-thread-1",
            ChannelKind::Telegram,
            "555",
            "555",
            "hello from a channel",
        );

        assert_eq!(msg.thread_id.as_deref(), Some("bridge-thread-1"));
        assert_eq!(msg.content, "hello from a channel");
        match msg.source {
            Some(MessageSource::Channel { kind, binding_id, conversation_id, sender_id }) => {
                assert_eq!(kind, ChannelKind::Telegram);
                assert_eq!(binding_id, "telegram");
                assert_eq!(conversation_id, "555");
                assert_eq!(sender_id, "555");
            }
            other => panic!("expected MessageSource::Channel, got {other:?}"),
        }
    }

    #[test]
    fn registry_returns_none_for_an_unregistered_kind() {
        let registry = ChannelTransportRegistry::new();
        assert!(registry.get(ChannelKind::Email).is_none());
    }

    // --- Author-labeled transcript content ---

    #[test]
    fn labeled_transcript_content_prefixes_a_display_name() {
        assert_eq!(labeled_transcript_content(Some("alice"), "hey there"), "alice: hey there");
    }

    #[test]
    fn labeled_transcript_content_is_unchanged_when_no_name_is_given() {
        assert_eq!(labeled_transcript_content(None, "hey there"), "hey there");
    }

    #[test]
    fn labeled_transcript_content_treats_a_blank_name_as_no_name() {
        assert_eq!(labeled_transcript_content(Some(""), "hey there"), "hey there");
        assert_eq!(labeled_transcript_content(Some("   "), "hey there"), "hey there");
    }

    // --- Auto-title from `auto_title_candidate` (the shared seam every
    // non-Slack, non-email transport wires its own cleaned candidate
    // through — see the doc on `submit_inbound_message` itself) ---

    /// A stub [`AgentRunner`] that completes immediately without spawning a
    /// real process — just enough for `submit_inbound_message`'s real
    /// `QueueManagerRegistry` path to run inside a unit test. Mirrors the
    /// identically-named test double in `slack::runner`'s and
    /// `crate::telegram::transport`'s own test modules.
    struct StubRunner;

    #[async_trait]
    impl AgentRunner for StubRunner {
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

    fn make_test_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
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
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 2,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
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

    async fn make_test_ctx(agent_id: &str, binding_id: &str) -> (ChannelRunContext, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let persistence =
            Arc::new(ao_persistence::PersistenceLayer::init_with_root(data_root).await.expect("init persistence"));
        persistence.agents.create(&make_test_agent(agent_id)).await.expect("create agent");

        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(InstanceRegistry::new());
        let dispatcher =
            Arc::new(RunnerDispatcher::with_runners(Arc::new(StubRunner), Arc::new(StubRunner)));
        let queue_registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            instance_registry,
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        let ctx = ChannelRunContext {
            agent_id: agent_id.to_string(),
            binding_id: binding_id.to_string(),
            persistence,
            queue_registry,
            connection_state: Arc::new(ConnectionStateRegistry::new()),
            lease_gate: Arc::new(LeaseGate::new()),
            event_bus,
        };
        (ctx, tmp)
    }

    #[tokio::test]
    async fn fresh_thread_gets_auto_title_from_first_inbound_candidate_and_a_later_message_does_not_overwrite_it() {
        let (ctx, _tmp) = make_test_ctx("agent-title", "discord").await;
        let fresh = ctx.persistence.threads.build_fresh_thread("agent-title", None);
        let thread = ctx.persistence.threads.create(fresh).await.expect("create thread");
        let agent = ctx.persistence.agents.get("agent-title").await.unwrap().expect("agent exists");

        submit_inbound_message(
            &ctx,
            &agent,
            &thread.id,
            ChannelKind::Discord,
            "channel-1",
            "user-1",
            Some("alice"),
            "please help with the deploy",
            Some("please help with the deploy".to_string()),
        )
        .await
        .expect("first inbound message delivers");

        let after_first = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert_eq!(after_first.auto_title.as_deref(), Some("please help with the deploy"));
        assert!(after_first.title.is_none(), "a fresh bridge thread must stay renamable (title unset)");

        submit_inbound_message(
            &ctx,
            &agent,
            &thread.id,
            ChannelKind::Discord,
            "channel-1",
            "user-1",
            Some("alice"),
            "totally unrelated follow-up text",
            Some("totally unrelated follow-up text".to_string()),
        )
        .await
        .expect("second inbound message delivers");

        let after_second = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert_eq!(
            after_second.auto_title.as_deref(),
            Some("please help with the deploy"),
            "a later message must never overwrite the auto_title set from the first one"
        );
    }

    #[tokio::test]
    async fn no_candidate_leaves_auto_title_unset() {
        let (ctx, _tmp) = make_test_ctx("agent-title-2", "slack").await;
        let fresh = ctx.persistence.threads.build_fresh_thread("agent-title-2", None);
        let thread = ctx.persistence.threads.create(fresh).await.expect("create thread");
        let agent = ctx.persistence.agents.get("agent-title-2").await.unwrap().expect("agent exists");

        // Mirrors Slack's and email's call sites, which pass `None` here.
        submit_inbound_message(
            &ctx,
            &agent,
            &thread.id,
            ChannelKind::Slack,
            "channel-1",
            "user-1",
            None,
            "hello",
            None,
        )
        .await
        .expect("inbound message delivers");

        let after = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert!(after.auto_title.is_none(), "a `None` candidate must never set auto_title");
    }

    #[tokio::test]
    async fn a_markup_only_candidate_that_cleans_to_blank_leaves_auto_title_unset() {
        let (ctx, _tmp) = make_test_ctx("agent-title-3", "discord").await;
        let fresh = ctx.persistence.threads.build_fresh_thread("agent-title-3", None);
        let thread = ctx.persistence.threads.create(fresh).await.expect("create thread");
        let agent = ctx.persistence.agents.get("agent-title-3").await.unwrap().expect("agent exists");

        // An already-cleaned candidate that derives to nothing (e.g. a
        // mention-only Discord message after `clean_discord_markup` has
        // already stripped it down) — `derive_auto_title` returning `None`
        // must mean the store write is skipped outright, not attempted with
        // a blank string.
        submit_inbound_message(
            &ctx,
            &agent,
            &thread.id,
            ChannelKind::Discord,
            "channel-1",
            "user-1",
            None,
            "<@123456>",
            Some(String::new()),
        )
        .await
        .expect("inbound message delivers");

        let after = ctx.persistence.threads.get(&thread.id).await.unwrap().expect("thread exists");
        assert!(after.auto_title.is_none(), "a blank-after-cleaning candidate must never set auto_title");
    }
}
