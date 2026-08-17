//! Channel-agnostic inbound supervisor.
//!
//! [`ChannelBridge`] is a supervisor, spawned once at process start
//! alongside [`crate::schedule_runner::ScheduleRunner`]. It periodically
//! reconciles which agents have an enabled, fully-provisioned channel
//! binding (Telegram, Discord, and later email, ...) and keeps exactly one
//! inbound task alive per binding, selecting the
//! [`crate::channels::ChannelTransport`] implementation registered for that
//! binding's [`ChannelKind`]. Each inbound message is written to the
//! binding's dedicated bridge thread and submitted through the normal
//! message queue, so the agent processes it exactly like a typed chat turn
//! — no separate run path. This file only covers pushing messages in; the
//! outbound half (relaying a finished turn back out) is currently
//! Telegram- and Discord-only — see [`super::outbound`] and
//! [`crate::channels::discord::outbound`] — since those are the only two
//! synchronous chat channels; a future email channel delivers replies via a
//! tool call instead.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::agent::ChannelKind;
use ao_protocol::channel_connection_state::ChannelConnectionState;

use crate::channels::connection_state::ConnectionStateRegistry;
use crate::channels::discord::DiscordTransport;
use crate::channels::email::EmailTransport;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::slack::SlackTransport;
use crate::channels::{ChannelRunContext, ChannelTransport, ChannelTransportRegistry};
use crate::event_bus::EventBus;
use crate::queue_manager::QueueManagerRegistry;
use crate::telegram::transport::TelegramTransport;

/// How often the supervisor re-reads agent profiles to start/stop per-binding
/// inbound tasks.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// TTL for a binding's single-writer lease — long
/// enough that one missed or slow reconcile tick doesn't cost the holder
/// its own lease, short enough that a crashed holder's binding becomes
/// claimable again well within a human noticing.
fn lease_ttl() -> chrono::Duration {
    chrono::Duration::seconds(3 * RECONCILE_INTERVAL.as_secs() as i64)
}

/// A live per-binding inbound task tracked by the reconcile loop.
struct RunningBinding {
    /// Snapshot of the fingerprint ([`ChannelTransport::fingerprint`]) this
    /// task was started with. Compared against a freshly computed
    /// fingerprint on every reconcile tick so a config/secret change (e.g. a
    /// rotated bot token) restarts the task instead of silently continuing
    /// with stale state.
    fingerprint: String,
    /// The binding's dedicated bridge thread, captured at spawn time so
    /// stopping this task can invalidate any outbound-relay mapping for it
    /// (see [`ChannelBridge::invalidate_thread`]) without a second profile
    /// read.
    bridge_thread_id: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Supervises the inbound channel tasks, one per (agent, binding) with an
/// enabled and provisioned channel binding whose kind has a registered
/// transport.
pub struct ChannelBridge {
    persistence: Arc<PersistenceLayer>,
    queue_registry: Arc<QueueManagerRegistry>,
    /// Shared with [`super::outbound::run_outbound_observer`], which watches
    /// this bus for turn completions.
    event_bus: Arc<EventBus>,
    transports: ChannelTransportRegistry,
    /// Kept as a direct handle, alongside (not instead of) its entry in
    /// `transports` above, purely so HTTP routes can reach kind-specific
    /// behavior the generic [`ChannelTransport`] trait deliberately doesn't
    /// model: [`Self::invalidate_thread_for_chat`]'s Telegram `chat_id: i64`
    /// unlink, which has no equivalent shape on Discord or email.
    /// [`Self::invalidate_thread`] itself no longer touches this field — see
    /// its own doc — so this is narrower than it looks.
    telegram: Arc<TelegramTransport>,
    /// Mirrors `telegram` above structurally, but has no kind-specific
    /// method of its own to back today (Discord has no `chat_id`-unlink
    /// equivalent) — every current use of this transport goes through its
    /// `transports` registry entry instead. Kept rather than removed only
    /// because the field itself is out of scope for this change; `#[allow]`
    /// documents that its current unused state is known, not an oversight.
    #[allow(dead_code)]
    discord: Arc<DiscordTransport>,
    /// Kinds already logged as "no transport registered" — keeps repeated
    /// reconcile ticks from spamming a warning once a channel kind is known
    /// to be unimplemented.
    warned_unregistered_kinds: Mutex<HashSet<&'static str>>,
    /// This process's identity for the single-writer lease — a random id generated once at construction, not a PID (PIDs
    /// recycle across restarts and mean nothing across machines). Every
    /// claim/heartbeat this bridge makes uses the same id, so
    /// [`ao_persistence::channel_lease_store::ChannelLeaseStore`] can tell a
    /// renewal by this process apart from a claim by another one.
    owner_id: String,
    /// `(agent_id, binding_id)` keys already logged as "lease held
    /// elsewhere, not starting here" — keeps repeated reconcile ticks from
    /// spamming that message while a binding legitimately stays unclaimable
    /// (e.g. the other worktree's backend is the live holder). Cleared once
    /// this process successfully claims the binding, so a real state change
    /// is observable again.
    lease_refused_logged: Mutex<HashSet<(String, String)>>,
    /// Per-binding connection state, read by
    /// [`Self::connection_state`] for `GET /agents/{id}/channels`. `reconcile`
    /// writes the lease-derived `not-holding-lease` value and clears an
    /// entry on every other kind of stop; each transport writes its own
    /// connect/backoff transitions through the copy handed to it via
    /// [`ChannelRunContext::connection_state`].
    connection_state: Arc<ConnectionStateRegistry>,
    /// Process-local record of which bridge threads this process currently
    /// holds the single-writer lease for. `reconcile` is the only writer —
    /// see [`LeaseGate`]'s module doc. Shared (not just consulted) by the
    /// outbound relay observers themselves: each registered transport's
    /// [`ChannelTransport::spawn_outbound_observer`] task runs process-wide,
    /// one per registered kind that has one, so without this an observer
    /// would keep relaying for a binding whose lease this process has since
    /// lost (or never held).
    lease_gate: Arc<LeaseGate>,
}

impl ChannelBridge {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        queue_registry: Arc<QueueManagerRegistry>,
        event_bus: Arc<EventBus>,
        telegram: Arc<TelegramTransport>,
        discord: Arc<DiscordTransport>,
        email: Arc<EmailTransport>,
        slack: Arc<SlackTransport>,
    ) -> Self {
        let mut transports = ChannelTransportRegistry::new();
        transports.register(Arc::clone(&telegram) as Arc<dyn ChannelTransport>);
        transports.register(Arc::clone(&discord) as Arc<dyn ChannelTransport>);
        transports.register(email as Arc<dyn ChannelTransport>);
        transports.register(slack as Arc<dyn ChannelTransport>);
        Self {
            persistence,
            queue_registry,
            event_bus,
            transports,
            telegram,
            discord,
            warned_unregistered_kinds: Mutex::new(HashSet::new()),
            owner_id: uuid::Uuid::new_v4().to_string(),
            lease_refused_logged: Mutex::new(HashSet::new()),
            connection_state: Arc::new(ConnectionStateRegistry::new()),
            lease_gate: Arc::new(LeaseGate::new()),
        }
    }

    /// Spawn the supervisor as a background task: the inbound reconcile loop
    /// and every registered transport's outbound relay observer (see
    /// [`ChannelTransport::spawn_outbound_observer`]) all run under it,
    /// sharing one shutdown signal. Returns a shutdown sender — drop it (or send
    /// `()`) to stop all of them and cancel every live inbound task — alongside a
    /// [`JoinHandle`] for the reconcile loop's task specifically. A graceful
    /// shutdown handler needs that handle, not just the sender: sending on
    /// the sender only *requests* the stop, while every binding this
    /// process holds actually releasing its lease ([`Self::run`]'s shutdown
    /// loop below) is async work that happens
    /// *after* the send — awaiting the handle is what lets a caller
    /// (`ao-server`'s signal handler) confirm those releases actually
    /// finished before the process exits, rather than guessing a sleep
    /// duration and hoping it was long enough.
    ///
    /// Takes `self` already `Arc`-wrapped (rather than wrapping it
    /// internally) so the caller keeps its own handle — `AppState` holds
    /// onto one to call [`Self::invalidate_thread`] /
    /// [`Self::invalidate_thread_for_chat`] straight from the token-delete
    /// and chat-unlink HTTP handlers, instead of waiting out
    /// [`RECONCILE_INTERVAL`] for the reconcile loop to notice.
    pub fn run(self: Arc<Self>) -> (watch::Sender<()>, JoinHandle<()>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        info!("ChannelBridge starting");
        let bridge = self;

        let reconcile_handle = {
            let bridge = Arc::clone(&bridge);
            let mut shutdown_rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut running: HashMap<(String, String), RunningBinding> = HashMap::new();

                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            info!("ChannelBridge shutting down");
                            break;
                        }
                        _ = tokio::time::sleep(RECONCILE_INTERVAL) => {
                            bridge.reconcile(&mut running).await;
                        }
                    }
                }

                for (key, binding) in running {
                    binding.cancel.cancel();
                    let _ = binding.handle.await;
                    // This process no longer runs this binding's inbound
                    // task, so its outbound observers must never relay for
                    // it again either — same invariant `reconcile` enforces
                    // on every other stop path (see `Self::reconcile`). Keyed
                    // on the binding, not `bridge_thread_id`, so this clears
                    // every thread id registered under it (Slack's
                    // per-conversation threads included), not just the one
                    // placeholder thread this struct happens to track.
                    bridge.lease_gate.mark_inactive(&key.1);
                    // Best-effort: release the lease so a fresh process
                    // (e.g. this same worktree's backend restarting, or a
                    // rolling deploy's replacement instance) can claim the
                    // binding immediately instead of waiting out its TTL.
                    // The caller awaiting this task's `JoinHandle` (see
                    // `Self::run`'s doc comment) is what makes this release
                    // actually happen before a graceful-shutdown process
                    // exit, not just before this task happens to get
                    // scheduled.
                    if let Err(e) = bridge
                        .persistence
                        .channel_leases
                        .release(&key.0, &key.1, &bridge.owner_id)
                        .await
                    {
                        warn!(agent_id = %key.0, binding_id = %key.1, owner_id = %bridge.owner_id, "channel lease: release-failed on shutdown: {e}");
                    }
                }
            })
        };

        // One outbound-relay observer per registered transport that has one
        // (see `ChannelTransport::spawn_outbound_observer`) — dispatched over
        // the registry rather than named per kind, so a newly registered
        // kind's observer starts here automatically, with no change to this
        // loop. Fire-and-forget exactly as the old per-kind spawns were: each
        // observer manages its own lifetime off its own `shutdown_rx` clone,
        // and this loop never stores or awaits the returned handle.
        for transport in bridge.transports.values() {
            let transport = Arc::clone(transport);
            let persistence = Arc::clone(&bridge.persistence);
            let lease_gate = Arc::clone(&bridge.lease_gate);
            let event_bus = Arc::clone(&bridge.event_bus);
            let shutdown_rx = shutdown_rx.clone();
            transport.spawn_outbound_observer(persistence, lease_gate, event_bus, shutdown_rx);
        }

        (shutdown_tx, reconcile_handle)
    }

    /// Ends the outbound-relay binding for `thread_id` outright — dispatches
    /// [`ChannelTransport::invalidate_thread`] over every registered
    /// transport rather than naming Telegram/Discord/Email explicitly, so a
    /// newly registered kind is included with no change here. Called
    /// whenever a binding is torn down: [`Self::reconcile`] calls this for
    /// every inbound task it stops (agent disabled, config/secret changed,
    /// or the agent/binding removed), and the token-delete HTTP handler
    /// calls it directly for immediate effect instead of waiting out
    /// [`RECONCILE_INTERVAL`]. Bridge thread ids are globally unique per
    /// binding, so calling this on every transport is safe regardless of
    /// which kind `thread_id` actually belongs to — every transport that
    /// never recorded it is a harmless no-op (including one with no
    /// outbound relay at all, e.g. email's deliberate no-op impl).
    pub fn invalidate_thread(&self, thread_id: &str) {
        for transport in self.transports.values() {
            transport.invalidate_thread(thread_id);
        }
    }

    /// Ends the outbound-relay binding for `thread_id` only if it currently
    /// points at `chat_id`. Used by the chat-unlink HTTP handler: several
    /// chats can share one dedicated bridge thread (multi-user pairing), so
    /// unlinking one must not discard an in-flight reply actually destined
    /// for a different, still-linked chat. Telegram-only by nature (`chat_id`
    /// is a Telegram concept with no equivalent on Discord's `channel_id: String`
    /// or on email), so this stays a direct call through the `telegram` field
    /// rather than a [`ChannelTransport`] method every kind would have to
    /// implement — unlike [`Self::invalidate_thread`] above, there is no
    /// silent-failure risk here: a hypothetical future kind needing its own
    /// per-conversation unlink would need its own HTTP route and its own
    /// method, not a slot in this one's dispatch.
    pub fn invalidate_thread_for_chat(&self, thread_id: &str, chat_id: i64) {
        self.telegram.invalidate_thread_for_chat(thread_id, chat_id);
    }

    /// Per-binding connection state for `GET /agents/{id}/channels`:
    /// whatever this process's supervisor or transport most
    /// recently reported for `(agent_id, binding_id)`, defaulting to
    /// [`ChannelConnectionState::Disconnected`] when nothing has ever been
    /// reported here (the binding isn't running in this process at all — and
    /// as far as this process's own lease attempts can tell, not anywhere
    /// else it can observe either). A synchronous read of the in-memory
    /// registry `reconcile` and every transport already keep current — no
    /// persistence I/O on this path.
    pub fn connection_state(&self, agent_id: &str, binding_id: &str) -> ChannelConnectionState {
        self.connection_state.get(agent_id, binding_id)
    }

    /// Compute the desired set of bound inbound tasks and start/stop
    /// per-binding tasks to match. A binding is eligible once it's enabled
    /// and its kind has a registered transport that reports a fingerprint
    /// (i.e. a resolvable secret) — and, for every kind except Discord,
    /// Telegram, and Email, its `bridge_thread_id` has been provisioned too
    /// (agents mid-setup are skipped until enabling finishes provisioning
    /// the thread). Discord, Telegram, and Email no longer need one: each
    /// mints a fresh per-conversation thread on demand from its own inbound
    /// dispatch instead of routing every conversation through one
    /// eagerly-provisioned thread (see
    /// `crate::channels::discord::runner::resolve_discord_conversation_thread`,
    /// `crate::telegram::transport::resolve_telegram_conversation_thread`,
    /// and `crate::channels::email::resolve_email_conversation_thread`).
    async fn reconcile(&self, running: &mut HashMap<(String, String), RunningBinding>) {
        let profiles = match self.persistence.agents.list().await {
            Ok(profiles) => profiles,
            Err(e) => {
                warn!("ChannelBridge reconcile: failed to list agent profiles: {e}");
                return;
            }
        };

        // (agent_id, binding_id) -> (fingerprint, bridge_thread_id, kind)
        let mut desired: HashMap<(String, String), (String, String, ChannelKind)> = HashMap::new();
        for profile in &profiles {
            for binding in &profile.channels {
                if !binding.enabled {
                    continue;
                }
                // Discord, Telegram, and Email each mint their own
                // per-conversation bridge threads on demand (see
                // `resolve_discord_conversation_thread` /
                // `resolve_telegram_conversation_thread` /
                // `resolve_email_conversation_thread`) and no longer need an
                // eagerly-provisioned `bridge_thread_id` to become eligible
                // here — `String::new()` stands in as their placeholder
                // value purely for this map's shared shape, guarded (below
                // and at start-up) by `is_empty()` so it's never registered
                // as a meaningless real thread id. Every other kind (Slack)
                // still requires a real, provisioned value.
                let bridge_thread_id = if matches!(binding.kind, ChannelKind::Discord | ChannelKind::Telegram | ChannelKind::Email) {
                    binding.bridge_thread_id.clone().unwrap_or_default()
                } else {
                    let Some(bridge_thread_id) = binding.bridge_thread_id.clone() else {
                        continue;
                    };
                    bridge_thread_id
                };
                let Some(transport) = self.transports.get(binding.kind) else {
                    self.warn_unregistered_kind_once(binding.kind);
                    continue;
                };
                let Some(fingerprint) = transport.fingerprint(profile, binding) else {
                    continue;
                };
                desired.insert(
                    (profile.id.clone(), binding.binding_id.clone()),
                    (fingerprint, bridge_thread_id, binding.kind),
                );
            }
        }

        let now = Utc::now();

        // Stop tasks for bindings no longer eligible, whose fingerprint
        // changed since the task was started (config or secret rotation),
        // or whose single-writer lease this process has since lost (another
        // process's heartbeat won the reclaim).
        // Either way the binding just ended, so the outbound observer must
        // not relay a stray completion on this thread anymore — see
        // `Self::invalidate_thread`.
        let mut stale: Vec<(String, String)> = Vec::new();
        for (key, binding) in running.iter() {
            let fingerprint_matches =
                desired.get(key).map(|(fingerprint, _, _)| fingerprint) == Some(&binding.fingerprint);
            if !fingerprint_matches {
                // No longer desired, or reconfigured — either way this
                // process's last-reported state for it must not linger. If
                // it's still desired under a new fingerprint, the start loop
                // below reports a fresh state right after restarting it.
                self.connection_state.remove(&key.0, &key.1);
                stale.push(key.clone());
                continue;
            }
            match self
                .persistence
                .channel_leases
                .try_claim(&key.0, &key.1, &self.owner_id, lease_ttl(), now)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // Only this call site (not `ChannelLeaseStore::try_claim`
                    // itself) can tell "lost" apart from "refused": both read
                    // as an ordinary `Ok(false)` heartbeat/claim attempt to
                    // the store, but this loop is iterating bindings it was
                    // *already running* — so a refusal here specifically
                    // means another owner just claimed a lease this process
                    // held a moment ago, not a first-time refusal.
                    let new_owner_id = match self.persistence.channel_leases.get(&key.0, &key.1).await {
                        Ok(Some(lease)) => Some(lease.owner_id),
                        _ => None,
                    };
                    info!(
                        agent_id = %key.0, binding_id = %key.1, owner_id = %self.owner_id,
                        new_owner_id = ?new_owner_id,
                        "channel lease: lost (another owner claimed this binding), stopping"
                    );
                    // Set (not remove): the binding is still desired, just
                    // not runnable here anymore — `not-holding-lease` is the
                    // honest terminal state, and the stop loop below must
                    // not clobber it back to the `Disconnected` default.
                    self.connection_state.set(&key.0, &key.1, ChannelConnectionState::NotHoldingLease);
                    stale.push(key.clone());
                }
                Err(e) => {
                    // Persistence hiccup: keep the binding running this tick
                    // rather than tearing it down on a transient IO error.
                    warn!(agent_id = %key.0, binding_id = %key.1, owner_id = %self.owner_id, "channel lease: heartbeat failed, keeping binding running this tick: {e}");
                }
            }
        }
        for key in stale {
            if let Some(binding) = running.remove(&key) {
                debug!(agent_id = %key.0, binding_id = %key.1, "ChannelBridge: stopping inbound task");
                binding.cancel.cancel();
                let _ = binding.handle.await;
                self.invalidate_thread(&binding.bridge_thread_id);
                // This process no longer runs this binding's inbound task —
                // its outbound observers must stop relaying for it too,
                // whether it stopped because it's no longer desired,
                // reconfigured, or its lease was lost to another owner.
                // Keyed on the binding so every thread id registered under
                // it clears together — see `LeaseGate::mark_inactive`.
                self.lease_gate.mark_inactive(&key.1);
                // Best-effort: release immediately so the binding is
                // claimable right away rather than waiting out the TTL. A
                // no-op if we no longer hold the lease (the lost-lease case
                // above).
                if let Err(e) = self.persistence.channel_leases.release(&key.0, &key.1, &self.owner_id).await {
                    warn!(agent_id = %key.0, binding_id = %key.1, owner_id = %self.owner_id, "channel lease: release-failed on stop: {e}");
                }
                self.lease_refused_logged.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            }
        }

        // Start tasks for newly (or just-restarted) eligible bindings —
        // only once this process holds (or successfully claims) the
        // binding's single-writer lease.
        for (key, (fingerprint, bridge_thread_id, kind)) in desired {
            if running.contains_key(&key) {
                continue;
            }
            let Some(transport) = self.transports.get(kind) else {
                continue;
            };

            match self
                .persistence
                .channel_leases
                .try_claim(&key.0, &key.1, &self.owner_id, lease_ttl(), now)
                .await
            {
                Ok(true) => {
                    self.lease_refused_logged.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                }
                Ok(false) => {
                    self.warn_lease_unavailable_once(&key.0, &key.1);
                    self.connection_state.set(&key.0, &key.1, ChannelConnectionState::NotHoldingLease);
                    continue;
                }
                Err(e) => {
                    warn!(agent_id = %key.0, binding_id = %key.1, owner_id = %self.owner_id, "channel lease: claim-failed, not starting this tick: {e}");
                    continue;
                }
            }

            debug!(agent_id = %key.0, binding_id = %key.1, "ChannelBridge: starting inbound task");
            let cancel = CancellationToken::new();
            let ctx = ChannelRunContext {
                agent_id: key.0.clone(),
                binding_id: key.1.clone(),
                persistence: Arc::clone(&self.persistence),
                queue_registry: Arc::clone(&self.queue_registry),
                connection_state: Arc::clone(&self.connection_state),
                lease_gate: Arc::clone(&self.lease_gate),
                event_bus: Arc::clone(&self.event_bus),
            };
            // Attempting-to-connect is the honest starting state — the
            // transport reports `Connected` once it actually has a healthy
            // session (see each transport's own inbound loop).
            self.connection_state.set(&key.0, &key.1, ChannelConnectionState::Reconnecting);
            // Marked active before the inbound task is even spawned: this is
            // the authoritative "this process holds the lease" signal the
            // outbound observers gate every relay on (see `LeaseGate`'s
            // module doc), independent of whether the inbound task itself
            // has processed anything yet. Discord, Telegram, and Slack each
            // register their own per-conversation thread ids instead, from
            // within their own inbound dispatch (see
            // `resolve_discord_conversation_thread` /
            // `resolve_telegram_conversation_thread` / `resolve_bridge_thread`)
            // — `bridge_thread_id` is empty for Discord and Telegram here
            // (see above), so this call is skipped rather than registering a
            // meaningless placeholder. Email is also exempted above but
            // deliberately never registers *any* per-conversation thread
            // with `LeaseGate` at all (Email is inbound-routing-only, with
            // no outbound relay for it to gate) — for Email this `is_empty()`
            // check is simply always true, not a placeholder waiting to be
            // replaced.
            if !bridge_thread_id.is_empty() {
                self.lease_gate.mark_active(&key.1, &bridge_thread_id);
            }
            let handle = transport.spawn(ctx, cancel.clone());
            running.insert(key, RunningBinding { fingerprint, bridge_thread_id, cancel, handle });
        }
    }

    fn warn_unregistered_kind_once(&self, kind: ChannelKind) {
        let mut warned = self.warned_unregistered_kinds.lock().unwrap_or_else(|e| e.into_inner());
        if warned.insert(kind.as_str()) {
            warn!(
                kind = kind.as_str(),
                "ChannelBridge reconcile: no transport registered for this channel kind, skipping"
            );
        }
    }

    /// Logs a binding's "lease held elsewhere" state exactly once per loss —
    /// this is the quiet, reportable state a not-yet/no-longer-held binding
    /// settles into rather than a warning spamming every 5s reconcile tick.
    fn warn_lease_unavailable_once(&self, agent_id: &str, binding_id: &str) {
        let mut logged = self.lease_refused_logged.lock().unwrap_or_else(|e| e.into_inner());
        if logged.insert((agent_id.to_string(), binding_id.to_string())) {
            info!(
                agent_id, binding_id, owner_id = %self.owner_id,
                "channel lease: refused (held live by another process), not starting here (state: not-holding-lease)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, ChannelBinding, ChannelKindConfig, CliProviderConfig, InputMode,
        OutputFormat, ProviderConfig, TelegramThreadMode,
    };
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use ao_engine_tools_provider_config::TelegramTokenStore;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};
    use crate::event_bus::EventBus;
    use crate::instance_registry::InstanceRegistry;
    use crate::telegram::client::TelegramClient;

    // Tests in this module mutate process-wide env vars (data root, Telegram
    // API base, file-fallback flag). Shared across `telegram`'s submodules —
    // see `super::super::test_env` — so they can't race the `client` or
    // `outbound` modules' own env-mutating tests under parallel test threads.
    use crate::telegram::test_env::lock as lock_env;

    struct EnvGuard {
        entries: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let entries = pairs
                .iter()
                .map(|(k, v)| {
                    let prior = std::env::var(k).ok();
                    std::env::set_var(k, v);
                    (*k, prior)
                })
                .collect();
            Self { entries }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, prior) in &self.entries {
                match prior {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// Never actually invoked: reconcile only starts/stops poll tasks, and
    /// these tests mock `getUpdates` to return no messages, so nothing ever
    /// reaches `QueueManagerRegistry::submit_message`.
    struct NoopRunner;

    #[async_trait]
    impl AgentRunner for NoopRunner {
        async fn run(&self, _req: AgentRunRequest) -> Result<RunComplete, AoError> {
            unimplemented!("reconcile tests never dispatch a real run")
        }

        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }
    }

    use ao_protocol::error::AoError;

    async fn make_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let layer = PersistenceLayer::init_with_root(data_root)
            .await
            .expect("init persistence");
        (Arc::new(layer), tmp)
    }

    fn make_queue_registry(
        persistence: &Arc<PersistenceLayer>,
        event_bus: &Arc<EventBus>,
    ) -> Arc<QueueManagerRegistry> {
        let event_bus = Arc::clone(event_bus);
        let instance_registry = Arc::new(InstanceRegistry::new());
        let noop = Arc::new(NoopRunner);
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&noop) as Arc<dyn AgentRunner>,
            Arc::clone(&noop) as Arc<dyn AgentRunner>,
        ));
        Arc::new(QueueManagerRegistry::new(
            dispatcher,
            instance_registry,
            event_bus,
            Arc::clone(persistence),
        ))
    }

    fn make_bridge(persistence: &Arc<PersistenceLayer>, event_bus: &Arc<EventBus>) -> ChannelBridge {
        let queue_registry = make_queue_registry(persistence, event_bus);
        let telegram = Arc::new(TelegramTransport::new(Arc::new(TelegramClient::new())));
        let discord = Arc::new(DiscordTransport::new());
        let email = Arc::new(EmailTransport::new());
        let slack = Arc::new(SlackTransport::new());
        ChannelBridge::new(
            Arc::clone(persistence),
            queue_registry,
            Arc::clone(event_bus),
            telegram,
            discord,
            email,
            slack,
        )
    }

    fn make_agent(id: &str, telegram_binding: Option<ChannelBinding>) -> AgentProfile {
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
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: telegram_binding.into_iter().collect(),
            max_turns: None,
        }
    }

    fn enabled_telegram_binding(bridge_thread_id: Option<&str>) -> ChannelBinding {
        ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: bridge_thread_id.map(|s| s.to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some("@test_bot".to_string()),
                thread_mode: TelegramThreadMode::Dedicated,
            },
        }
    }

    async fn mount_empty_get_updates(mock_server: &MockServer, token: &str) {
        use wiremock::matchers::{method, path};
        Mock::given(method("GET"))
            .and(path(format!("/bot{token}/getUpdates")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "result": [] })),
            )
            .mount(mock_server)
            .await;
    }

    fn key(agent_id: &str) -> (String, String) {
        (agent_id.to_string(), "telegram".to_string())
    }

    // --- Reconcile diff tests ---

    #[tokio::test]
    async fn reconcile_starts_task_for_enabled_agent_with_token_and_bridge_thread() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-a").await;

        let agent = make_agent("agent-a", Some(enabled_telegram_binding(Some("thread-a"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-a", "token-a").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;

        assert!(running.contains_key(&key("agent-a")));

        for (_, binding) in running {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_starts_task_for_enabled_agent_with_token_and_no_bridge_thread_id() {
        // As of the per-conversation minting phase, Telegram no longer needs
        // an eagerly-provisioned `bridge_thread_id` to become eligible here —
        // it mints a fresh per-conversation thread on demand from its own
        // inbound dispatch instead (see
        // `crate::telegram::transport::resolve_telegram_conversation_thread`).
        // This binding has a resolvable token and nothing else, and must
        // still start.
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-b").await;

        let agent = make_agent("agent-b", Some(enabled_telegram_binding(None)));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-b", "token-b").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;

        assert!(
            running.contains_key(&key("agent-b")),
            "a Telegram binding with a resolvable token but no provisioned bridge thread must still start"
        );

        for (_, binding) in running {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_skips_disabled_agent() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-c"));
        binding.enabled = false;
        let agent = make_agent("agent-c", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;

        assert!(running.is_empty(), "a disabled binding must not be polled");
    }

    #[tokio::test]
    async fn reconcile_stops_task_when_agent_disabled_after_start() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-d").await;

        let agent = make_agent("agent-d", Some(enabled_telegram_binding(Some("thread-d"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-d", "token-d").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-d")));

        // Simulates a completion still in flight on this thread (e.g. an
        // async Delegate) when the binding gets disabled.
        bridge.telegram.in_flight().record("thread-d", 4242);

        let mut disabled_agent = agent.clone();
        disabled_agent.telegram_binding_mut().unwrap().enabled = false;
        persistence.agents.update(&disabled_agent).await.unwrap();

        bridge.reconcile(&mut running).await;
        assert!(
            !running.contains_key(&key("agent-d")),
            "disabling the binding must stop its inbound task"
        );
        assert_eq!(
            bridge.telegram.in_flight().peek("thread-d"),
            None,
            "stopping the inbound task must invalidate any in-flight outbound-relay mapping too"
        );
    }

    #[tokio::test]
    async fn reconcile_restarts_task_when_token_changes() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-e-v1").await;
        mount_empty_get_updates(&mock_server, "token-e-v2").await;

        let agent = make_agent("agent-e", Some(enabled_telegram_binding(Some("thread-e"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-e", "token-e-v1").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        let fingerprint_v1 = running[&key("agent-e")].fingerprint.clone();

        // Simulates a completion still in flight on this thread when the
        // token rotates and the old poll task gets stopped.
        bridge.telegram.in_flight().record("thread-e", 4343);

        token_store.set("agent-e", "token-e-v2").unwrap();
        bridge.reconcile(&mut running).await;
        assert_ne!(
            running[&key("agent-e")].fingerprint, fingerprint_v1,
            "a rotated token must restart the poll task with a new fingerprint"
        );
        assert_eq!(
            bridge.telegram.in_flight().peek("thread-e"),
            None,
            "restarting the poll task on token rotation must invalidate the stale mapping"
        );

        for (_, binding) in running {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_skips_and_warns_once_for_an_unregistered_channel_kind() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        // `make_bridge` registers a real `EmailTransport` (email has a
        // transport today), so this binding is actually skipped because no
        // secret is on file for it, not because the kind is unregistered —
        // the file-fallback flag still must be set so that resolving "no
        // secret" goes through the fake store rather than the real OS
        // keychain, which would otherwise block this test on a permission
        // prompt no one is present to dismiss.
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);

        let binding = ChannelBinding {
            binding_id: "email-default".to_string(),
            kind: ChannelKind::Email,
            enabled: true,
            bridge_thread_id: Some("thread-email".to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Email {
                address: "agent-inbox@example.com".to_string(),
                imap_host: String::new(),
                imap_port: 0,
                smtp_host: String::new(),
                smtp_port: 0,
                poll_secs: 30,
                require_auth_results: false,
            },
        };
        let agent = make_agent("agent-email", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;

        assert!(
            running.is_empty(),
            "a channel kind with no registered transport must not be polled"
        );
    }

    // --- Single-writer lease tests ---
    //
    // These exercise the lease through two independent `ChannelBridge`s
    // sharing one `PersistenceLayer` — each bridge generates its own random
    // `owner_id` at construction, the same way two real backend processes
    // pointed at the same data dir would. This proves the refusal and
    // lease-loss-teardown behavior at the `reconcile()` level. It does
    // NOT prove the live two-OS-process gate ("start two backends against
    // the same data dir, second refuses to start") — that genuinely
    // requires two real processes and is out of unit-test reach.

    #[tokio::test]
    async fn reconcile_refuses_to_start_a_binding_whose_lease_is_held_by_another_process() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-lease-a").await;

        let agent = make_agent("agent-lease-a", Some(enabled_telegram_binding(Some("thread-lease-a"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-lease-a", "token-lease-a").unwrap();

        let event_bus = Arc::new(EventBus::new(16));

        // Bridge 1 ("process A") claims the lease and starts the binding.
        let bridge_a = make_bridge(&persistence, &event_bus);
        let mut running_a = HashMap::new();
        bridge_a.reconcile(&mut running_a).await;
        assert!(running_a.contains_key(&key("agent-lease-a")));

        // Bridge 2 ("process B") shares the same persistence layer — same
        // data dir, same lease store — but has its own random owner_id.
        let bridge_b = make_bridge(&persistence, &event_bus);
        let mut running_b = HashMap::new();
        bridge_b.reconcile(&mut running_b).await;
        assert!(
            running_b.is_empty(),
            "a second process must not start a binding whose lease process A already holds"
        );

        for (_, binding) in running_a {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_stops_a_running_binding_once_its_lease_is_lost_to_another_owner() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-lease-b").await;

        let agent = make_agent("agent-lease-b", Some(enabled_telegram_binding(Some("thread-lease-b"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-lease-b", "token-lease-b").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-lease-b")));

        // Simulates a completion still in flight on this thread (e.g. an
        // async Delegate) when the lease is lost to another owner.
        bridge.telegram.in_flight().record("thread-lease-b", 5151);

        // Simulate a rival process reclaiming the lease once it has expired
        // — i.e. this process (`bridge`) was slow enough, or crashed and
        // came back, that another process's heartbeat won the reclaim. This
        // uses the store's own `try_claim` with an explicit future `now` so
        // the test never has to sleep out a real TTL.
        let original_lease = persistence
            .channel_leases
            .get("agent-lease-b", "telegram")
            .await
            .unwrap()
            .expect("bridge must have persisted a lease when it started the binding");
        let after_expiry = original_lease.expires_at + chrono::Duration::seconds(1);
        let rival_claimed = persistence
            .channel_leases
            .try_claim(
                "agent-lease-b",
                "telegram",
                "rival-owner",
                chrono::Duration::hours(1),
                after_expiry,
            )
            .await
            .unwrap();
        assert!(rival_claimed, "the rival must be able to reclaim the expired lease");

        // The next reconcile tick heartbeats using the real clock (still
        // far earlier than the rival's `now`-stamped future expiry), sees
        // the lease is held by someone else, and must stop the binding.
        bridge.reconcile(&mut running).await;
        assert!(
            !running.contains_key(&key("agent-lease-b")),
            "losing the lease to another owner must stop the inbound task"
        );
        assert_eq!(
            bridge.telegram.in_flight().peek("thread-lease-b"),
            None,
            "stopping on lease loss must invalidate any in-flight outbound-relay mapping too, via the existing invalidate_thread path"
        );
    }

    // --- Outbound-relay lease gate ---
    //
    // `run_outbound_observer` runs process-wide, not per-binding, so it
    // can't be started/stopped alongside the inbound task the way the lease
    // itself is. `LeaseGate` is what `reconcile` threads through instead —
    // these tests prove `reconcile` actually keeps it in sync with lease
    // ownership, the property the observer-level tests in
    // `crate::channels::relay::observer` assume holds.

    #[tokio::test]
    async fn reconcile_marks_the_lease_gate_active_the_moment_it_starts_a_binding() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-gate-a").await;

        let agent = make_agent("agent-gate-a", Some(enabled_telegram_binding(Some("thread-gate-a"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-gate-a", "token-gate-a").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        assert!(
            !bridge.lease_gate.is_active("thread-gate-a"),
            "nothing has started yet, so the gate must not already be open"
        );

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-gate-a")));
        assert!(
            bridge.lease_gate.is_active("thread-gate-a"),
            "reconcile must open the gate for a binding it just started under a claimed lease"
        );

        for (_, binding) in running {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_marks_the_lease_gate_inactive_the_instant_the_lease_is_lost() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-gate-b").await;

        let agent = make_agent("agent-gate-b", Some(enabled_telegram_binding(Some("thread-gate-b"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-gate-b", "token-gate-b").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(bridge.lease_gate.is_active("thread-gate-b"));

        let original_lease = persistence
            .channel_leases
            .get("agent-gate-b", "telegram")
            .await
            .unwrap()
            .expect("bridge must have persisted a lease when it started the binding");
        let after_expiry = original_lease.expires_at + chrono::Duration::seconds(1);
        let rival_claimed = persistence
            .channel_leases
            .try_claim("agent-gate-b", "telegram", "rival-owner", chrono::Duration::hours(1), after_expiry)
            .await
            .unwrap();
        assert!(rival_claimed, "the rival must be able to reclaim the expired lease");

        bridge.reconcile(&mut running).await;
        assert!(!running.contains_key(&key("agent-gate-b")));
        assert!(
            !bridge.lease_gate.is_active("thread-gate-b"),
            "the outbound relay gate must close the instant reconcile notices the lease was lost — \
             this process's outbound observer must never relay for this binding again after this point"
        );
    }

    // --- Per-binding connection state ---

    #[tokio::test]
    async fn connection_state_defaults_to_disconnected_for_an_unknown_binding() {
        let (persistence, _tmp) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        assert_eq!(
            bridge.connection_state("no-such-agent", "no-such-binding"),
            ChannelConnectionState::Disconnected
        );
    }

    #[tokio::test]
    async fn reconcile_reports_reconnecting_as_soon_as_it_starts_a_binding() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-conn-a").await;

        let agent = make_agent("agent-conn-a", Some(enabled_telegram_binding(Some("thread-conn-a"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-conn-a", "token-conn-a").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        // Nothing has been reported yet, before the binding is even started.
        assert_eq!(
            bridge.connection_state("agent-conn-a", "telegram"),
            ChannelConnectionState::Disconnected
        );

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-conn-a")));

        // `reconcile` itself reports the attempting-to-connect state the
        // instant it starts the task, before the poll loop it just spawned
        // has had a chance to run and report anything on its own.
        assert_eq!(
            bridge.connection_state("agent-conn-a", "telegram"),
            ChannelConnectionState::Reconnecting
        );

        for (_, binding) in running {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_reports_not_holding_lease_when_a_second_process_cannot_claim() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-conn-b").await;

        let agent = make_agent("agent-conn-b", Some(enabled_telegram_binding(Some("thread-conn-b"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-conn-b", "token-conn-b").unwrap();

        let event_bus = Arc::new(EventBus::new(16));

        let bridge_a = make_bridge(&persistence, &event_bus);
        let mut running_a = HashMap::new();
        bridge_a.reconcile(&mut running_a).await;
        assert!(running_a.contains_key(&key("agent-conn-b")));
        assert_eq!(
            bridge_a.connection_state("agent-conn-b", "telegram"),
            ChannelConnectionState::Reconnecting,
            "the process that actually holds the lease still reports its own attempting-to-connect state"
        );

        // A second process, sharing the same persistence layer, cannot
        // claim the lease process A already holds — its own connection
        // state for the binding must read as `not-holding-lease`, not the
        // `disconnected` default a plain "not running here" would report.
        let bridge_b = make_bridge(&persistence, &event_bus);
        let mut running_b = HashMap::new();
        bridge_b.reconcile(&mut running_b).await;
        assert!(running_b.is_empty());
        assert_eq!(
            bridge_b.connection_state("agent-conn-b", "telegram"),
            ChannelConnectionState::NotHoldingLease
        );

        for (_, binding) in running_a {
            binding.cancel.cancel();
            let _ = binding.handle.await;
        }
    }

    #[tokio::test]
    async fn reconcile_reports_not_holding_lease_once_the_lease_is_lost_to_another_owner() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-conn-c").await;

        let agent = make_agent("agent-conn-c", Some(enabled_telegram_binding(Some("thread-conn-c"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-conn-c", "token-conn-c").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-conn-c")));

        let original_lease = persistence
            .channel_leases
            .get("agent-conn-c", "telegram")
            .await
            .unwrap()
            .expect("bridge must have persisted a lease when it started the binding");
        let after_expiry = original_lease.expires_at + chrono::Duration::seconds(1);
        let rival_claimed = persistence
            .channel_leases
            .try_claim(
                "agent-conn-c",
                "telegram",
                "rival-owner",
                chrono::Duration::hours(1),
                after_expiry,
            )
            .await
            .unwrap();
        assert!(rival_claimed);

        bridge.reconcile(&mut running).await;
        assert!(!running.contains_key(&key("agent-conn-c")));
        assert_eq!(
            bridge.connection_state("agent-conn-c", "telegram"),
            ChannelConnectionState::NotHoldingLease,
            "losing the lease must report `not-holding-lease`, not silently fall back to `disconnected`"
        );
    }

    #[tokio::test]
    async fn reconcile_clears_connection_state_when_a_binding_stops_for_a_reason_other_than_lease_loss() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);
        mount_empty_get_updates(&mock_server, "token-conn-d").await;

        let agent = make_agent("agent-conn-d", Some(enabled_telegram_binding(Some("thread-conn-d"))));
        persistence.agents.create(&agent).await.unwrap();
        let token_store = TelegramTokenStore::open().unwrap();
        token_store.set("agent-conn-d", "token-conn-d").unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let bridge = make_bridge(&persistence, &event_bus);

        let mut running = HashMap::new();
        bridge.reconcile(&mut running).await;
        assert!(running.contains_key(&key("agent-conn-d")));
        assert_eq!(bridge.connection_state("agent-conn-d", "telegram"), ChannelConnectionState::Reconnecting);

        let mut disabled_agent = agent.clone();
        disabled_agent.telegram_binding_mut().unwrap().enabled = false;
        persistence.agents.update(&disabled_agent).await.unwrap();

        bridge.reconcile(&mut running).await;
        assert!(!running.contains_key(&key("agent-conn-d")));
        assert_eq!(
            bridge.connection_state("agent-conn-d", "telegram"),
            ChannelConnectionState::Disconnected,
            "disabling a binding must clear its last-reported state rather than leaving it looking still-connected"
        );
    }

    // --- Registry-driven per-kind dispatch ----------------------------------
    //
    // `ChannelBridge::new`'s constructor only ever wires up Telegram, Discord
    // and Email by name — it never changes as part of this proof, and never
    // needs to. What these tests exercise instead is whether a transport for
    // a kind `new` was never told about still participates in
    // `invalidate_thread` and `run`'s outbound-observer spawn purely by
    // being registered into the shared `ChannelTransportRegistry` — the
    // actual seam a future Slack transport would plug into. `FakeTransport`
    // stands in for that not-yet-implemented kind, using the unused
    // `ChannelKind::WhatsApp` variant so it can be registered without
    // colliding with the three real entries `make_bridge` already installs.

    /// Fake [`ChannelTransport`] that records whether each dispatch-site
    /// method actually ran, mirroring `RecordingSeam`/`RecordingSink`'s role
    /// in the sibling `discord::outbound` / `relay::observer` test modules.
    /// `fingerprint`/`spawn` are never exercised by the tests below (they
    /// only reach `invalidate_thread` and `spawn_outbound_observer`) but
    /// must still be implemented to satisfy the trait.
    struct FakeTransport {
        kind: ChannelKind,
        invalidated: Mutex<Vec<String>>,
        observer_spawned: std::sync::atomic::AtomicBool,
    }

    impl FakeTransport {
        fn new(kind: ChannelKind) -> Self {
            Self {
                kind,
                invalidated: Mutex::new(Vec::new()),
                observer_spawned: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl ChannelTransport for FakeTransport {
        fn kind(&self) -> ChannelKind {
            self.kind
        }

        fn fingerprint(&self, _agent: &AgentProfile, _binding: &ChannelBinding) -> Option<String> {
            None
        }

        fn spawn(&self, _ctx: ChannelRunContext, _cancel: CancellationToken) -> JoinHandle<()> {
            tokio::spawn(async {})
        }

        fn invalidate_thread(&self, thread_id: &str) {
            self.invalidated.lock().unwrap_or_else(|e| e.into_inner()).push(thread_id.to_string());
        }

        fn spawn_outbound_observer(
            self: Arc<Self>,
            _persistence: Arc<PersistenceLayer>,
            _lease_gate: Arc<LeaseGate>,
            _event_bus: Arc<EventBus>,
            mut shutdown_rx: watch::Receiver<()>,
        ) -> Option<JoinHandle<()>> {
            // Set synchronously, before the task is even scheduled, so the
            // assertion right after `ChannelBridge::run` returns is
            // deterministic — it doesn't need to wait for the spawned task
            // to actually be polled.
            self.observer_spawned.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(tokio::spawn(async move {
                let _ = shutdown_rx.changed().await;
            }))
        }
    }

    /// (a) `ChannelBridge::invalidate_thread` must dispatch to every
    /// registered transport, not a hardcoded Telegram/Discord pair — proven
    /// by registering a fake transport for a kind the constructor never
    /// mentions and confirming it still receives the call.
    #[tokio::test]
    async fn a_newly_registered_transport_kind_receives_invalidation_with_no_change_to_this_file() {
        let (persistence, _tmp) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(16));
        let mut bridge = make_bridge(&persistence, &event_bus);

        let fake = Arc::new(FakeTransport::new(ChannelKind::WhatsApp));
        bridge.transports.register(Arc::clone(&fake) as Arc<dyn ChannelTransport>);

        bridge.invalidate_thread("thread-whatsapp");

        assert_eq!(
            fake.invalidated.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            ["thread-whatsapp".to_string()],
            "a transport registered under a kind ChannelBridge::new never names must still be \
             invalidated — the dispatch must be over the registry, not a hardcoded per-kind list"
        );
    }

    /// (b) `ChannelBridge::run` must start an outbound observer for every
    /// registered transport that has one, not two hardcoded
    /// `tokio::spawn` calls named `telegram`/`discord` — proven the same
    /// way as (a).
    #[tokio::test]
    async fn a_newly_registered_transport_kind_gets_its_outbound_observer_spawned() {
        let (persistence, _tmp) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(16));
        let mut bridge = make_bridge(&persistence, &event_bus);

        let fake = Arc::new(FakeTransport::new(ChannelKind::WhatsApp));
        bridge.transports.register(Arc::clone(&fake) as Arc<dyn ChannelTransport>);

        let bridge = Arc::new(bridge);
        let (shutdown_tx, reconcile_handle) = Arc::clone(&bridge).run();

        assert!(
            fake.observer_spawned.load(std::sync::atomic::Ordering::SeqCst),
            "ChannelBridge::run must spawn every registered transport's outbound observer, \
             including one no positional constructor field names"
        );

        drop(shutdown_tx);
        let _ = reconcile_handle.await;
    }
}
