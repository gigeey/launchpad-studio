//! End-to-end integration tests for the Slack transport: drive
//! [`super::runner::run_slack_socket_mode_loop`] against the fake socket
//! seam ([`super::socket_seam::FakeSlackSocketSeam`]) exactly as
//! `super::runner`'s own `#[cfg(test)] mod tests` does, but additionally
//! wire the shared outbound relay ([`crate::channels::relay::observer`])
//! against a fake Slack Web API seam ([`super::fake_seam::FakeSlackApiSeam`])
//! so a whole turn — socket frame in, dispatch, agent reply, relay out — is
//! exercised without any live network. `runner`'s own tests stop at
//! dispatch (transcript-only); this module is the "reply lands
//! in-thread" / "two different channels route correctly" / "kill the
//! network → reconnect without duplicate replies" checks, end to end — plus
//! the [`crate::channels::relay::lease_gate::LeaseGate`] coverage below: a
//! holder relays, a standby process holding no lease for the binding never
//! does, and a binding that loses its lease stops relaying every one of its
//! per-conversation threads on the very next event, not just the one it was
//! mid-turn on.
//!
//! Lease registration itself is real production wiring here, not a
//! test-only stand-in: `super::runner::resolve_bridge_thread` calls
//! `ChannelRunContext::lease_gate`'s `mark_active` the moment it resolves or
//! mints each per-conversation thread id, exactly as it would in `ao-server`
//! — this harness's [`TestHarness::ctx`] just hands it the same `LeaseGate`
//! instance the relay pump below checks against.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_provider_config::{ChannelSecretStore, SLACK_BOT_TOKEN_SECRET_ROLE};
use ao_protocol::agent::{
    AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat,
    ProviderConfig, SlackConversationMode,
};
use ao_protocol::error::AoError;
use ao_protocol::event::{AgentEventPayload, RunEndReason};

use crate::agent_runner::{AgentRunRequest, AgentRunner, AgentRunnerMode, RunComplete, RunnerDispatcher};
use crate::channels::connection_state::ConnectionStateRegistry;
use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::relay::observer::{handle_relay_event, RelaySink};
use crate::event_bus::EventBus;
use crate::instance_registry::InstanceRegistry;
use crate::queue_manager::QueueManagerRegistry;

use super::fake_seam::FakeSlackApiSeam;
use super::runner::{run_slack_socket_mode_loop, SlackSeamFactory};
use super::socket_seam::{FakeSlackSocketSeam, SlackSocketSeam, SlackSocketSeamError, SocketFrame};
use super::web_api_seam::SlackApiCallError;
use super::{relay_reply, SlackOrigin, SlackTransport};

const AGENT_ID: &str = "agent-slack-e2e";
const BINDING_ID: &str = "slack";
const TEAM_ID: &str = "T1";
const BOT_USER_ID: &str = "U0BOT";

// --- env / secret store test scaffolding (mirrors `super::tests`' own
// copy — a separate sibling test module can't reach a private helper
// defined inside another file's inline `mod tests`, so this is
// duplicated rather than shared) ---------------------------------------

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

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_secret(agent_id: &str, binding_id: &str, role: &str, value: &str) {
    ChannelSecretStore::open().expect("secret store opens").set(agent_id, binding_id, role, value).expect("secret stored");
}

// --- harness -------------------------------------------------------------

/// A stub [`AgentRunner`] that completes immediately without spawning a
/// real process, mirroring `super::runner::tests::StubRunner` — but also
/// emits the `TextComplete` + `RunEnded` pair a real runner would put on
/// the shared [`EventBus`], since this module's tests need the outbound
/// relay (which is driven purely off that bus) to actually fire.
struct RelayingStubRunner {
    event_bus: Arc<EventBus>,
}

#[async_trait]
impl AgentRunner for RelayingStubRunner {
    fn mode(&self) -> AgentRunnerMode {
        AgentRunnerMode::Cli
    }

    async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
        let run_id = req.pre_registered_run_id.clone().unwrap_or_else(|| "test-run".to_string());
        let thread_id = req.thread_id.clone();
        let reply_text = format!("reply: {}", req.prompt);
        self.event_bus
            .emit(&run_id, &req.agent.id, thread_id.clone(), AgentEventPayload::TextComplete { text: reply_text.clone() })
            .await;
        self.event_bus.emit(&run_id, &req.agent.id, thread_id, AgentEventPayload::RunEnded { reason: RunEndReason::Completed }).await;

        let rc = RunComplete { run_id, output_text: reply_text, workflow_followups: vec![], end_reason: RunEndReason::Completed };
        let _ = req.run_complete_tx.send(rc.clone()).await;
        Ok(rc)
    }
}

/// Bridges [`SlackOrigin`] relays to [`super::relay_reply`] against a fake
/// Slack Web API seam — the hermetic stand-in for [`SlackTransport`]'s real
/// `spawn_outbound_observer` wiring, which hardcodes a live
/// [`super::web_api_seam::ReqwestSlackApiSeam`] and so cannot be driven in a
/// test without a real network call.
struct RecordingRelaySink {
    transport: SlackTransport,
    seam: FakeSlackApiSeam,
}

#[async_trait]
impl RelaySink<SlackOrigin> for RecordingRelaySink {
    async fn relay(&self, agent_id: &str, origin: &SlackOrigin, text: &str) {
        relay_reply(&self.transport, &self.seam, agent_id, origin, text).await;
    }
}

/// Shared backing state for one test's [`crate::channels::ChannelRunContext`]s
/// — mirrors `super::runner::tests::TestHarness`.
struct TestHarness {
    persistence: Arc<ao_persistence::PersistenceLayer>,
    event_bus: Arc<EventBus>,
    queue_registry: Arc<QueueManagerRegistry>,
    connection_state: Arc<ConnectionStateRegistry>,
    lease_gate: Arc<LeaseGate>,
    in_flight: Arc<CorrelationMap<SlackOrigin>>,
    _tmp: tempfile::TempDir,
}

impl TestHarness {
    fn ctx(&self) -> crate::channels::ChannelRunContext {
        crate::channels::ChannelRunContext {
            agent_id: AGENT_ID.to_string(),
            binding_id: BINDING_ID.to_string(),
            persistence: Arc::clone(&self.persistence),
            queue_registry: Arc::clone(&self.queue_registry),
            connection_state: Arc::clone(&self.connection_state),
            lease_gate: Arc::clone(&self.lease_gate),
            event_bus: Arc::clone(&self.event_bus),
        }
    }
}

async fn make_test_harness(agent: AgentProfile) -> TestHarness {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    let persistence = Arc::new(ao_persistence::PersistenceLayer::init_with_root(data_root).await.expect("init persistence"));
    persistence.agents.create(&agent).await.expect("create agent");

    let event_bus = Arc::new(EventBus::new(64));
    let lease_gate = Arc::new(LeaseGate::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let stub = Arc::new(RelayingStubRunner { event_bus: Arc::clone(&event_bus) });
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        Arc::clone(&stub) as Arc<dyn AgentRunner>,
        Arc::clone(&stub) as Arc<dyn AgentRunner>,
    ));
    let queue_registry =
        Arc::new(QueueManagerRegistry::new(dispatcher, instance_registry, Arc::clone(&event_bus), Arc::clone(&persistence)));
    let connection_state = Arc::new(ConnectionStateRegistry::new());
    let in_flight = Arc::new(CorrelationMap::new());

    TestHarness { persistence, event_bus, queue_registry, connection_state, lease_gate, in_flight, _tmp: tmp }
}

fn make_test_agent(allowed_channels: Vec<String>, allowed_users: Vec<String>) -> AgentProfile {
    AgentProfile {
        id: AGENT_ID.to_string(),
        name: "Slack E2E Test Agent".to_string(),
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
        channels: vec![ChannelBinding {
            binding_id: BINDING_ID.to_string(),
            kind: ChannelKind::Slack,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Slack {
                allowed_channels,
                allowed_users,
                connection_id: None,
                conversation_mode: SlackConversationMode::PerConversation,
            },
        }],
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        max_turns: None,
}
}

/// Builds a [`RecordingRelaySink`] with a bot token already on file, so
/// [`relay_reply`] actually reaches the fake Web API seam instead of
/// bailing out on "no token on file".
fn make_relay_sink() -> Arc<RecordingRelaySink> {
    set_secret(AGENT_ID, BINDING_ID, SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-fake");
    let seam = FakeSlackApiSeam::new(Err(SlackApiCallError::Auth("unused".to_string())), Ok(()));
    Arc::new(RecordingRelaySink { transport: SlackTransport::new(), seam })
}

/// Subscribes to `event_bus` **synchronously**, before returning, then hands
/// the receiver to a spawned pump loop that calls
/// [`handle_relay_event`] directly for each event. Deliberately does not use
/// [`crate::channels::relay::observer::run_relay_observer`] — see that
/// function's own doc: `subscribe()` inside a freshly `tokio::spawn`ed task
/// races the caller's own subsequent emits, exactly the flakiness
/// `handle_relay_event` exists to let tests avoid. Calling `event_bus.subscribe()`
/// here, before this function returns, closes that race for good: the
/// `RelayingStubRunner`'s emits (which only happen once this harness's
/// socket loop has dispatched an inbound message, several await points
/// later) can never land before this subscription exists.
fn spawn_relay_pump(
    event_bus: &EventBus,
    lease_gate: Arc<LeaseGate>,
    in_flight: Arc<CorrelationMap<SlackOrigin>>,
    sink: Arc<RecordingRelaySink>,
) -> (JoinHandle<()>, watch::Sender<()>) {
    let mut events = event_bus.subscribe();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let handle = tokio::spawn(async move {
        let mut pending_text: HashMap<String, String> = HashMap::new();
        let mut last_relayed: HashMap<String, String> = HashMap::new();
        loop {
            let event = tokio::select! {
                _ = shutdown_rx.changed() => return,
                event = events.recv() => event,
            };
            let Ok(event) = event else { return };
            handle_relay_event(&lease_gate, &in_flight, sink.as_ref(), event, &mut pending_text, &mut last_relayed).await;
        }
    });
    (handle, shutdown_tx)
}

fn seam_factory_from(seams: Vec<Box<dyn SlackSocketSeam>>) -> SlackSeamFactory {
    let queue = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(seams)));
    Arc::new(move || -> Box<dyn SlackSocketSeam> {
        let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
        match queue.pop_front() {
            Some(seam) => seam,
            None => Box::new(FakeSlackSocketSeam::new(
                Err(SlackSocketSeamError::ConnectionsOpen("no more scripted seams".to_string())),
                vec![],
            )),
        }
    })
}

fn hello_frame() -> SocketFrame {
    SocketFrame::Text(r#"{"type":"hello"}"#.to_string())
}

fn disconnect_frame(reason: &str) -> SocketFrame {
    SocketFrame::Text(format!(r#"{{"type":"disconnect","reason":"{reason}"}}"#))
}

fn app_mention_frame(envelope_id: &str, event_id: &str, channel: &str, user: &str, text: &str) -> SocketFrame {
    SocketFrame::Text(format!(
        r#"{{
            "envelope_id":"{envelope_id}",
            "payload":{{
                "event":{{"type":"app_mention","channel":"{channel}","user":"{user}","text":"{text}","ts":"1701234567.000100","team":"{TEAM_ID}"}},
                "type":"event_callback","event_id":"{event_id}"
            }},
            "type":"events_api"
        }}"#
    ))
}

fn dm_message_frame(envelope_id: &str, event_id: &str, channel: &str, user: &str, text: &str) -> SocketFrame {
    SocketFrame::Text(format!(
        r#"{{
            "envelope_id":"{envelope_id}",
            "payload":{{
                "event":{{"type":"message","channel":"{channel}","user":"{user}","text":"{text}","ts":"1701234999.000500","team":"{TEAM_ID}"}},
                "type":"event_callback","event_id":"{event_id}"
            }},
            "type":"events_api"
        }}"#
    ))
}

/// Polls `check` until it returns `Some`, or gives up after ~2 seconds —
/// resilient to scheduling jitter without a fixed, arbitrary sleep. Mirrors
/// `super::runner::tests::wait_for`.
async fn wait_for<F, Fut, T>(mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..200 {
        if let Some(value) = check().await {
            return Some(value);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

async fn transcript_text_for(ctx: &crate::channels::ChannelRunContext, thread_id: &str) -> Option<String> {
    let thread = ctx.persistence.threads.get(thread_id).await.ok()??;
    let entries = ctx.persistence.transcripts.read_all_at(&std::path::PathBuf::from(&thread.transcript_path)).await.ok()?;
    Some(entries.into_iter().map(|e| e.content).collect::<Vec<_>>().join("\n"))
}

// --- 1. app_mention -> reply produced, outbound relay attempted ---------

#[tokio::test]
async fn app_mention_produces_a_reply_and_the_outbound_relay_is_attempted() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C123".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![hello_frame(), app_mention_frame("env-1", "Ev001", "C123", "U456", "hello there")],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("conversation row must be provisioned once the mention is dispatched");

    let text = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("transcript entry must be written for the dispatched mention");
    assert!(text.contains("hello there"), "the inbound mention must be dispatched into the bridge thread, got: {text}");

    let calls = wait_for(|| {
        let sink = Arc::clone(&sink);
        async move {
            let calls = sink.seam.post_message_calls();
            (!calls.is_empty()).then_some(calls)
        }
    })
    .await
    .expect("the agent's reply must be relayed outbound through the fake Slack Web API seam");
    assert_eq!(calls.len(), 1, "exactly one outbound relay call expected, got {calls:?}");
    let (channel, thread_ts, relayed_text) = &calls[0];
    assert_eq!(channel, "C123", "the reply must be relayed back to the channel the mention came from");
    assert_eq!(
        thread_ts.as_deref(),
        Some("1701234567.000100"),
        "a fresh top-level mention's reply must thread under the mention's own ts"
    );
    assert!(relayed_text.contains("hello there"), "the relayed text must reflect the agent's reply to the mention");

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 2. DM -> reply -------------------------------------------------------

#[tokio::test]
async fn a_dm_produces_a_reply_relayed_back_to_the_dm_channel_with_no_thread() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    // DM channel ids live in Slack's `D` namespace.
    let agent = make_test_agent(vec!["D555".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![hello_frame(), dm_message_frame("env-dm", "Ev-DM", "D555", "U456", "hi from a DM")],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    // A DM collapses to the channel id alone — no thread_ts.
    let row = wait_for(|| async { ctx.persistence.slack_conversations.get(TEAM_ID, "D555", None).await.ok().flatten() })
        .await
        .expect("a DM conversation row must be provisioned keyed on the channel alone");

    let text = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("transcript entry must be written for the dispatched DM");
    assert!(text.contains("hi from a DM"));

    // The minted thread must be stamped with a `channel_origin` naming this
    // Slack binding — otherwise this per-conversation thread is invisible to
    // both the composer-gating hint and `is_channel_bridge_thread`'s
    // tool-admission gate, since Slack never populates a `bridge_thread_id`
    // to reverse-look-up from (see `ChannelBridgeOrigin`'s docstring).
    let thread = ctx
        .persistence
        .threads
        .get(&row.thread_id)
        .await
        .expect("thread lookup must not error")
        .expect("the minted bridge thread must exist");
    let origin = thread.channel_origin.expect("a freshly-minted Slack bridge thread must carry a channel_origin");
    assert_eq!(origin.kind, ao_protocol::agent::ChannelKind::Slack);
    assert_eq!(origin.binding_id, BINDING_ID);

    let calls = wait_for(|| {
        let sink = Arc::clone(&sink);
        async move {
            let calls = sink.seam.post_message_calls();
            (!calls.is_empty()).then_some(calls)
        }
    })
    .await
    .expect("the agent's DM reply must be relayed outbound");
    assert_eq!(calls.len(), 1);
    let (channel, thread_ts, relayed_text) = &calls[0];
    assert_eq!(channel, "D555");
    assert_eq!(thread_ts, &None, "a DM reply posts at the top level, never inside a thread");
    assert!(relayed_text.contains("hi from a DM"));

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 3. Two channels / two conversations -> distinct threads, no cross-talk

#[tokio::test]
async fn two_channels_route_to_distinct_threads_and_do_not_cross_talk() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C-A".to_string(), "C-B".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![
            hello_frame(),
            app_mention_frame("env-a", "Ev-A", "C-A", "U456", "message for channel A"),
            app_mention_frame("env-b", "Ev-B", "C-B", "U789", "message for channel B"),
        ],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row_a = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C-A", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("channel A's conversation row must be provisioned");
    let row_b = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C-B", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("channel B's conversation row must be provisioned");

    assert_ne!(
        row_a.thread_id, row_b.thread_id,
        "two distinct Slack conversations (different channels) must map to two distinct Launchpad threads (P2b registry)"
    );

    let text_a = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row_a.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("channel A's transcript must exist");
    let text_b = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row_b.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("channel B's transcript must exist");

    assert!(text_a.contains("message for channel A"));
    assert!(!text_a.contains("message for channel B"), "channel A's thread must never see channel B's inbound text");
    assert!(text_b.contains("message for channel B"));
    assert!(!text_b.contains("message for channel A"), "channel B's thread must never see channel A's inbound text");

    let calls = wait_for(|| {
        let sink = Arc::clone(&sink);
        async move {
            let calls = sink.seam.post_message_calls();
            (calls.len() >= 2).then_some(calls)
        }
    })
    .await
    .expect("both channels' replies must be relayed outbound");
    assert_eq!(calls.len(), 2, "exactly one relay call per channel expected, got {calls:?}");

    let call_a = calls.iter().find(|(channel, ..)| channel == "C-A").expect("a relay call targeting channel A");
    let call_b = calls.iter().find(|(channel, ..)| channel == "C-B").expect("a relay call targeting channel B");
    assert!(call_a.2.contains("message for channel A"));
    assert!(!call_a.2.contains("message for channel B"), "channel A's relayed reply must never carry channel B's content");
    assert!(call_b.2.contains("message for channel B"));
    assert!(!call_b.2.contains("message for channel A"), "channel B's relayed reply must never carry channel A's content");

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 3b. Two mentions in the SAME thread -> the second (a `resolve_bridge_thread`
//         *load*, not a mint) must still be dispatched and relayed. Neither
//         `app_mention_frame` above nor any other fixture carries a
//         `thread_ts`, so this is the only coverage of the load path today. ---

fn app_mention_in_thread_frame(envelope_id: &str, event_id: &str, channel: &str, user: &str, text: &str, ts: &str, thread_ts: &str) -> SocketFrame {
    SocketFrame::Text(format!(
        r#"{{
            "envelope_id":"{envelope_id}",
            "payload":{{
                "event":{{"type":"app_mention","channel":"{channel}","user":"{user}","text":"{text}","ts":"{ts}","thread_ts":"{thread_ts}","team":"{TEAM_ID}"}},
                "type":"event_callback","event_id":"{event_id}"
            }},
            "type":"events_api"
        }}"#
    ))
}

#[tokio::test]
async fn two_mentions_in_the_same_thread_are_both_dispatched_and_relayed() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C123".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let thread_root = "1700000000.000001";
    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![
            hello_frame(),
            app_mention_in_thread_frame("env-1", "Ev-1", "C123", "U456", "first mention", "1700000000.000005", thread_root),
            app_mention_in_thread_frame("env-2", "Ev-2", "C123", "U789", "second mention", "1700000000.000010", thread_root),
        ],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some(thread_root)).await.ok().flatten()
    })
    .await
    .expect("conversation row must be provisioned once the first mention is dispatched");

    let text = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("transcript entries must be written for both mentions");
    assert!(text.contains("first mention"), "got: {text}");

    let text = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await.filter(|t| t.contains("second mention")) }
    })
    .await
    .unwrap_or(text);
    assert!(
        text.contains("second mention"),
        "the second mention in the same thread (a resolve_bridge_thread LOAD, not a mint) must also be dispatched, got: {text}"
    );

    let calls = wait_for(|| {
        let sink = Arc::clone(&sink);
        async move {
            let calls = sink.seam.post_message_calls();
            (calls.len() >= 2).then_some(calls)
        }
    })
    .await
    .unwrap_or_else(|| sink.seam.post_message_calls());
    assert_eq!(
        calls.len(),
        2,
        "exactly one relay call per same-thread mention expected, got {calls:?} (only {} relayed)",
        calls.len()
    );

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 4. REQUIRED CASE: disconnect -> warm rotation -> same event_id
//        redelivered across the rotation -> dispatched AND relayed exactly
//        once, not merely "eventually". This is the acceptance gate. ---

#[tokio::test]
async fn disconnect_triggers_warm_rotation_and_the_redelivered_event_id_is_dispatched_and_relayed_exactly_once() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C123".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    // `active`: hello, a routine disconnect (triggers the warm rotation),
    // then the SAME event_id redelivered on this socket too.
    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![
            hello_frame(),
            disconnect_frame("refresh_requested"),
            app_mention_frame("env-dup-active", "Ev-dup", "C123", "U456", "warm rotation payload"),
        ],
    );
    // `incoming`: its own hello (promotes it), then the SAME event_id again —
    // proving the overlap is safe by dedup, not by ack timing.
    let incoming = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket-2",
        vec![hello_frame(), app_mention_frame("env-dup-incoming", "Ev-dup", "C123", "U456", "warm rotation payload")],
    );

    let seam_factory = seam_factory_from(vec![Box::new(active), Box::new(incoming)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("conversation row must be provisioned once the mention is dispatched");

    // Let any (would-be) second delivery / second relay attempt run through
    // too, rather than asserting the instant the first one lands.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let text = transcript_text_for(&ctx, &row.thread_id).await.unwrap_or_default();
    assert_eq!(
        text.matches("warm rotation payload").count(),
        1,
        "the same event_id delivered on both the old and new socket must be DISPATCHED exactly once, transcript was: {text}"
    );

    let calls = sink.seam.post_message_calls();
    let relayed_for_event: Vec<_> = calls.iter().filter(|(channel, _, text)| channel == "C123" && text.contains("warm rotation payload")).collect();
    assert_eq!(
        relayed_for_event.len(),
        1,
        "the same event_id redelivered across the warm-rotation socket handoff must be RELAYED exactly once, not once per delivery — got {calls:?}"
    );

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 5. LeaseGate: a standby process (no lease) never relays -------------

/// The negative half of the `LeaseGate` contract: the holder process's own
/// inbound dispatch registers the per-conversation thread id via
/// `resolve_bridge_thread`'s real `ctx.lease_gate.mark_active` call, but a
/// second process's outbound observer — modeled here as a relay pump driven
/// off a completely separate `LeaseGate` instance that was never told about
/// this binding at all — must still never relay the reply that produces.
#[tokio::test]
async fn a_standby_process_holding_no_lease_never_relays_a_reply() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C123".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    // Deliberately NOT `harness.lease_gate` — a standby process holds its
    // own `LeaseGate`, never told this binding is active anywhere.
    let standby_lease_gate = Arc::new(LeaseGate::new());
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&standby_lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![hello_frame(), app_mention_frame("env-standby", "Ev-standby", "C123", "U456", "should never reach a standby")],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    // This is the "holder" process: its own `ChannelRunContext::lease_gate`
    // is `harness.lease_gate`, and `resolve_bridge_thread` marks it active
    // for real, exactly as it would in production.
    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("conversation row must be provisioned once the mention is dispatched");

    let text = wait_for(|| {
        let ctx = &ctx;
        let thread_id = row.thread_id.clone();
        async move { transcript_text_for(ctx, &thread_id).await }
    })
    .await
    .expect("transcript entry must be written for the dispatched mention — dispatch itself does not depend on the lease");
    assert!(text.contains("should never reach a standby"));

    assert!(
        harness.lease_gate.is_active(&row.thread_id),
        "the holder process's own lease_gate must have registered this per-conversation thread"
    );
    assert!(
        !standby_lease_gate.is_active(&row.thread_id),
        "a lease_gate belonging to a different process must never learn about this thread"
    );

    // No `wait_for` here on purpose — there's nothing to eventually appear;
    // a fixed pause is how the test proves a negative (the reply the stub
    // runner already emitted on the shared event bus had every chance to
    // reach the standby's pump and never should).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        sink.seam.post_message_calls().is_empty(),
        "a standby process holding no lease for this binding must never relay, even though the holder dispatched and replied normally"
    );

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}

// --- 6. LeaseGate: losing the binding's lease stops relaying every one of
//        its conversations, not just the one active at the moment of loss --

/// [`crate::channels::relay::lease_gate::LeaseGate::mark_inactive`] is keyed
/// on the binding, not on any single thread id — this is the real-wiring
/// proof that losing a binding's lease (mirroring
/// `crate::telegram::bridge::ChannelBridge::reconcile`'s stop path) clears
/// EVERY per-conversation thread that binding ever registered, in one call,
/// and that a reply arriving afterward for any of them is dropped — not just
/// the specific conversation that happened to be in flight when the lease
/// was lost.
#[tokio::test]
async fn losing_the_bindings_lease_stops_relaying_every_conversation_it_had() {
    let _guard = lock_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(&[
        (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
        ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
    ]);

    let agent = make_test_agent(vec!["C-A".to_string(), "C-B".to_string()], vec![]);
    let harness = make_test_harness(agent).await;
    let sink = make_relay_sink();
    let (relay_handle, relay_shutdown) =
        spawn_relay_pump(&harness.event_bus, Arc::clone(&harness.lease_gate), Arc::clone(&harness.in_flight), Arc::clone(&sink));

    let active = FakeSlackSocketSeam::connects_to(
        "wss://example.slack.com/socket",
        vec![
            hello_frame(),
            app_mention_frame("env-a", "Ev-A", "C-A", "U456", "message for channel A"),
            app_mention_frame("env-b", "Ev-B", "C-B", "U789", "message for channel B"),
        ],
    );
    let seam_factory = seam_factory_from(vec![Box::new(active)]);
    let cancel = CancellationToken::new();

    let inbound_handle = tokio::spawn(run_slack_socket_mode_loop(
        harness.ctx(),
        "xapp-fake".to_string(),
        TEAM_ID.to_string(),
        BOT_USER_ID.to_string(),
        Arc::clone(&harness.in_flight),
        seam_factory,
        cancel.clone(),
    ));

    let ctx = harness.ctx();
    let row_a = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C-A", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("channel A's conversation row must be provisioned");
    let row_b = wait_for(|| async {
        ctx.persistence.slack_conversations.get(TEAM_ID, "C-B", Some("1701234567.000100")).await.ok().flatten()
    })
    .await
    .expect("channel B's conversation row must be provisioned");

    let calls = wait_for(|| {
        let sink = Arc::clone(&sink);
        async move {
            let calls = sink.seam.post_message_calls();
            (calls.len() >= 2).then_some(calls)
        }
    })
    .await
    .expect("both conversations must relay normally while the binding still holds its lease");
    assert_eq!(calls.len(), 2);

    assert!(harness.lease_gate.is_active(&row_a.thread_id));
    assert!(harness.lease_gate.is_active(&row_b.thread_id));

    // Mirrors `ChannelBridge::reconcile`'s stop path: the binding's lease is
    // lost (or the binding is torn down), so every thread it ever registered
    // must clear together, in this one call.
    harness.lease_gate.mark_inactive(BINDING_ID);

    assert!(!harness.lease_gate.is_active(&row_a.thread_id), "losing the binding's lease must clear channel A's thread too");
    assert!(!harness.lease_gate.is_active(&row_b.thread_id), "losing the binding's lease must clear channel B's thread too");

    // A stray late completion on EACH conversation — mirroring the
    // async-Delegate hand-off race `relay::observer`'s own tests cover
    // (`CorrelationMap` is peeked, never consumed, so a second, independent
    // `RunEnded` on an already-answered thread still resolves an origin) —
    // must not relay on either thread now that the binding's lease is gone.
    for thread_id in [row_a.thread_id.clone(), row_b.thread_id.clone()] {
        let run_id = format!("late-run-{thread_id}");
        harness
            .event_bus
            .emit(&run_id, &AGENT_ID.to_string(), Some(thread_id.clone()), AgentEventPayload::TextComplete {
                text: "a reply arriving after the lease was lost".to_string(),
            })
            .await;
        harness
            .event_bus
            .emit(&run_id, &AGENT_ID.to_string(), Some(thread_id), AgentEventPayload::RunEnded { reason: RunEndReason::Completed })
            .await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sink.seam.post_message_calls().len(),
        2,
        "no additional relay may happen for either conversation once the binding's lease is lost"
    );

    cancel.cancel();
    let _ = inbound_handle.await;
    let _ = relay_shutdown.send(());
    let _ = relay_handle.await;
}
