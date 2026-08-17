//! Core per-route webhook dispatch loop: pre-agent relevance gating (the
//! `events` allowlist plus declarative `filters`), delivery-id dedup,
//! `prompt_template` rendering, and `deliver_only`/`github_comment` routing —
//! shared by the HTTP gateway handler
//! (`ao-server::routes::webhooks::handle_webhook`) and directly unit-tested
//! here with a plain [`NotificationDispatcher`], the same way
//! [`crate::assignment_runner::fire_assignment`] is tested.
//!
//! Kept independent of any HTTP framework type: callers hand in the already
//! parsed JSON payload plus whatever headers they've already extracted
//! (event type, delivery id) rather than a raw request.

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;
use tracing::{info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::assignment::{
    Assignment, AssignmentTrigger, AssignmentTriggerKind, TriggerEventContext, WebhookDeliverTarget,
};
use ao_protocol::webhook_filter::event_type_allowed;
use ao_protocol::webhook_template::render_prompt_template;

use crate::assignment_runner::fire_assignment;
use crate::event_bus::EventBus;
use crate::github_comment::deliver_github_comment;
use crate::queue_manager::NotificationDispatcher;

/// Tally of what happened to every assignment sharing an inbound route,
/// returned to the HTTP handler for the response body.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebhookDispatchTally {
    /// Assignments sharing this route, before any gating.
    pub matched: usize,
    /// Passed relevance gating and dedup; the agent was started.
    pub fired: usize,
    /// Passed relevance gating and dedup, but `deliver` skipped the agent
    /// (`DeliverOnly` or `GithubComment`).
    pub delivered: usize,
    /// Dropped by the `events` allowlist or declarative `filters` — no
    /// agent run, no delivery-id bookkeeping. Zero tokens spent.
    pub filtered: usize,
    /// A delivery id already seen for that assignment (a provider retry).
    pub deduped: usize,
}

/// Runs every enabled assignment in `route_assignments` through relevance
/// gating, delivery-id dedup, template rendering, and firing/delivery.
///
/// `payload` is the parsed JSON body. `payload_summary` is the truncated
/// raw-body excerpt recorded verbatim on the `AssignmentRun.trigger_payload`
/// column (unchanged legacy convention — separate from the rendered
/// instruction, which flows through `TriggerEventContext` instead).
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_webhook_route(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    route_assignments: &[Assignment],
    route_name: &str,
    event_type: Option<&str>,
    payload: &Value,
    payload_summary: &str,
    delivery_id: Option<&str>,
    timezone: Option<&str>,
) -> WebhookDispatchTally {
    let mut tally = WebhookDispatchTally {
        matched: route_assignments.len(),
        ..Default::default()
    };

    for assignment in route_assignments.iter().filter(|a| a.enabled) {
        let AssignmentTrigger::Webhook { events, filters, prompt_template, deliver, .. } = &assignment.trigger
        else {
            continue;
        };

        // 0. Skip and disable expired assignments, mirroring the cron,
        //    connector-event, and agent-watch dispatch paths in
        //    `ScheduleRunner` — those trigger types are ticked and can catch
        //    their own expiry, but a webhook only runs when a request lands,
        //    so this check has to happen here instead.
        if let Some(expires_at) = assignment.expires_at {
            if expires_at < Utc::now() {
                tally.filtered += 1;
                if let Err(e) = disable_expired_assignment(persistence, &assignment.id).await {
                    warn!(
                        "webhook route {route_name}: failed to disable expired assignment {}: {e}",
                        assignment.id
                    );
                }
                continue;
            }
        }

        // 1. Pre-agent relevance gating — zero tokens. Dropped here, before
        //    any dedup bookkeeping: a filtered-out delivery has nothing
        //    worth remembering, and a provider retry of it will just be
        //    filtered again.
        let filters_match = filters.as_ref().map(|f| f.matches(payload)).unwrap_or(true);
        if !event_type_allowed(events, event_type) || !filters_match {
            tally.filtered += 1;
            continue;
        }

        // 2. Delivery-id dedup — a provider retry of an already-processed
        //    delivery is a no-op, not a re-fire.
        if let Some(id) = delivery_id {
            match persistence.assignment_scratchpads.has_seen_delivery(&assignment.id, id).await {
                Ok(true) => {
                    tally.deduped += 1;
                    continue;
                }
                Ok(false) => {
                    if let Err(e) = persistence.assignment_scratchpads.record_delivery(&assignment.id, id).await {
                        warn!(
                            "webhook route {route_name}: failed to record delivery id for assignment {}: {e}",
                            assignment.id
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "webhook route {route_name}: delivery dedup check failed for assignment {}: {e}",
                        assignment.id
                    );
                }
            }
        }

        // 3. Render the route's prompt template (if any) against the
        //    payload, and package the raw event for TriggerEventContext.
        let rendered_instruction = prompt_template.as_deref().map(|tpl| render_prompt_template(tpl, payload));
        let summary = match event_type {
            Some(et) => format!("Webhook event `{et}` on route `{route_name}`"),
            None => format!("Webhook event on route `{route_name}`"),
        };
        let event_context = TriggerEventContext { summary, payload: payload.clone() };

        match deliver {
            WebhookDeliverTarget::Agent => {
                // The rendered template (when set) replaces the assignment's
                // static instruction for this one fire — `fire_assignment`
                // still layers the event summary + raw payload on top via
                // `event_context`, so the agent sees both the tailored
                // instruction and the full event data.
                let mut fire_target = assignment.clone();
                if let Some(instruction) = &rendered_instruction {
                    fire_target.instruction = instruction.clone();
                }
                match fire_assignment(
                    persistence,
                    dispatcher,
                    event_bus,
                    &fire_target,
                    AssignmentTriggerKind::Webhook,
                    Some(payload_summary.to_string()),
                    timezone,
                    Some(event_context),
                )
                .await
                {
                    Ok(_) => tally.fired += 1,
                    Err(e) => {
                        warn!("webhook route {route_name}: dispatch failed for assignment {}: {e}", assignment.id)
                    }
                }
            }
            WebhookDeliverTarget::DeliverOnly => {
                info!(
                    "webhook route {route_name}: delivered without starting an agent run for assignment {} — {}",
                    assignment.id,
                    rendered_instruction.as_deref().unwrap_or(event_context.summary.as_str())
                );
                tally.delivered += 1;
            }
            WebhookDeliverTarget::GithubComment => {
                // Resolves repo/PR straight from the payload and posts via
                // `gh pr comment` (`crate::github_comment`). Counts as
                // `delivered` regardless of whether the underlying `gh` call
                // actually succeeds — like `DeliverOnly`, this branch's job
                // is "route here instead of firing the agent," not "the
                // external POST succeeded"; a failure (bad repo/PR shape,
                // `gh` missing, network error) is logged, not silently
                // swallowed, but doesn't turn into an agent run either.
                let comment_body = rendered_instruction.as_deref().unwrap_or(event_context.summary.as_str());
                match deliver_github_comment(payload, comment_body).await {
                    Ok(()) => {
                        info!("webhook route {route_name}: posted github_comment for assignment {}", assignment.id)
                    }
                    Err(e) => warn!(
                        "webhook route {route_name}: github_comment delivery failed for assignment {}: {e}",
                        assignment.id
                    ),
                }
                tally.delivered += 1;
            }
        }
    }

    tally
}

/// Flips `enabled` to `false` for an expired assignment — the same
/// disable-on-expiry treatment `ScheduleRunner::disable_assignment` applies
/// to cron, connector-event, and agent-watch triggers, reimplemented here
/// since the webhook path has no `ScheduleRunner` instance to call through.
async fn disable_expired_assignment(
    persistence: &Arc<PersistenceLayer>,
    assignment_id: &str,
) -> Result<(), ao_protocol::error::AoError> {
    if let Some(mut assignment) = persistence.assignments.get(assignment_id).await {
        assignment.enabled = false;
        persistence.assignments.update(assignment).await
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;

    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::assignment::{AssignmentRunStatus, AssignmentThreadPolicy, OutputMode};
    use ao_protocol::error::AoError;
    use ao_protocol::message::QueuedMessage;
    use ao_protocol::webhook_filter::{WebhookFieldFilter, WebhookFilter, WebhookFilterOp};

    struct RecordingDispatcher {
        tx: mpsc::Sender<(String, QueuedMessage)>,
    }

    #[async_trait]
    impl NotificationDispatcher for RecordingDispatcher {
        async fn submit_to_agent(&self, agent_id: &str, message: QueuedMessage) -> Result<(), AoError> {
            self.tx
                .send((agent_id.to_string(), message))
                .await
                .map_err(|e| AoError::Internal(format!("recording dispatcher send error: {e}")))?;
            Ok(())
        }
    }

    fn make_recording_dispatcher() -> (Arc<dyn NotificationDispatcher>, mpsc::Receiver<(String, QueuedMessage)>) {
        let (tx, rx) = mpsc::channel(16);
        (Arc::new(RecordingDispatcher { tx }) as Arc<dyn NotificationDispatcher>, rx)
    }

    async fn make_persistence() -> (tempfile::TempDir, Arc<PersistenceLayer>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let layer = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
        (tmp, Arc::new(layer))
    }

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec!["ok".to_string()],
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
            channels: vec![],
            max_turns: None,
        }
    }

    fn webhook_assignment(
        id: &str,
        agent_id: &str,
        route_name: &str,
        events: Vec<String>,
        filters: Option<WebhookFilter>,
        prompt_template: Option<String>,
        deliver: WebhookDeliverTarget,
    ) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "Inbound hook".to_string(),
            instruction: "Static fallback instruction.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Webhook {
                token: None,
                route_name: Some(route_name.to_string()),
                secret_ref: Some(format!("vault:webhook/{route_name}")),
                events,
                filters,
                prompt_template,
                deliver,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn pr_payload() -> Value {
        json!({
            "action": "opened",
            "pull_request": { "title": "Fix the flaky retry loop", "number": 42 },
        })
    }

    #[tokio::test]
    async fn event_not_in_allowlist_is_filtered_and_spawns_no_agent() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-filtered-event");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-filtered-event",
            "agent-filtered-event",
            "route-a",
            vec!["pull_request".to_string()],
            None,
            None,
            WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment],
            "route-a",
            Some("push"),
            &pr_payload(),
            "{}",
            None,
            None,
        )
        .await;

        assert_eq!(tally, WebhookDispatchTally { matched: 1, fired: 0, delivered: 0, filtered: 1, deduped: 0 });
        assert!(rx.try_recv().is_err(), "no agent run must be spawned for a filtered-out event");
    }

    #[tokio::test]
    async fn declarative_filter_mismatch_is_filtered_and_spawns_no_agent() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-filtered-field");
        persistence.agents.create(&agent).await.unwrap();
        let filter = WebhookFilter::Field(WebhookFieldFilter {
            field: "action".to_string(),
            op: WebhookFilterOp::Equals { value: json!("closed") },
        });
        let assignment = webhook_assignment(
            "assign-filtered-field",
            "agent-filtered-field",
            "route-b",
            vec![],
            Some(filter),
            None,
            WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment],
            "route-b",
            None,
            &pr_payload(),
            "{}",
            None,
            None,
        )
        .await;

        assert_eq!(tally.filtered, 1);
        assert_eq!(tally.fired, 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn matching_event_renders_template_and_carries_payload_into_agent_message() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-render");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-render",
            "agent-render",
            "route-c",
            vec!["pull_request".to_string()],
            None,
            Some("Review PR #{pull_request.number}: {pull_request.title}".to_string()),
            WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment],
            "route-c",
            Some("pull_request"),
            &pr_payload(),
            "{}",
            Some("delivery-1"),
            None,
        )
        .await;

        assert_eq!(tally.fired, 1);
        assert_eq!(tally.filtered, 0);
        assert_eq!(tally.deduped, 0);

        let (_, dispatched) = rx.try_recv().expect("agent message must be enqueued");
        assert!(
            dispatched.content.contains("Review PR #42: Fix the flaky retry loop"),
            "rendered prompt template must become the agent instruction, got: {}",
            dispatched.content
        );
        assert!(
            !dispatched.content.contains("Static fallback instruction."),
            "the static instruction must be replaced by the rendered template when one is set"
        );
        assert!(
            dispatched.content.contains("\"title\": \"Fix the flaky retry loop\""),
            "the raw event payload must reach the agent message via TriggerEventContext, got: {}",
            dispatched.content
        );
    }

    #[tokio::test]
    async fn no_template_falls_back_to_static_instruction() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-no-template");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-no-template",
            "agent-no-template",
            "route-d",
            vec![],
            None,
            None,
            WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-d", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally.fired, 1);
        let (_, dispatched) = rx.try_recv().expect("agent message must be enqueued");
        assert!(dispatched.content.contains("Static fallback instruction."));
    }

    #[tokio::test]
    async fn deliver_only_never_starts_an_agent_run() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-deliver-only");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-deliver-only",
            "agent-deliver-only",
            "route-e",
            vec![],
            None,
            Some("New PR: {pull_request.title}".to_string()),
            WebhookDeliverTarget::DeliverOnly,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-e", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally, WebhookDispatchTally { matched: 1, fired: 0, delivered: 1, filtered: 0, deduped: 0 });
        assert!(rx.try_recv().is_err(), "deliver_only must never start an agent run");

        // No AssignmentRun row is created either — deliver_only truly skips
        // the run pipeline, not just the process spawn.
        let runs = persistence.assignment_runs.list_for_assignment("assign-deliver-only").await.unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn github_comment_target_does_not_start_an_agent_run() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-gh-comment");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-gh-comment",
            "agent-gh-comment",
            "route-f",
            vec![],
            None,
            None,
            WebhookDeliverTarget::GithubComment,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        // `pr_payload()` carries no `repository` field, so repo/PR resolution
        // fails closed inside `deliver_github_comment` (no subprocess is ever
        // spawned) — but the branch still counts as `delivered`, same as
        // `DeliverOnly`: "no agent run" is the tally's job, not "the GitHub
        // POST succeeded."
        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-f", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally.fired, 0);
        assert_eq!(tally.delivered, 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn github_comment_target_resolves_repo_and_pr_from_realistic_payload() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-gh-comment-realistic");
        persistence.agents.create(&agent).await.unwrap();
        let realistic_payload = json!({
            "action": "opened",
            "number": 42,
            "pull_request": { "number": 42, "title": "Fix the flaky retry loop" },
            "repository": { "full_name": "acme/widgets" },
            "sender": { "login": "octocat" },
        });
        let assignment = webhook_assignment(
            "assign-gh-comment-realistic",
            "agent-gh-comment-realistic",
            "route-gh-realistic",
            vec!["pull_request".to_string()],
            None,
            Some("New PR: {pull_request.title}".to_string()),
            WebhookDeliverTarget::GithubComment,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        // A well-formed `repository.full_name` + `pull_request.number` clears
        // resolution and validation before any subprocess is attempted — this
        // exercises `github_comment::deliver_github_comment`'s happy path
        // through the dispatch loop, independent of whether `gh` itself is
        // installed/authenticated in the environment running the test.
        let tally = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment],
            "route-gh-realistic",
            Some("pull_request"),
            &realistic_payload,
            "{}",
            None,
            None,
        )
        .await;

        assert_eq!(tally.fired, 0, "github_comment must never start an agent run");
        assert_eq!(tally.delivered, 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicate_delivery_id_is_deduped_after_first_fire() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-dedup");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-dedup", "agent-dedup", "route-g", vec![], None, None, WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let first = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment.clone()],
            "route-g",
            None,
            &pr_payload(),
            "{}",
            Some("dup-1"),
            None,
        )
        .await;
        assert_eq!(first.fired, 1);
        rx.try_recv().expect("first fire enqueues a message");

        let second = dispatch_webhook_route(
            &persistence,
            &dispatcher,
            &event_bus,
            &[assignment],
            "route-g",
            None,
            &pr_payload(),
            "{}",
            Some("dup-1"),
            None,
        )
        .await;
        assert_eq!(second, WebhookDispatchTally { matched: 1, fired: 0, delivered: 0, filtered: 0, deduped: 1 });
        assert!(rx.try_recv().is_err(), "a duplicate delivery id must not fire a second time");
    }

    #[tokio::test]
    async fn disabled_assignment_is_skipped_entirely() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-disabled");
        persistence.agents.create(&agent).await.unwrap();
        let mut assignment = webhook_assignment(
            "assign-disabled", "agent-disabled", "route-h", vec![], None, None, WebhookDeliverTarget::Agent,
        );
        assignment.enabled = false;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-h", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally, WebhookDispatchTally { matched: 1, fired: 0, delivered: 0, filtered: 0, deduped: 0 });
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn expired_assignment_does_not_fire_and_is_disabled() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-expired");
        persistence.agents.create(&agent).await.unwrap();
        let mut assignment = webhook_assignment(
            "assign-expired", "agent-expired", "route-j", vec![], None, None, WebhookDeliverTarget::Agent,
        );
        assignment.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-j", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally, WebhookDispatchTally { matched: 1, fired: 0, delivered: 0, filtered: 1, deduped: 0 });
        assert!(rx.try_recv().is_err(), "an expired webhook trigger must not start an agent run");

        let after = persistence.assignments.get("assign-expired").await.unwrap();
        assert!(!after.enabled, "an expired webhook assignment must be disabled on the fire attempt that finds it past expiry");
    }

    #[tokio::test]
    async fn future_expires_at_still_fires() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-not-expired");
        persistence.agents.create(&agent).await.unwrap();
        let mut assignment = webhook_assignment(
            "assign-not-expired", "agent-not-expired", "route-k", vec![], None, None, WebhookDeliverTarget::Agent,
        );
        assignment.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let tally = dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-k", None, &pr_payload(), "{}", None, None,
        )
        .await;

        assert_eq!(tally.fired, 1);
        rx.try_recv().expect("a webhook trigger with a future expires_at must still fire");

        let after = persistence.assignments.get("assign-not-expired").await.unwrap();
        assert!(after.enabled, "must remain enabled while expires_at is still in the future");
    }

    #[tokio::test]
    async fn fired_run_status_is_queued() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-status");
        persistence.agents.create(&agent).await.unwrap();
        let assignment = webhook_assignment(
            "assign-status", "agent-status", "route-i", vec![], None, None, WebhookDeliverTarget::Agent,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        dispatch_webhook_route(
            &persistence, &dispatcher, &event_bus, &[assignment], "route-i", None, &pr_payload(), "{}", None, None,
        )
        .await;
        rx.try_recv().expect("message enqueued");

        let runs = persistence.assignment_runs.list_for_assignment("assign-status").await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, AssignmentRunStatus::Queued);
    }
}
