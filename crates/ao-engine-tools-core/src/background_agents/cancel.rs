use super::handle::BackgroundAgentId;
use super::registry::BackgroundAgentRegistry;

/// Outcome of attempting to cancel a single live delegation.
///
/// Distinguishes "there was nothing to cancel" (`NotFound`), "cancellation
/// was already in flight" (`AlreadyCancelled`), and "this call is the one
/// that fired the token" (`Cancelled`) so callers — the `DelegateStop` tool
/// and the HTTP cancel route alike — can report the right outcome instead of
/// treating a second cancel as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// No live handle exists for this id in the registry.
    NotFound,
    /// The delegation's cancellation token was already fired by an earlier
    /// call before this one ran.
    AlreadyCancelled,
    /// This call fired the delegation's cancellation token.
    Cancelled,
}

/// Cancel one delegation by id in `registry`.
///
/// Fires the delegation's [`CancellationToken`](tokio_util::sync::CancellationToken)
/// and returns immediately — it does not wait for the delegation's task to
/// actually stop. The handle is left in the registry either way; reaping it
/// stays the job of whatever already polls for completion (the `DelegateOutput`
/// tool on the model-facing path). Idempotent: a second call on an
/// already-cancelled id returns `AlreadyCancelled` rather than erroring.
///
/// Shared by the `DelegateStop` engine tool and the `POST
/// /delegates/{delegation_id}/cancel` HTTP route so both surfaces cancel
/// exactly one delegation, the same way, with no separate re-implementation
/// to drift out of sync.
pub async fn cancel_delegation(
    registry: &BackgroundAgentRegistry,
    id: &BackgroundAgentId,
) -> CancelOutcome {
    let snapshot = match registry.get(id).await {
        Some(s) => s,
        None => return CancelOutcome::NotFound,
    };

    if snapshot.cancel.is_cancelled() {
        return CancelOutcome::AlreadyCancelled;
    }

    snapshot.cancel.cancel();
    CancelOutcome::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_agents::handle::{BackgroundAgentHandle, TaskFinalReport};
    use tokio::sync::broadcast;

    fn make_handle(name: &str) -> BackgroundAgentHandle {
        let (_tx, rx) = broadcast::channel(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Ok::<TaskFinalReport, ao_protocol::error::AoError>(TaskFinalReport::cancelled())
        });
        BackgroundAgentHandle {
            id: BackgroundAgentId::new(),
            subagent_name: name.to_string(),
            spawned_at: chrono::Utc::now(),
            cancel,
            events: rx,
            join,
        }
    }

    #[tokio::test]
    async fn unknown_id_is_not_found() {
        let registry = BackgroundAgentRegistry::new(2);
        let outcome = cancel_delegation(&registry, &BackgroundAgentId::new()).await;
        assert_eq!(outcome, CancelOutcome::NotFound);
    }

    #[tokio::test]
    async fn first_cancel_fires_token_and_reports_cancelled() {
        let registry = BackgroundAgentRegistry::new(2);
        let h = make_handle("alpha");
        let id = h.id.clone();
        registry.insert(h).await.unwrap();

        let outcome = cancel_delegation(&registry, &id).await;
        assert_eq!(outcome, CancelOutcome::Cancelled);

        let snapshot = registry.get(&id).await.expect("handle stays in registry");
        assert!(snapshot.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn second_cancel_is_idempotent() {
        let registry = BackgroundAgentRegistry::new(2);
        let h = make_handle("alpha");
        let id = h.id.clone();
        registry.insert(h).await.unwrap();

        assert_eq!(cancel_delegation(&registry, &id).await, CancelOutcome::Cancelled);
        assert_eq!(
            cancel_delegation(&registry, &id).await,
            CancelOutcome::AlreadyCancelled
        );

        // Still exactly one handle — cancel never reaps.
        assert_eq!(registry.live_count().await, 1);
    }
}
