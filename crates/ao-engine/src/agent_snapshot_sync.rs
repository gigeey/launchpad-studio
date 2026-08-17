use std::sync::Arc;

use ao_persistence::PersistenceLayer;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::TasklistOwner;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::event_bus::EventBus;

const SNAPSHOT_SYNC_RUN_ID: &str = "agent-snapshot-sync";

/// Subscribe to tasklist lifecycle events for agent-owned tasklists and keep
/// `AgentSnapshot.active_tasklist_title` in sync.
///
/// Sets the title when an agent-owned tasklist enters the `Active` state and
/// clears it when the tasklist reaches a terminal state (`completed`, `failed`,
/// `cancelled`). Drives the sidebar ping without needing per-agent SSE for
/// non-selected agents.
///
/// Returns a `watch::Sender<()>`; drop or `send(())` to stop the subscriber.
pub fn spawn_agent_snapshot_tasklist_sync(
    persistence: Arc<PersistenceLayer>,
    event_bus: Arc<EventBus>,
) -> watch::Sender<()> {
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let mut rx = event_bus.subscribe();
    info!("AgentSnapshotTasklistSync starting");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    info!("AgentSnapshotTasklistSync shutting down");
                    break;
                }
                evt = rx.recv() => {
                    let evt = match evt {
                        Ok(e) => e,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(skipped, "AgentSnapshotTasklistSync lagged on broadcast bus");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };

                    let agent_id = match &evt.payload {
                        AgentEventPayload::TasklistCreated { owner, .. }
                        | AgentEventPayload::TasklistCompleted { owner, .. }
                        | AgentEventPayload::TasklistFailed { owner, .. }
                        | AgentEventPayload::TasklistStatusChanged { owner, .. } => {
                            match owner {
                                Some(TasklistOwner::Agent { agent_id }) => agent_id.clone(),
                                _ => continue,
                            }
                        }
                        _ => continue,
                    };

                    match refresh_agent_active_tasklist_title(&persistence, &agent_id).await {
                        Ok(title) => {
                            event_bus
                                .emit(
                                    SNAPSHOT_SYNC_RUN_ID,
                                    &agent_id,
                                    None,
                                    AgentEventPayload::AgentSnapshotUpdated {
                                        agent_id: agent_id.clone(),
                                        active_tasklist_title: title,
                                    },
                                )
                                .await;
                        }
                        Err(e) => {
                            warn!(agent_id = %agent_id, "active_tasklist_title refresh failed: {}", e);
                        }
                    }
                }
            }
        }
    });

    shutdown_tx
}

/// Recompute `AgentSnapshot.active_tasklist_title` for a single agent.
/// Sets the title to the first active tasklist found, or clears it when no
/// agent-owned tasklist is in the `Active` state. Returns the new title so
/// callers can include it in a follow-up event without a second snapshot read.
pub async fn refresh_agent_active_tasklist_title(
    persistence: &PersistenceLayer,
    agent_id: &str,
) -> Result<Option<String>, AoError> {
    let tasklists = persistence.tasklists.list_for_agent(agent_id).await?;
    let active_title = tasklists
        .into_iter()
        .find(|tl| tl.status == ao_protocol::tasklist::TasklistStatus::Active)
        .map(|tl| tl.title);

    let returned_title = active_title.clone();
    persistence
        .snapshots
        .update_agent_entry(agent_id, |entry| {
            entry.active_tasklist_title = active_title;
        })
        .await?;

    Ok(returned_title)
}

/// One-shot startup sweep: recompute `active_tasklist_title` for every agent
/// that has at least one tasklist on disk. Best-effort — per-agent failures
/// are logged and don't abort the sweep.
pub async fn hydrate_agent_snapshot_fields(persistence: Arc<PersistenceLayer>) {
    let snapshot = persistence.snapshots.get().await;
    let agent_ids: Vec<String> = snapshot.agents.keys().cloned().collect();
    for agent_id in agent_ids {
        if let Err(e) = refresh_agent_active_tasklist_title(&persistence, &agent_id).await {
            warn!(agent_id = %agent_id, "startup active_tasklist_title hydration failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ao_persistence::{paths::DataRoot, PersistenceLayer};
    use ao_protocol::{
        event::AgentEventPayload,
        tasklist::{Tasklist, TasklistOwner, TasklistStatus},
    };
    use chrono::Utc;
    use tokio::time::{timeout, Duration};

    use crate::event_bus::EventBus;

    use super::{refresh_agent_active_tasklist_title, spawn_agent_snapshot_tasklist_sync};

    async fn make_test_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.expect("ensure_directories");
        let p = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
        (Arc::new(p), tmp)
    }

    #[tokio::test]
    async fn snapshot_sync_sets_title_and_emits_agent_snapshot_updated() {
        let (persistence, _tmp) = make_test_persistence().await;
        let agent_id = "test-agent-1".to_string();
        let tasklist_id = "tl-test-1".to_string();
        let tasklist_title = "My Test Tasklist".to_string();

        // Ensure the agent exists in the snapshot store.
        persistence
            .snapshots
            .update_agent_entry(&agent_id, |e| {
                e.name = agent_id.clone();
            })
            .await
            .expect("create agent snapshot");

        // Build path strings before creating the tasklist.
        let workspace_dir = persistence
            .tasklists
            .data_root()
            .agent_tasklist_workspace_dir(&agent_id, &tasklist_id)
            .to_string_lossy()
            .into_owned();
        let transcripts_dir = persistence
            .tasklists
            .data_root()
            .agent_tasklist_transcripts_dir(&agent_id, &tasklist_id)
            .to_string_lossy()
            .into_owned();

        let tasklist = Tasklist {
            id: tasklist_id.clone(),
            owner: TasklistOwner::Agent { agent_id: agent_id.clone() },
            team_id: None,
            title: tasklist_title.clone(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir,
            transcripts_dir,
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create_for_agent(&tasklist)
            .await
            .expect("create tasklist");

        let event_bus = Arc::new(EventBus::new(256));
        // Subscribe before spawning so we don't miss the AgentSnapshotUpdated event.
        let mut rx = event_bus.subscribe();
        let _shutdown =
            spawn_agent_snapshot_tasklist_sync(Arc::clone(&persistence), Arc::clone(&event_bus));

        // Simulate the tasklist becoming active by publishing a TasklistStatusChanged event.
        event_bus
            .emit(
                "test-run",
                &agent_id,
                None,
                AgentEventPayload::TasklistStatusChanged {
                    team_id: String::new(),
                    tasklist_id: tasklist_id.clone(),
                    status: "active".into(),
                    owner: Some(TasklistOwner::Agent { agent_id: agent_id.clone() }),
                    project_id: None,
                },
            )
            .await;

        // Collect bus events until AgentSnapshotUpdated arrives for our agent.
        let (found_agent_id, found_title) = timeout(Duration::from_secs(3), async {
            loop {
                let evt = rx.recv().await.expect("bus closed unexpectedly");
                if let AgentEventPayload::AgentSnapshotUpdated {
                    agent_id: eid,
                    active_tasklist_title: title,
                } = evt.payload
                {
                    if eid == agent_id {
                        return (eid, title);
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for AgentSnapshotUpdated");

        assert_eq!(found_agent_id, agent_id);
        assert_eq!(found_title, Some(tasklist_title.clone()));

        // Verify the snapshot was actually persisted with the new title.
        let snap = persistence.snapshots.get().await;
        let agent_snap = snap.agents.get(&agent_id).expect("agent snapshot missing");
        assert_eq!(agent_snap.active_tasklist_title, Some(tasklist_title));
    }

    #[tokio::test]
    async fn refresh_returns_none_when_no_active_tasklist() {
        let (persistence, _tmp) = make_test_persistence().await;
        let agent_id = "test-agent-2".to_string();

        persistence
            .snapshots
            .update_agent_entry(&agent_id, |e| {
                e.name = agent_id.clone();
                e.active_tasklist_title = Some("stale-title".into());
            })
            .await
            .expect("create agent snapshot");

        let result = refresh_agent_active_tasklist_title(&persistence, &agent_id)
            .await
            .expect("refresh should not fail");

        assert_eq!(result, None);

        let snap = persistence.snapshots.get().await;
        let agent_snap = snap.agents.get(&agent_id).expect("agent snapshot missing");
        assert_eq!(agent_snap.active_tasklist_title, None, "stale title should be cleared");
    }
}
