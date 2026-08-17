use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::background_agents::handle::TaskFinalReport;

/// Character cap on the final-text excerpt embedded in a delegate completion
/// notification. Keeps the message readable without embedding the full
/// transcript inline.
pub const DELEGATE_EXCERPT_CAP: usize = 2000;

/// Delivers a single notification to the parent agent's durable queue when a
/// background delegate finishes.
///
/// Defined in this crate so [`crate::context::RunnerContext`] can hold an
/// optional sink without creating a circular crate dependency. The production
/// implementation lives in `ao-engine` (backed by `QueueManagerRegistry`);
/// tests supply a recording stub.
///
/// Implementors must be `Send + Sync` so the sink can be moved into a
/// `tokio::spawn` that outlives the HTTP request.
#[async_trait]
pub trait DelegateCompletionSink: Send + Sync {
    /// Queue a completion notification for the parent agent.
    ///
    /// Called once by `SubagentSpawner::spawn_named_async` after the background
    /// delegate reaches a terminal state. Implementations build a model-facing
    /// message that matches the tone of the tasklist-completion notification:
    /// delegate name, delegation_id, status (completed/failed/cancelled),
    /// duration, a final-text excerpt, the transcript path, and a note that
    /// `DelegateOutput` retrieves the full result.
    ///
    /// Failures are logged by the implementation and do not propagate to the
    /// caller. No retry or dedup is needed — the notification fires before any
    /// poll could consume the result, and `DelegateOutput` returns the same
    /// stored result regardless.
    async fn notify(
        &self,
        delegate_name: &str,
        delegation_id: &str,
        report: &TaskFinalReport,
        transcript_path: &str,
    );

    /// Fire the bracketing "started" notification, called once by
    /// `SubagentSpawner::spawn_named_async_core` right after the background
    /// delegate's handle is registered (before the child begins producing
    /// output). Async-mode delegates only — sync delegates never call this.
    ///
    /// `spawned_at` is the same timestamp the caller stamped on the
    /// background handle at registration — passing it through here (rather
    /// than letting the sink call `Utc::now()` itself) keeps the live
    /// `DelegateStarted` event and any later reconnect-replay of it in
    /// agreement about when the run actually began.
    ///
    /// Defaults to a no-op so existing implementations (test stubs, sinks that
    /// only care about completion) don't need to change. The production
    /// implementation emits `AgentEventPayload::DelegateStarted` on the same
    /// thread-tagged event bus channel [`notify`](Self::notify) uses for
    /// `DelegateComplete`.
    async fn notify_started(
        &self,
        _delegate_name: &str,
        _delegation_id: &str,
        _spawned_at: DateTime<Utc>,
    ) {
    }
}
