//! Unit tests for the task feeder.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use std::sync::Mutex;

use ao_persistence::paths::DataRoot;
use ao_protocol::tasklist::{
    AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, TasklistStatus,
};

/// Test dispatcher that records every call and never errors.
struct RecordingDispatcher {
    calls: Mutex<Vec<(AgentId, TaskId, String)>>,
}

impl RecordingDispatcher {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(AgentId, TaskId, String)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl TaskDispatcher for RecordingDispatcher {
    async fn dispatch_task(
        &self,
        owner_agent_id: &AgentId,
        prompt: String,
        _owner: &TasklistOwner,
        _tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError> {
        self.calls
            .lock()
            .unwrap()
            .push((owner_agent_id.clone(), task_id.clone(), prompt));
        Ok(())
    }
}

fn task(id: &str, owner: &str, group_id: &str) -> Task {
    Task {
        id: id.to_string(),
        owner_agent_id: owner.to_string(),
        prompt: format!("prompt for {id}"),
        expected_outputs: vec![format!("{id}.md")],
        status: TaskStatus::Pending,
        group_id: group_id.to_string(),
        attempt_count: 0,
        error_log: vec![],
        comments: vec![],
        attachments: vec![],
        remind_me: None,
        parse_failed: false,
        notification_parse_retry_count: 0,
        assignment: None,
        classifier_token: 0,
        dispatch_token: 0,
    }
}

/// Create a task with a Pinned assignment set — required for agent-owned
/// tasklist dispatch (agent-owned tasks route via assignment, not
/// owner_agent_id alone).
fn agent_task(id: &str, owner: &str, group_id: &str) -> Task {
    Task {
        assignment: Some(TaskAssignment {
            owner_agent_id: owner.to_string(),
            mode: AssignmentMode::Pinned,
        }),
        ..task(id, owner, group_id)
    }
}

/// Create a task shaped like a production classifier-assigned one: the
/// top-level `owner_agent_id` is **empty** and the chosen executor lives
/// only in `assignment.owner_agent_id` with `AssignmentMode::Classified`.
/// This is the shape that exposed the watchdog false-failure — a liveness
/// probe keyed off the empty `owner_agent_id` builds the wrong registry key
/// and never sees the live run. `agent_task` (Pinned, owner == executor)
/// cannot reproduce it because both fields agree.
fn classified_task(id: &str, executor: &str, group_id: &str) -> Task {
    Task {
        owner_agent_id: String::new(),
        assignment: Some(TaskAssignment {
            owner_agent_id: executor.to_string(),
            mode: AssignmentMode::Classified,
        }),
        ..task(id, "", group_id)
    }
}

fn agent_task_with_outputs(id: &str, owner: &str, group_id: &str, outputs: Vec<&str>) -> Task {
    Task {
        assignment: Some(TaskAssignment {
            owner_agent_id: owner.to_string(),
            mode: AssignmentMode::Pinned,
        }),
        ..task_with_outputs(id, owner, group_id, outputs)
    }
}

fn group(id: &str, mode: TaskGroupMode, tasks: Vec<Task>) -> TaskGroup {
    TaskGroup {
        id: id.to_string(),
        mode,
        tasks,
    }
}

fn tasklist(team_id: &str, id: &str, groups: Vec<TaskGroup>) -> Tasklist {
    use ao_protocol::tasklist::TasklistOwner;
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Team {
            team_id: team_id.to_string(),
        },
        team_id: Some(team_id.to_string()),
        title: format!("Tasklist {id}"),
        description: String::new(),
        status: TasklistStatus::Active,
        groups,
        workspace_dir: format!("/tmp/teams/{team_id}/tasklists/{id}/workspace"),
        transcripts_dir: format!("/tmp/teams/{team_id}/tasklists/{id}/transcripts"),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        }
}

async fn setup() -> (
    tempfile::TempDir,
    Arc<TasklistStore>,
    Arc<RecordingDispatcher>,
    TaskFeeder,
) {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());
    (tmp, store, dispatcher, feeder)
}

async fn task_status(store: &TasklistStore, tl_id: &str, task_id: &str) -> TaskStatus {
    let tl = store.get("team-a", tl_id).await.unwrap().unwrap();
    tl.groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .map(|t| t.status)
        .unwrap()
}

#[tokio::test]
async fn par_group_dispatches_all_tasks_at_once() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-par",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![
                task("t1", "researcher-a", "g1"),
                task("t2", "researcher-b", "g1"),
            ],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-par".to_string())
        .await
        .unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
    let mut ids: Vec<_> = calls.iter().map(|c| c.1.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["t1".to_string(), "t2".to_string()]);

    assert_eq!(
        task_status(&store, "tl-par", "t1").await,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_status(&store, "tl-par", "t2").await,
        TaskStatus::InProgress
    );

    // Registry maps each agent to its assigned task.
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-par".to_string(), &"researcher-a".to_string())
            .await,
        Some("t1".to_string())
    );
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-par".to_string(), &"researcher-b".to_string())
            .await,
        Some("t2".to_string())
    );
}

#[tokio::test]
async fn seq_group_dispatches_one_then_advances_on_terminal() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-seq",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                task("t1", "worker", "g1"),
                task("t2", "worker", "g1"),
                task("t3", "worker", "g1"),
            ],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-seq".to_string())
        .await
        .unwrap();

    // Only the first SEQ task is dispatched.
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
    assert_eq!(
        task_status(&store, "tl-seq", "t1").await,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_status(&store, "tl-seq", "t2").await,
        TaskStatus::Pending
    );

    // Mark t1 completed and notify the feeder.
    store
        .set_task_status("team-a", "tl-seq", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-seq".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].1, "t2");
    assert_eq!(
        task_status(&store, "tl-seq", "t2").await,
        TaskStatus::InProgress
    );

    // Registry rotated: agent now owns t2, not t1.
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-seq".to_string(), &"worker".to_string())
            .await,
        Some("t2".to_string())
    );

    // Complete t2, then t3 dispatches.
    store
        .set_task_status("team-a", "tl-seq", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-seq".to_string(),
            &"t2".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(dispatcher.calls().len(), 3);
    assert_eq!(dispatcher.calls()[2].1, "t3");
}

#[tokio::test]
async fn group_advancement_waits_for_all_par_tasks_terminal() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-mixed",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![
                    task("t1", "researcher-a", "g1"),
                    task("t2", "researcher-b", "g1"),
                ],
            ),
            group("g2", TaskGroupMode::Seq, vec![task("t3", "analyst", "g2")]),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-mixed".to_string())
        .await
        .unwrap();

    // Both PAR tasks dispatched, but g2 has not.
    assert_eq!(dispatcher.calls().len(), 2);

    // First PAR task completes — g2 must NOT dispatch yet.
    store
        .set_task_status("team-a", "tl-mixed", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-mixed".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        dispatcher.calls().len(),
        2,
        "g2 must not start until ALL g1 tasks terminate"
    );
    assert_eq!(
        task_status(&store, "tl-mixed", "t3").await,
        TaskStatus::Pending
    );

    // Second PAR task completes — now g2 dispatches.
    store
        .set_task_status("team-a", "tl-mixed", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-mixed".to_string(),
            &"t2".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(dispatcher.calls().len(), 3);
    assert_eq!(dispatcher.calls()[2].1, "t3");
    assert_eq!(
        task_status(&store, "tl-mixed", "t3").await,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn failed_terminal_task_halts_tasklist_and_stops_dispatch() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-fail",
        vec![
            group("g1", TaskGroupMode::Par, vec![task("t1", "a", "g1")]),
            group("g2", TaskGroupMode::Seq, vec![task("t2", "b", "g2")]),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-fail".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // t1 fails — the tasklist transitions to Failed and g2 must NOT dispatch.
    store
        .set_task_status("team-a", "tl-fail", "t1", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-fail".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // g2 must NOT have been dispatched.
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(
        task_status(&store, "tl-fail", "t2").await,
        TaskStatus::Pending
    );

    // Tasklist itself transitioned to Failed.
    let updated = store.get("team-a", "tl-fail").await.unwrap().unwrap();
    assert_eq!(updated.status, TasklistStatus::Failed);
}

#[tokio::test]
async fn coordinator_self_assigned_task_dispatches_identically() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    // The coordinator owns g1's task, then the coordinator-self-assigned
    // summary task in g2. Both go through the same dispatcher path.
    let tl = tasklist(
        "team-a",
        "tl-coord",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![task("t1", "researcher", "g1")],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task("t2", "coordinator", "g2")],
            ),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-coord".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].0, "researcher");

    store
        .set_task_status("team-a", "tl-coord", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-coord".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Coordinator-owned task is dispatched the same way as a member task.
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].0, "coordinator");
    assert_eq!(dispatcher.calls()[1].1, "t2");
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-coord".to_string(), &"coordinator".to_string())
            .await,
        Some("t2".to_string())
    );
}

#[tokio::test]
async fn start_is_idempotent_when_group_already_in_flight() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-idem",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![task("t1", "a", "g1"), task("t2", "b", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-idem".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);

    // Second start: tasks are InProgress, neither matches Pending → no
    // re-dispatch.
    feeder
        .start(&"team-a".to_string(), &"tl-idem".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);
}

#[tokio::test]
async fn empty_group_is_skipped_and_next_group_runs() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-empty",
        vec![
            group("g1", TaskGroupMode::Par, vec![]),
            group("g2", TaskGroupMode::Seq, vec![task("t1", "worker", "g2")]),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-empty".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
}

#[tokio::test]
async fn start_fails_when_tasklist_not_active() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-done",
        vec![group("g1", TaskGroupMode::Par, vec![task("t1", "a", "g1")])],
    );
    tl.status = TasklistStatus::Completed;
    store.create(&tl).await.unwrap();

    let err = feeder
        .start(&"team-a".to_string(), &"tl-done".to_string())
        .await;
    assert!(matches!(err, Err(AoError::InvalidTasklistTransition(_))));
}

#[tokio::test]
async fn start_unknown_tasklist_errors() {
    let (_tmp, _store, _dispatcher, feeder) = setup().await;
    let err = feeder
        .start(&"team-a".to_string(), &"ghost".to_string())
        .await;
    assert!(matches!(err, Err(AoError::TasklistNotFound(_))));
}

#[tokio::test]
async fn on_task_terminal_clears_registry_entry() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-reg",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![task("t1", "a", "g1"), task("t2", "b", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-reg".to_string())
        .await
        .unwrap();

    store
        .set_task_status("team-a", "tl-reg", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-reg".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Agent 'a' is no longer assigned anything; agent 'b' still owns t2.
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-reg".to_string(), &"a".to_string())
            .await,
        None
    );
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-reg".to_string(), &"b".to_string())
            .await,
        Some("t2".to_string())
    );
}

// ---- output validation + reprompt ---------------------------------------

/// Build a tasklist whose `workspace_dir` is the real on-disk path that
/// `store.create` will populate, so the validation tests can write files
/// into the workspace and have `tokio::fs::try_exists` see them.
fn tasklist_with_real_workspace(
    data_root: &DataRoot,
    team_id: &str,
    id: &str,
    groups: Vec<TaskGroup>,
) -> Tasklist {
    let workspace = data_root.tasklist_workspace_dir(team_id, id);
    let transcripts = data_root.tasklist_transcripts_dir(team_id, id);
    use ao_protocol::tasklist::TasklistOwner;
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Team {
            team_id: team_id.to_string(),
        },
        team_id: Some(team_id.to_string()),
        title: format!("Tasklist {id}"),
        description: String::new(),
        status: TasklistStatus::Active,
        groups,
        workspace_dir: workspace.to_string_lossy().to_string(),
        transcripts_dir: transcripts.to_string_lossy().to_string(),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        }
}

fn task_with_outputs(id: &str, owner: &str, group_id: &str, outputs: Vec<&str>) -> Task {
    Task {
        id: id.to_string(),
        owner_agent_id: owner.to_string(),
        prompt: format!("prompt for {id}"),
        expected_outputs: outputs.into_iter().map(String::from).collect(),
        status: TaskStatus::Pending,
        group_id: group_id.to_string(),
        attempt_count: 0,
        error_log: vec![],
        comments: vec![],
        attachments: vec![],
        remind_me: None,
        parse_failed: false,
        notification_parse_retry_count: 0,
        assignment: None,
        classifier_token: 0,
        dispatch_token: 0,
    }
}

async fn task_snapshot(store: &TasklistStore, tl_id: &str, task_id: &str) -> Task {
    let tl = store.get("team-a", tl_id).await.unwrap().unwrap();
    tl.groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn validate_and_complete_passes_when_all_outputs_present() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-ok",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![task_with_outputs(
                    "t1",
                    "researcher",
                    "g1",
                    vec!["report.md"],
                )],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task_with_outputs(
                    "t2",
                    "summarizer",
                    "g2",
                    vec!["summary.md"],
                )],
            ),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-ok".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");

    // Agent produced the expected output before emitting <task complete>.
    let workspace = data_root.tasklist_workspace_dir("team-a", "tl-ok");
    tokio::fs::write(workspace.join("report.md"), b"findings")
        .await
        .unwrap();

    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-ok".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Task transitioned to Completed; group 2 dispatched.
    let t1 = task_snapshot(&store, "tl-ok", "t1").await;
    assert_eq!(t1.status, TaskStatus::Completed);
    assert_eq!(t1.attempt_count, 0);
    assert!(t1.error_log.is_empty());

    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].1, "t2");
}

#[tokio::test]
async fn validate_and_complete_reprompts_when_outputs_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone()).with_max_attempts(3);

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-miss",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task_with_outputs(
                "t1",
                "researcher",
                "g1",
                vec!["a.md", "b.md"],
            )],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-miss".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // Agent emits <task complete> but neither file exists.
    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-miss".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Reprompted (second dispatch on the same task), status still InProgress,
    // attempt_count bumped, error_log populated naming missing files.
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].0, "researcher");
    assert_eq!(dispatcher.calls()[1].1, "t1");
    assert!(
        dispatcher.calls()[1].2.contains("a.md"),
        "reprompt prompt should name missing file a.md, got: {}",
        dispatcher.calls()[1].2
    );
    assert!(dispatcher.calls()[1].2.contains("b.md"));

    let t1 = task_snapshot(&store, "tl-miss", "t1").await;
    assert_eq!(t1.status, TaskStatus::InProgress);
    assert_eq!(t1.attempt_count, 1);
    assert_eq!(t1.error_log.len(), 1);
    assert!(t1.error_log[0].contains("a.md"));
    assert!(t1.error_log[0].contains("b.md"));

    // After the reprompt the agent produces ONE of the files but not both.
    let workspace = data_root.tasklist_workspace_dir("team-a", "tl-miss");
    tokio::fs::write(workspace.join("a.md"), b"partial")
        .await
        .unwrap();

    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-miss".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Still missing b.md → second reprompt, attempt_count = 2.
    assert_eq!(dispatcher.calls().len(), 3);
    assert!(dispatcher.calls()[2].2.contains("b.md"));
    assert!(!dispatcher.calls()[2].2.contains("a.md"));

    let t1 = task_snapshot(&store, "tl-miss", "t1").await;
    assert_eq!(t1.status, TaskStatus::InProgress);
    assert_eq!(t1.attempt_count, 2);
    assert_eq!(t1.error_log.len(), 2);
}

#[tokio::test]
async fn validate_and_complete_fails_after_max_attempts_and_halts_downstream() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    // Lower the cap so the test only needs 2 validation failures.
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone()).with_max_attempts(2);

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-max",
        vec![
            group(
                "g1",
                TaskGroupMode::Seq,
                vec![task_with_outputs(
                    "t1",
                    "researcher",
                    "g1",
                    vec!["report.md"],
                )],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task_with_outputs(
                    "t2",
                    "summarizer",
                    "g2",
                    vec!["summary.md"],
                )],
            ),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-max".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // First validation: outputs missing → reprompt.
    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-max".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        task_snapshot(&store, "tl-max", "t1").await.status,
        TaskStatus::InProgress
    );

    // Second validation: outputs still missing AND attempt_count reached
    // max_attempts → task transitions to Failed and downstream group is
    // NOT dispatched.
    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-max".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // No new dispatch — neither another reprompt nor t2.
    assert_eq!(
        dispatcher.calls().len(),
        2,
        "no further dispatch after max attempts reached"
    );

    let t1 = task_snapshot(&store, "tl-max", "t1").await;
    assert_eq!(t1.status, TaskStatus::Failed);
    assert_eq!(t1.attempt_count, 2);
    assert_eq!(t1.error_log.len(), 2);

    // Tasklist halted; downstream group did not dispatch.
    let updated = store.get("team-a", "tl-max").await.unwrap().unwrap();
    assert_eq!(updated.status, TasklistStatus::Failed);
    assert_eq!(
        task_snapshot(&store, "tl-max", "t2").await.status,
        TaskStatus::Pending
    );
}

#[tokio::test]
async fn validate_and_complete_with_no_expected_outputs_completes_immediately() {
    // Backwards compatibility: tasks declared with empty expected_outputs
    // (e.g. existing tests, request_clarification flows) skip validation
    // and complete on the first call.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-empty-out",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "researcher", "g1")],
        )],
    );
    // Override expected_outputs to empty (the `task()` helper sets a default
    // `{id}.md`).
    let mut tl = tl;
    tl.groups[0].tasks[0].expected_outputs.clear();
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-empty-out".to_string())
        .await
        .unwrap();
    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-empty-out".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        task_snapshot(&store, "tl-empty-out", "t1").await.status,
        TaskStatus::Completed
    );
}

// ---- stale-run reprompt ----------------------------------------------

#[tokio::test]
async fn on_run_ended_no_op_when_agent_has_no_assigned_task() {
    // Clean run case: agent finishes with no task in registry → no-op,
    // no dispatch, no state change.
    let (_tmp, _store, dispatcher, feeder) = setup().await;

    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-none".to_string(),
            &"agent-x".to_string(),
        )
        .await
        .unwrap();

    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn on_run_ended_no_op_when_task_is_no_longer_in_progress() {
    // After <task complete> + on_task_terminal, the registry is cleared.
    // A subsequent on_run_ended must NOT re-fire the stale-run reprompt
    // because the registry lookup returns None.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-clean",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-clean".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // Simulate a clean completion: task → Completed, on_task_terminal
    // clears the registry entry.
    store
        .set_task_status("team-a", "tl-clean", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-clean".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Now RunEnded fires for the just-completed run. Registry has no
    // entry for `worker` → on_run_ended is a no-op.
    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-clean".to_string(),
            &"worker".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        dispatcher.calls().len(),
        1,
        "no extra dispatch on clean completion"
    );
    let t1 = task_snapshot(&store, "tl-clean", "t1").await;
    assert_eq!(t1.status, TaskStatus::Completed);
    assert_eq!(t1.attempt_count, 0);
    assert!(t1.error_log.is_empty());
}

#[tokio::test]
async fn on_run_ended_reprompts_when_task_still_in_progress() {
    // Stale run: agent's run ended but task is still InProgress and
    // registry still maps owner → task. on_run_ended should bump the
    // attempt count, append a stale-run error, and reprompt via the
    // dispatcher (no transition to Completed/Failed yet).
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-stale",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-stale".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-stale".to_string(),
            &"worker".to_string(),
        )
        .await
        .unwrap();

    // Reprompt dispatched on the same task; status still InProgress.
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].0, "worker");
    assert_eq!(dispatcher.calls()[1].1, "t1");
    assert!(
        dispatcher.calls()[1].2.contains("Stale run"),
        "reprompt prompt should explain the stale run, got: {}",
        dispatcher.calls()[1].2,
    );
    assert!(
        dispatcher.calls()[1].2.contains("t1"),
        "reprompt should reference task_id"
    );
    assert!(
        dispatcher.calls()[1].2.contains("t1.md"),
        "reprompt should reference expected_outputs",
    );

    let t1 = task_snapshot(&store, "tl-stale", "t1").await;
    assert_eq!(t1.status, TaskStatus::InProgress);
    assert_eq!(t1.attempt_count, 1);
    assert_eq!(t1.error_log.len(), 1);
    assert!(t1.error_log[0].contains("agent run ended"));

    // Registry entry persists across the reprompt so the next RunEnded
    // also detects the stale state.
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-stale".to_string(), &"worker".to_string())
            .await,
        Some("t1".to_string()),
    );
}

#[tokio::test]
async fn on_run_ended_fails_after_max_attempts_and_halts_tasklist() {
    // Repeated stale runs hit the attempt cap → task transitions to
    // Failed, tasklist halts (no further dispatch), downstream group
    // stays Pending.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    // Lower cap so two stale RunEnded events trip the failure.
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone()).with_max_attempts(2);

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-stale-max",
        vec![
            group(
                "g1",
                TaskGroupMode::Seq,
                vec![task_with_outputs("t1", "worker", "g1", vec!["report.md"])],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task_with_outputs(
                    "t2",
                    "summarizer",
                    "g2",
                    vec!["summary.md"],
                )],
            ),
        ],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-stale-max".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // First stale RunEnded → reprompt (attempt_count → 1).
    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-stale-max".to_string(),
            &"worker".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        task_snapshot(&store, "tl-stale-max", "t1").await.status,
        TaskStatus::InProgress
    );

    // Second stale RunEnded → attempt_count = 2 = max, task → Failed,
    // tasklist halted, no further dispatch.
    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-stale-max".to_string(),
            &"worker".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        dispatcher.calls().len(),
        2,
        "no further dispatch after stale-run failure",
    );

    let t1 = task_snapshot(&store, "tl-stale-max", "t1").await;
    assert_eq!(t1.status, TaskStatus::Failed);
    assert_eq!(t1.attempt_count, 2);
    assert_eq!(t1.error_log.len(), 2);
    assert!(t1.error_log.iter().all(|e| e.contains("agent run ended")));

    // Tasklist halted; downstream group stays Pending; registry cleared.
    let updated = store.get("team-a", "tl-stale-max").await.unwrap().unwrap();
    assert_eq!(updated.status, TasklistStatus::Failed);
    assert_eq!(
        task_snapshot(&store, "tl-stale-max", "t2").await.status,
        TaskStatus::Pending
    );
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-stale-max".to_string(), &"worker".to_string())
            .await,
        None,
    );
}

#[tokio::test]
async fn on_run_ended_no_op_when_tasklist_already_failed() {
    // If the tasklist halted between dispatch and RunEnded (e.g. a sibling
    // PAR task failed), on_run_ended must NOT bump attempt_count or
    // dispatch a reprompt.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-halted",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![task("t1", "a", "g1"), task("t2", "b", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-halted".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);

    // t1 fails → tasklist halts.
    store
        .set_task_status("team-a", "tl-halted", "t1", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-halted".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();
    let halted = store.get("team-a", "tl-halted").await.unwrap().unwrap();
    assert_eq!(halted.status, TasklistStatus::Failed);

    // Now agent 'b' (whose task t2 is still in flight) finishes its run
    // without emitting <task complete|fail>. on_run_ended should no-op
    // because the tasklist is no longer Active.
    feeder
        .on_run_ended(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-halted".to_string(),
            &"b".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        dispatcher.calls().len(),
        2,
        "no reprompt after tasklist halted"
    );
    let t2 = task_snapshot(&store, "tl-halted", "t2").await;
    assert_eq!(t2.status, TaskStatus::InProgress);
    assert_eq!(t2.attempt_count, 0);
    assert!(t2.error_log.is_empty());
}

#[tokio::test]
async fn par_group_partial_failure_lets_inflight_finish_then_halts() {
    // PAR group with t1, t2. t1 fails first → tasklist halted. t2 (still
    // InProgress) eventually completes — but the next group must NOT
    // dispatch, per "already-running PAR tasks allowed to finish, but
    // halt prevents new dispatch".
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let tl = tasklist_with_real_workspace(
        &data_root,
        "team-a",
        "tl-par-fail",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![
                    task_with_outputs("t1", "a", "g1", vec![]),
                    task_with_outputs("t2", "b", "g1", vec![]),
                ],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task_with_outputs("t3", "c", "g2", vec![])],
            ),
        ],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-par-fail".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);

    // t1 fails → tasklist halts.
    store
        .set_task_status("team-a", "tl-par-fail", "t1", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-par-fail".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    let after_t1 = store.get("team-a", "tl-par-fail").await.unwrap().unwrap();
    assert_eq!(after_t1.status, TasklistStatus::Failed);
    assert_eq!(dispatcher.calls().len(), 2, "no new dispatch after halt");

    // t2 (still in-flight) eventually completes via validate_and_complete.
    // Even though g1 is now fully terminal, g2 must NOT dispatch.
    feeder
        .validate_and_complete(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-par-fail".to_string(),
            &"t2".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        task_snapshot(&store, "tl-par-fail", "t2").await.status,
        TaskStatus::Completed
    );
    assert_eq!(
        task_snapshot(&store, "tl-par-fail", "t3").await.status,
        TaskStatus::Pending
    );
    assert_eq!(dispatcher.calls().len(), 2);
}

// ---- Fix: PAR enforces one task per agent at a time -------------------

#[tokio::test]
async fn par_group_with_same_owner_serializes_within_agent() {
    // Three PAR tasks all owned by the same agent. The registry is
    // single-slot per (tasklist, agent), so dispatching all three at once
    // would overwrite the entry and leave the earlier tasks unrecoverable
    // when their runs end. The feeder must dispatch only the first task,
    // then the next on `on_task_terminal`.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-same-owner",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![
                task("t1", "shared-agent", "g1"),
                task("t2", "shared-agent", "g1"),
                task("t3", "shared-agent", "g1"),
            ],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-same-owner".to_string())
        .await
        .unwrap();

    // Only t1 dispatched; t2 and t3 stay Pending; registry holds t1.
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
    assert_eq!(
        task_status(&store, "tl-same-owner", "t1").await,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_status(&store, "tl-same-owner", "t2").await,
        TaskStatus::Pending
    );
    assert_eq!(
        task_status(&store, "tl-same-owner", "t3").await,
        TaskStatus::Pending
    );
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-same-owner".to_string(), &"shared-agent".to_string())
            .await,
        Some("t1".to_string())
    );

    // t1 completes → t2 dispatches next.
    store
        .set_task_status("team-a", "tl-same-owner", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-same-owner".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].1, "t2");
    assert_eq!(
        task_status(&store, "tl-same-owner", "t2").await,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_status(&store, "tl-same-owner", "t3").await,
        TaskStatus::Pending
    );

    // t2 completes → t3 dispatches.
    store
        .set_task_status("team-a", "tl-same-owner", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-same-owner".to_string(),
            &"t2".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(dispatcher.calls().len(), 3);
    assert_eq!(dispatcher.calls()[2].1, "t3");
}

#[tokio::test]
async fn par_group_mixed_owners_dispatches_one_per_distinct_agent() {
    // 4 PAR tasks: agent-a owns 2, agent-b owns 2. First pass dispatches
    // exactly 2 tasks (one per agent); the other two stay Pending.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-mixed-owners",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![
                task("t1a", "agent-a", "g1"),
                task("t1b", "agent-b", "g1"),
                task("t2a", "agent-a", "g1"),
                task("t2b", "agent-b", "g1"),
            ],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-mixed-owners".to_string())
        .await
        .unwrap();

    // Two dispatches: one per agent. Specifically the FIRST task per agent
    // in declared order.
    assert_eq!(dispatcher.calls().len(), 2);
    let mut dispatched_ids: Vec<_> = dispatcher.calls().iter().map(|c| c.1.clone()).collect();
    dispatched_ids.sort();
    assert_eq!(dispatched_ids, vec!["t1a".to_string(), "t1b".to_string()]);
    assert_eq!(
        task_status(&store, "tl-mixed-owners", "t2a").await,
        TaskStatus::Pending
    );
    assert_eq!(
        task_status(&store, "tl-mixed-owners", "t2b").await,
        TaskStatus::Pending
    );

    // t1a completes → t2a dispatches; t2b still Pending (agent-b is busy on t1b).
    store
        .set_task_status("team-a", "tl-mixed-owners", "t1a", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-mixed-owners".to_string(),
            &"t1a".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 3);
    assert_eq!(dispatcher.calls()[2].1, "t2a");
    assert_eq!(
        task_status(&store, "tl-mixed-owners", "t2b").await,
        TaskStatus::Pending
    );

    // t1b completes → t2b dispatches.
    store
        .set_task_status("team-a", "tl-mixed-owners", "t1b", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-mixed-owners".to_string(),
            &"t1b".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 4);
    assert_eq!(dispatcher.calls()[3].1, "t2b");
}

// ---- Watchdog -------------------------------------------------------

async fn setup_with_watchdog(
    grace: Duration,
) -> (
    tempfile::TempDir,
    Arc<TasklistStore>,
    Arc<RecordingDispatcher>,
    Arc<InstanceRegistry>,
    TaskFeeder,
) {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(grace);
    (tmp, store, dispatcher, instance_registry, feeder)
}

#[tokio::test]
async fn watchdog_no_op_when_no_active_tasklists() {
    let (_tmp, _store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_millis(0)).await;
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 0);
    assert!(dispatcher.calls().is_empty());
}

#[tokio::test]
async fn watchdog_no_op_when_instance_registry_not_wired() {
    // Without with_instance_registry, watchdog_tick is a no-op even if
    // there are active tasklists with InProgress tasks.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-noreg",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-noreg".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 0);
    assert_eq!(
        dispatcher.calls().len(),
        1,
        "no extra dispatch from a wiring-less watchdog"
    );
}

#[tokio::test]
async fn watchdog_skips_tasks_with_active_runs() {
    // The agent has an active run registered → InProgress task is "alive",
    // watchdog must leave it alone.
    let (_tmp, store, dispatcher, instance_registry, feeder) =
        setup_with_watchdog(Duration::from_millis(0)).await;

    let tl = tasklist(
        "team-a",
        "tl-alive",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-alive".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // The watchdog checks the same registry key format as the agent_runner:
    // "tasklist:{tasklist_id}:{agent_id}" (RunScope::Tasklist registry key).
    instance_registry
        .register_run(&"tasklist:tl-alive:worker".to_string(), "run-1")
        .await;

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 0);
    assert_eq!(
        dispatcher.calls().len(),
        1,
        "no reprompt while agent has an active run"
    );
    assert_eq!(
        task_snapshot(&store, "tl-alive", "t1").await.attempt_count,
        0
    );
}

#[tokio::test]
async fn watchdog_honours_grace_period() {
    // Grace = 5 minutes (effectively forever for this test). The task was
    // just dispatched, agent has no active runs (slow startup) — but
    // watchdog must NOT reprompt because we're still within the grace.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_secs(300)).await;

    let tl = tasklist(
        "team-a",
        "tl-grace",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-grace".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // Agent has no active runs at all, but task was just dispatched.
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 0, "grace period should suppress recovery");
    assert_eq!(dispatcher.calls().len(), 1);
}

#[test]
fn default_watchdog_grace_tolerates_real_run_startup_latency() {
    // Regression guard. The production watchdog runs with
    // DEFAULT_WATCHDOG_GRACE (no override). A busy single-executor SEQ
    // tasklist was observed taking ~130s between a task being marked
    // InProgress and its run actually registering — during which
    // `running_count` reads zero. A 60s default reaped those healthy,
    // still-starting runs as "stuck", flipping them to a terminal state
    // the in-flight run could never recover from and stalling the group.
    // The grace must stay well above realistic startup latency; the
    // watchdog is only a backstop (genuine non-reporting ends are caught
    // event-driven by `on_run_ended`).
    //
    // The per-task `run_observed` bit now distinguishes "never started"
    // (cold start — keep honoring this grace) from "registered then
    // vanished" (genuine drop — recover on the next tick regardless of
    // grace), so an observed-then-gone task no longer has to wait out the
    // full window. The grace floor still protects the cold-start case, so
    // keep it at 300s.
    assert!(
        DEFAULT_WATCHDOG_GRACE >= Duration::from_secs(300),
        "watchdog grace {:?} is too tight to survive real run-startup latency",
        DEFAULT_WATCHDOG_GRACE,
    );
}

#[tokio::test]
async fn watchdog_recovers_stuck_task_when_agent_idle() {
    // Past grace, agent has zero active runs, task is InProgress → stuck.
    // Watchdog reprompts via the dispatcher.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_millis(0)).await;

    let tl = tasklist(
        "team-a",
        "tl-stuck",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-stuck".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // Wait a hair past zero-grace before ticking.
    tokio::time::sleep(Duration::from_millis(5)).await;

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 1);
    assert_eq!(
        dispatcher.calls().len(),
        2,
        "watchdog reprompts the stuck task"
    );
    assert_eq!(dispatcher.calls()[1].0, "worker");
    assert_eq!(dispatcher.calls()[1].1, "t1");
    assert!(
        dispatcher.calls()[1].2.contains("Stuck task"),
        "reprompt prompt should explain stuck task, got: {}",
        dispatcher.calls()[1].2,
    );

    let t1 = task_snapshot(&store, "tl-stuck", "t1").await;
    assert_eq!(t1.status, TaskStatus::InProgress);
    assert_eq!(t1.attempt_count, 1);
    assert_eq!(t1.error_log.len(), 1);
    assert!(t1.error_log[0].contains("watchdog"));
}

#[tokio::test]
async fn recover_stuck_task_concurrent_calls_do_not_double_dispatch() {
    // Regression test for the chain-B double-dispatch race: two concurrent
    // recovery attempts for the SAME still-`InProgress` task must not both
    // reach the dispatcher. `recover_stuck_task` reads the task unlocked
    // before it ever takes the per-tasklist write lock; the task's status
    // never leaves `InProgress` during recovery (there is no state
    // transition to serialize on), so without a dispatch-generation CAS,
    // nothing stops a second concurrent caller from also passing the
    // `status == InProgress` check and also calling
    // `dispatcher.dispatch_task` — two live executor runs for one task.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_millis(0)).await;
    let feeder = Arc::new(feeder);

    let tl = tasklist(
        "team-a",
        "tl-race",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    // Put t1 InProgress directly rather than dispatching through the feeder
    // first — recover_stuck_task only requires InProgress, and this keeps
    // the dispatcher recorder empty going into the race so a final count of
    // 1 vs 2 is unambiguous.
    store
        .mutate("team-a", "tl-race", |tl| {
            tl.groups[0].tasks[0].status = TaskStatus::InProgress;
            Ok(())
        })
        .await
        .unwrap();

    let owner = TasklistOwner::Team {
        team_id: "team-a".to_string(),
    };

    // Two concurrent callers racing to recover the same task — the shape of
    // a watchdog tick racing `kick_and_reconcile` (or two watchdog ticks)
    // both observing the same stuck task. `recover_stuck_task`'s initial
    // tasklist read goes through `tokio::fs`, which always yields to the
    // runtime rather than resolving inline — spawning both calls up front
    // and awaiting them together is enough to let them genuinely interleave
    // before either commits its claim, with no sleep-based synchronization.
    let owner_a = owner.clone();
    let feeder_a = Arc::clone(&feeder);
    let ha = tokio::spawn(async move {
        feeder_a
            .recover_stuck_task(
                &owner_a,
                &"tl-race".to_string(),
                &"worker".to_string(),
                &"t1".to_string(),
            )
            .await
    });
    let owner_b = owner.clone();
    let feeder_b = Arc::clone(&feeder);
    let hb = tokio::spawn(async move {
        feeder_b
            .recover_stuck_task(
                &owner_b,
                &"tl-race".to_string(),
                &"worker".to_string(),
                &"t1".to_string(),
            )
            .await
    });

    let (ra, rb) = tokio::join!(ha, hb);
    ra.unwrap().unwrap();
    rb.unwrap().unwrap();

    let calls = dispatcher.calls();
    let t1_dispatches: Vec<_> = calls.iter().filter(|c| c.1 == "t1").collect();
    assert_eq!(
        t1_dispatches.len(),
        1,
        "two concurrent recover_stuck_task calls for the same task must \
         collapse to exactly one dispatch; got {}: {:?}",
        t1_dispatches.len(),
        calls,
    );
}

#[tokio::test]
async fn watchdog_recovers_observed_then_vanished_within_grace() {
    // A run that registered (observed-alive) and then disappeared is a
    // genuine drop, not a slow start. The watchdog must recover it on the
    // next tick even though we are still well within the cold-start grace
    // window — that is the whole point of the per-task observed bit.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_secs(300)).await;

    let tl = tasklist(
        "team-a",
        "tl-observed",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-observed".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // The run registered at some point (observed)…
    feeder
        .mark_run_observed(&"tl-observed".to_string(), &"t1".to_string())
        .await;
    // …but is now gone: nothing is registered in the InstanceRegistry, so
    // running_count reads zero. Observed + zero runs = genuine drop.

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(
        recovered, 1,
        "observed-then-vanished task must recover within grace"
    );
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].0, "worker");
    assert_eq!(dispatcher.calls()[1].1, "t1");
    assert_eq!(
        task_snapshot(&store, "tl-observed", "t1").await.attempt_count,
        1
    );
}

#[tokio::test]
async fn watchdog_within_grace_not_observed_is_protected() {
    // Cold-start protection preserved: a task dispatched but whose run has
    // not yet registered (never observed) must NOT be recovered while
    // inside the grace window. This is the contrast case to the
    // observed-then-vanished test above — same setup, no observed bit.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_secs(300)).await;

    let tl = tasklist(
        "team-a",
        "tl-cold",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-cold".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // No mark_run_observed: the run is still cold-starting.
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(
        recovered, 0,
        "cold-starting (never-observed) task must be protected by grace"
    );
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(
        task_snapshot(&store, "tl-cold", "t1").await.attempt_count,
        0
    );
}

#[tokio::test]
async fn watchdog_clears_observed_on_redispatch_no_fast_reap_loop() {
    // After recovering an observed-then-vanished task, the observed bit
    // must be cleared so the re-dispatched run earns a fresh cold-start
    // grace window. A second immediate tick must therefore NOT reap it
    // again — otherwise the watchdog would spin the same task to Failed in
    // back-to-back ticks.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_secs(300)).await;

    let tl = tasklist(
        "team-a",
        "tl-reloop",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-reloop".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    feeder
        .mark_run_observed(&"tl-reloop".to_string(), &"t1".to_string())
        .await;

    // First tick: observed-then-vanished → recover (attempt_count → 1).
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 1);
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        task_snapshot(&store, "tl-reloop", "t1").await.attempt_count,
        1
    );

    // Second tick right away: the re-dispatch cleared the observed bit and
    // refreshed the dispatch timestamp, so the fresh run is cold-starting
    // and protected by grace — no fast-reap loop.
    let recovered2 = feeder.watchdog_tick().await.unwrap();
    assert_eq!(
        recovered2, 0,
        "re-dispatched run must not be reaped on sight"
    );
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        task_snapshot(&store, "tl-reloop", "t1").await.attempt_count,
        1
    );
}

#[tokio::test]
async fn watchdog_fails_task_after_max_attempts() {
    // Three consecutive watchdog recoveries on the same task hit max
    // attempts → task transitions to Failed, tasklist halts.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_max_attempts(2)
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0));

    let tl = tasklist(
        "team-a",
        "tl-stuck-max",
        vec![
            group("g1", TaskGroupMode::Seq, vec![task("t1", "worker", "g1")]),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![task("t2", "summarizer", "g2")],
            ),
        ],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-stuck-max".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);

    // First watchdog tick: reprompt (attempt_count → 1).
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(feeder.watchdog_tick().await.unwrap(), 1);
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        task_snapshot(&store, "tl-stuck-max", "t1").await.status,
        TaskStatus::InProgress
    );

    // Second tick: attempt_count = 2 = max → task Failed, tasklist halted.
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(feeder.watchdog_tick().await.unwrap(), 1);
    assert_eq!(
        dispatcher.calls().len(),
        2,
        "no further dispatch after max attempts"
    );

    let t1 = task_snapshot(&store, "tl-stuck-max", "t1").await;
    assert_eq!(t1.status, TaskStatus::Failed);
    assert_eq!(t1.attempt_count, 2);

    let updated = store.get("team-a", "tl-stuck-max").await.unwrap().unwrap();
    assert_eq!(updated.status, TasklistStatus::Failed);
    assert_eq!(
        task_snapshot(&store, "tl-stuck-max", "t2").await.status,
        TaskStatus::Pending
    );
}

#[tokio::test]
async fn watchdog_skips_paused_tasklist() {
    // A paused tasklist with InProgress tasks must not be touched by the
    // watchdog — recovery only fires while Active.
    let (_tmp, store, dispatcher, _instance_registry, feeder) =
        setup_with_watchdog(Duration::from_millis(0)).await;

    let tl = tasklist(
        "team-a",
        "tl-paused",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-paused".to_string())
        .await
        .unwrap();
    feeder
        .pause(&"team-a".to_string(), &"tl-paused".to_string())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(5)).await;
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 0, "paused tasklist must be ignored");
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(
        task_snapshot(&store, "tl-paused", "t1").await.attempt_count,
        0
    );
}

#[tokio::test]
async fn watchdog_recovers_post_restart_with_no_dispatch_timestamp() {
    // Simulate a server restart: tasklist on-disk has an InProgress task
    // but the in-memory `dispatched_at` map is empty. With no timestamp,
    // the task must still be eligible for recovery (otherwise restart
    // would never reconcile stale InProgress state).
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_secs(300));

    // Build an Active tasklist whose task is already InProgress (as if the
    // previous server marked it so before crashing).
    let mut tl = tasklist(
        "team-a",
        "tl-restart",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create(&tl).await.unwrap();

    // Feeder has no dispatch timestamp for t1. Agent has no active runs.
    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(recovered, 1, "post-restart stale InProgress should recover");
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
    assert_eq!(
        task_snapshot(&store, "tl-restart", "t1")
            .await
            .attempt_count,
        1
    );
}

#[tokio::test]
async fn continue_failed_resets_failed_tasks_and_redispatches() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-cont",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    // Drive the tasklist into Failed via the normal failure path.
    feeder
        .start(&"team-a".to_string(), &"tl-cont".to_string())
        .await
        .unwrap();
    store
        .set_task_status("team-a", "tl-cont", "t1", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-cont".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get("team-a", "tl-cont")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Failed
    );
    assert_eq!(
        task_snapshot(&store, "tl-cont", "t1").await.status,
        TaskStatus::Failed
    );
    let dispatched_before = dispatcher.calls().len();

    // Continue: tasklist back to Active, t1 reset to Pending and re-dispatched.
    let updated = feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-cont".to_string())
        .await
        .unwrap();
    assert_eq!(updated.status, TasklistStatus::Active);

    // After advance(), t1 should have been picked up and is now InProgress
    // (re-dispatched). The persisted state reflects the dispatch.
    assert_eq!(
        task_snapshot(&store, "tl-cont", "t1").await.status,
        TaskStatus::InProgress
    );
    assert_eq!(dispatcher.calls().len(), dispatched_before + 1);
    assert_eq!(dispatcher.calls().last().unwrap().1, "t1");
}

#[tokio::test]
async fn continue_clears_attempt_count_and_error_log() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    // Build a tasklist that's already in Failed state with a task that has
    // accumulated retries and an error log (simulating max-attempts failure).
    let mut tl = tasklist(
        "team-a",
        "tl-reset",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    tl.groups[0].tasks[0].attempt_count = 3;
    tl.groups[0].tasks[0].error_log = vec![
        "missing output: notes.md".into(),
        "missing output: notes.md".into(),
        "permission denied".into(),
    ];
    store.create(&tl).await.unwrap();

    feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-reset".to_string())
        .await
        .unwrap();

    let snap = task_snapshot(&store, "tl-reset", "t1").await;
    assert_eq!(snap.status, TaskStatus::InProgress); // re-dispatched immediately
    assert_eq!(snap.attempt_count, 0, "Continue must reset attempt_count");
    assert!(snap.error_log.is_empty(), "Continue must clear error_log");
}

#[tokio::test]
async fn continue_rejects_non_failed_tasklist() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    // Active tasklist — Continue must refuse.
    let tl = tasklist(
        "team-a",
        "tl-active",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    let err = feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-active".to_string())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AoError::InvalidTasklistTransition(_)),
        "expected InvalidTasklistTransition, got {err:?}"
    );

    // Tasklist must remain Active (no partial mutation).
    assert_eq!(
        store
            .get("team-a", "tl-active")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Active
    );
}

#[tokio::test]
async fn continue_preserves_completed_and_other_terminal_tasks() {
    // Multi-group failure: Completed tasks stay Completed, only Failed
    // tasks are reset. Verifies Continue is surgical, not a full reset.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-mixed",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![
                    task("t1", "researcher-a", "g1"),
                    task("t2", "researcher-b", "g1"),
                ],
            ),
            group("g2", TaskGroupMode::Seq, vec![task("t3", "analyst", "g2")]),
        ],
    );
    // g1: t1 Completed, t2 Failed (caused tasklist failure, g2 never ran).
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Completed;
    tl.groups[0].tasks[1].status = TaskStatus::Failed;
    tl.groups[0].tasks[1].attempt_count = 3;
    store.create(&tl).await.unwrap();

    feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-mixed".to_string())
        .await
        .unwrap();

    // t1 stays Completed; t2 is reset and (re)dispatched as InProgress;
    // t3 stays Pending (g2 hasn't run yet).
    assert_eq!(
        task_snapshot(&store, "tl-mixed", "t1").await.status,
        TaskStatus::Completed
    );
    assert_eq!(
        task_snapshot(&store, "tl-mixed", "t2").await.status,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_snapshot(&store, "tl-mixed", "t3").await.status,
        TaskStatus::Pending
    );
    assert_eq!(
        task_snapshot(&store, "tl-mixed", "t2").await.attempt_count,
        0
    );
}

#[tokio::test]
async fn continue_rejects_when_team_has_other_active_tasklist() {
    // The one-active-slot invariant: if the user created a new tasklist
    // after the old one failed, Continue on the old one must refuse so
    // we don't end up with two Active tasklists for the team.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    // Failed tasklist (the one we'd try to continue).
    let mut failed_tl = tasklist(
        "team-a",
        "tl-old",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    failed_tl.status = TasklistStatus::Failed;
    failed_tl.groups[0].tasks[0].status = TaskStatus::Failed;
    store.create(&failed_tl).await.unwrap();

    // New active tasklist created after the failure (occupies the slot).
    let new_tl = tasklist(
        "team-a",
        "tl-new",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t2", "worker", "g1")],
        )],
    );
    store.create(&new_tl).await.unwrap();

    let err = feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-old".to_string())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AoError::TasklistAlreadyActive { .. }),
        "expected TasklistAlreadyActive, got {err:?}"
    );

    // Old tasklist still Failed; new tasklist still Active.
    assert_eq!(
        store.get("team-a", "tl-old").await.unwrap().unwrap().status,
        TasklistStatus::Failed
    );
    assert_eq!(
        store.get("team-a", "tl-new").await.unwrap().unwrap().status,
        TasklistStatus::Active
    );
}

#[tokio::test]
async fn skip_task_revives_tasklist_when_only_failure_and_advances() {
    // Single Failed task in a Failed tasklist. Skip flips it to Skipped,
    // revives the tasklist to Active, and advance() moves on. With no
    // remaining non-terminal tasks, the tasklist transitions to Completed.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-skip-last",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    tl.groups[0].tasks[0].attempt_count = 3;
    store.create(&tl).await.unwrap();
    let dispatched_before = dispatcher.calls().len();

    let updated = feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-last".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        task_snapshot(&store, "tl-skip-last", "t1").await.status,
        TaskStatus::Skipped
    );
    // Every group is now terminal (only Skipped + no Failed). advance()
    // transitions the tasklist to Completed.
    let final_state = store.get("team-a", "tl-skip-last").await.unwrap().unwrap();
    assert_eq!(final_state.status, TasklistStatus::Completed);
    // The intermediate revival to Active is reflected in the value
    // returned by skip_task before advance() finished walking the groups.
    assert_eq!(updated.status, TasklistStatus::Active);
    // Skip itself does not dispatch anything new (the only task is Skipped).
    assert_eq!(dispatcher.calls().len(), dispatched_before);
}

#[tokio::test]
async fn skip_task_keeps_tasklist_failed_when_other_failures_remain() {
    // Two Failed tasks. Skipping one must leave the tasklist Failed and
    // not dispatch anything (the other failure still blocks dispatch).
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-skip-partial",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![task("t1", "worker-a", "g1"), task("t2", "worker-b", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    tl.groups[0].tasks[1].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();
    let dispatched_before = dispatcher.calls().len();

    let updated = feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-partial".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(updated.status, TasklistStatus::Failed);
    assert_eq!(
        task_snapshot(&store, "tl-skip-partial", "t1").await.status,
        TaskStatus::Skipped
    );
    assert_eq!(
        task_snapshot(&store, "tl-skip-partial", "t2").await.status,
        TaskStatus::Failed
    );
    // No dispatch occurred — tasklist still Failed, advance() short-circuits.
    assert_eq!(dispatcher.calls().len(), dispatched_before);
}

#[tokio::test]
async fn skip_task_revives_then_advances_to_next_group() {
    // Failure in g1 with a pending task in g2. Skipping the only Failed
    // task must revive the tasklist AND let advance() move past g1 to
    // dispatch the first task of g2.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-skip-advance",
        vec![
            group("g1", TaskGroupMode::Seq, vec![task("t1", "worker", "g1")]),
            group("g2", TaskGroupMode::Seq, vec![task("t2", "analyst", "g2")]),
        ],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();
    let dispatched_before = dispatcher.calls().len();

    feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-advance".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(
        task_snapshot(&store, "tl-skip-advance", "t1").await.status,
        TaskStatus::Skipped
    );
    assert_eq!(
        task_snapshot(&store, "tl-skip-advance", "t2").await.status,
        TaskStatus::InProgress
    );
    assert_eq!(dispatcher.calls().len(), dispatched_before + 1);
    assert_eq!(dispatcher.calls().last().unwrap().1, "t2");
    assert_eq!(
        store
            .get("team-a", "tl-skip-advance")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Active
    );
}

#[tokio::test]
async fn skip_task_rejects_non_failed_task() {
    // Pending and InProgress tasks cannot be skipped via this path —
    // Skip is the recovery action for Failed tasks specifically.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-skip-invalid",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    // Task is Pending, not Failed.
    store.create(&tl).await.unwrap();

    let err = feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-invalid".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, AoError::InvalidTasklistTransition(_)),
        "expected InvalidTasklistTransition, got {err:?}"
    );
    // No mutation: task and tasklist are unchanged.
    assert_eq!(
        task_snapshot(&store, "tl-skip-invalid", "t1").await.status,
        TaskStatus::Pending
    );
    assert_eq!(
        store
            .get("team-a", "tl-skip-invalid")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Failed
    );
}

#[tokio::test]
async fn skip_task_rejects_when_team_has_other_active_tasklist() {
    // The one-active-slot invariant must hold across Skip-driven revivals
    // too: if skipping the last Failed task would revive the tasklist but
    // another tasklist already occupies the active slot, refuse.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut failed_tl = tasklist(
        "team-a",
        "tl-old",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    failed_tl.status = TasklistStatus::Failed;
    failed_tl.groups[0].tasks[0].status = TaskStatus::Failed;
    store.create(&failed_tl).await.unwrap();

    let new_tl = tasklist(
        "team-a",
        "tl-new",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t2", "worker", "g1")],
        )],
    );
    store.create(&new_tl).await.unwrap();

    let err = feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-old".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, AoError::TasklistAlreadyActive { .. }),
        "expected TasklistAlreadyActive, got {err:?}"
    );
    // Old tasklist still Failed (the task is NOT mutated when revival is
    // refused — pre-check happens before the persistence write).
    assert_eq!(
        task_snapshot(&store, "tl-old", "t1").await.status,
        TaskStatus::Failed
    );
    assert_eq!(
        store.get("team-a", "tl-old").await.unwrap().unwrap().status,
        TasklistStatus::Failed
    );
}

#[tokio::test]
async fn discard_active_tasklist_marks_pending_and_blocked_skipped() {
    // From an Active tasklist mid-flight: Pending and Blocked tasks are
    // marked Skipped (so the panel doesn't show stranded rows); InProgress
    // is left alone so the agent's current turn finishes naturally;
    // Completed/Failed/Skipped stay put. Tasklist flips to Cancelled.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-discard-active",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![
                    task("t1", "a", "g1"),
                    task("t2", "b", "g1"),
                    task("t3", "c", "g1"),
                    task("t4", "d", "g1"),
                ],
            ),
            group("g2", TaskGroupMode::Seq, vec![task("t5", "e", "g2")]),
        ],
    );
    tl.groups[0].tasks[0].status = TaskStatus::Completed;
    tl.groups[0].tasks[1].status = TaskStatus::InProgress;
    tl.groups[0].tasks[2].status = TaskStatus::Pending;
    tl.groups[0].tasks[3].status = TaskStatus::Blocked;
    // t5 stays Pending.
    store.create(&tl).await.unwrap();

    let updated = feeder
        .discard_tasklist(&"team-a".to_string(), &"tl-discard-active".to_string())
        .await
        .unwrap();

    assert_eq!(updated.status, TasklistStatus::Cancelled);
    assert_eq!(
        task_status(&store, "tl-discard-active", "t1").await,
        TaskStatus::Completed
    );
    assert_eq!(
        task_status(&store, "tl-discard-active", "t2").await,
        TaskStatus::InProgress
    );
    assert_eq!(
        task_status(&store, "tl-discard-active", "t3").await,
        TaskStatus::Skipped
    );
    assert_eq!(
        task_status(&store, "tl-discard-active", "t4").await,
        TaskStatus::Skipped
    );
    assert_eq!(
        task_status(&store, "tl-discard-active", "t5").await,
        TaskStatus::Skipped
    );
}

#[tokio::test]
async fn discard_paused_tasklist_succeeds() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-discard-paused",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Paused;
    store.create(&tl).await.unwrap();

    let updated = feeder
        .discard_tasklist(&"team-a".to_string(), &"tl-discard-paused".to_string())
        .await
        .unwrap();

    assert_eq!(updated.status, TasklistStatus::Cancelled);
    assert_eq!(
        task_status(&store, "tl-discard-paused", "t1").await,
        TaskStatus::Skipped
    );
}

#[tokio::test]
async fn discard_failed_tasklist_succeeds_and_preserves_failed_task() {
    // Failed tasks stay Failed (they're already terminal); only Pending /
    // Blocked tasks get auto-skipped. Tasklist itself flips to Cancelled.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-discard-failed",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1"), task("t2", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    // t2 stays Pending (next-up when t1 failed).
    store.create(&tl).await.unwrap();

    let updated = feeder
        .discard_tasklist(&"team-a".to_string(), &"tl-discard-failed".to_string())
        .await
        .unwrap();

    assert_eq!(updated.status, TasklistStatus::Cancelled);
    assert_eq!(
        task_status(&store, "tl-discard-failed", "t1").await,
        TaskStatus::Failed
    );
    assert_eq!(
        task_status(&store, "tl-discard-failed", "t2").await,
        TaskStatus::Skipped
    );
}

#[tokio::test]
async fn discard_rejects_terminal_tasklist() {
    // Completed and Cancelled tasklists cannot be discarded (no-op rather
    // than silent success — the popup shouldn't be offering Discard there).
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    for terminal in [TasklistStatus::Completed, TasklistStatus::Cancelled] {
        let id = format!("tl-discard-term-{:?}", terminal).to_lowercase();
        let mut tl = tasklist(
            "team-a",
            &id,
            vec![group(
                "g1",
                TaskGroupMode::Seq,
                vec![task("t1", "worker", "g1")],
            )],
        );
        tl.status = terminal;
        tl.groups[0].tasks[0].status = TaskStatus::Completed;
        store.create(&tl).await.unwrap();

        let err = feeder
            .discard_tasklist(&"team-a".to_string(), &id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AoError::InvalidTasklistTransition(_)),
            "expected InvalidTasklistTransition for {:?}, got {err:?}",
            terminal
        );
        // Status unchanged.
        assert_eq!(
            store.get("team-a", &id).await.unwrap().unwrap().status,
            terminal
        );
    }
}

#[tokio::test]
async fn discard_then_advance_is_noop() {
    // After discard, subsequent advance() calls (e.g. from a late
    // on_task_terminal for an in-flight task) must not dispatch anything.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-discard-noop",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1"), task("t2", "worker", "g1")],
        )],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    // t2 stays Pending.
    store.create(&tl).await.unwrap();

    feeder
        .discard_tasklist(&"team-a".to_string(), &"tl-discard-noop".to_string())
        .await
        .unwrap();

    // Simulate the in-flight t1 completing after the discard: terminal
    // notification should not re-dispatch t2 (or transition tasklist).
    store
        .set_task_status("team-a", "tl-discard-noop", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-discard-noop".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    assert!(
        dispatcher.calls().is_empty(),
        "expected no dispatch after discard, got {:?}",
        dispatcher.calls()
    );
    assert_eq!(
        store
            .get("team-a", "tl-discard-noop")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Cancelled
    );
}

#[tokio::test]
async fn replay_completed_tasklist_clones_plan_with_fresh_ids_and_dispatches() {
    // Replay from a Completed tasklist: original is left alone (still
    // Completed, original ids preserved), new tasklist exists with fresh
    // ids, all tasks Pending, status Active, fresh workspace + transcripts
    // dirs on disk, and dispatch has bootstrapped the first group.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-source",
        vec![
            group(
                "g1",
                TaskGroupMode::Par,
                vec![task("t1", "a", "g1"), task("t2", "b", "g1")],
            ),
            group("g2", TaskGroupMode::Seq, vec![task("t3", "c", "g2")]),
        ],
    );
    tl.status = TasklistStatus::Completed;
    for g in &mut tl.groups {
        for t in &mut g.tasks {
            t.status = TaskStatus::Completed;
            t.attempt_count = 2;
        }
    }
    store.create(&tl).await.unwrap();

    let new_tl = feeder
        .replay_tasklist(&"team-a".to_string(), &"tl-source".to_string())
        .await
        .unwrap();

    // New id, new group ids, new task ids — none collide with the source.
    assert_ne!(new_tl.id, "tl-source");
    let original_group_ids: HashSet<String> =
        ["g1".to_string(), "g2".to_string()].into_iter().collect();
    let original_task_ids: HashSet<String> =
        ["t1", "t2", "t3"].into_iter().map(String::from).collect();
    for g in &new_tl.groups {
        assert!(!original_group_ids.contains(&g.id));
        for t in &g.tasks {
            assert!(!original_task_ids.contains(&t.id));
            assert_eq!(t.attempt_count, 0);
            assert!(t.error_log.is_empty());
        }
    }

    // Plan shape preserved: same group count, same modes, same task count
    // per group, same owners + prompts + expected_outputs in order.
    assert_eq!(new_tl.groups.len(), tl.groups.len());
    for (orig, copy) in tl.groups.iter().zip(new_tl.groups.iter()) {
        assert_eq!(orig.mode, copy.mode);
        assert_eq!(orig.tasks.len(), copy.tasks.len());
        for (ot, ct) in orig.tasks.iter().zip(copy.tasks.iter()) {
            assert_eq!(ot.owner_agent_id, ct.owner_agent_id);
            assert_eq!(ot.prompt, ct.prompt);
            assert_eq!(ot.expected_outputs, ct.expected_outputs);
            assert_eq!(ct.status, TaskStatus::Pending);
        }
    }

    // Title + description carried over.
    assert_eq!(new_tl.title, tl.title);
    assert_eq!(new_tl.description, tl.description);
    // Status is Active (the bootstrap dispatch keeps it Active until first
    // terminal task is observed).
    assert_eq!(new_tl.status, TasklistStatus::Active);

    // Original tasklist untouched.
    let source = store.get("team-a", "tl-source").await.unwrap().unwrap();
    assert_eq!(source.status, TasklistStatus::Completed);
    for g in &source.groups {
        for t in &g.tasks {
            assert_eq!(t.status, TaskStatus::Completed);
        }
    }

    // Fresh workspace + transcripts dirs exist on disk for the new
    // tasklist (created by tasklist_store.create()).
    assert!(tokio::fs::try_exists(&new_tl.workspace_dir).await.unwrap());
    assert!(tokio::fs::try_exists(&new_tl.transcripts_dir)
        .await
        .unwrap());

    // Bootstrap dispatched the first group (Par mode, two distinct
    // owners): one call per task in g1, none for g2 yet.
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2, "expected 2 dispatches, got {calls:?}");
    let dispatched_owners: HashSet<String> = calls.iter().map(|c| c.0.clone()).collect();
    assert_eq!(
        dispatched_owners,
        ["a".to_string(), "b".to_string()].into_iter().collect()
    );
}

#[tokio::test]
async fn replay_cancelled_tasklist_succeeds() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-cancelled",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Cancelled;
    tl.groups[0].tasks[0].status = TaskStatus::Skipped;
    store.create(&tl).await.unwrap();

    let new_tl = feeder
        .replay_tasklist(&"team-a".to_string(), &"tl-cancelled".to_string())
        .await
        .unwrap();
    assert_eq!(new_tl.status, TasklistStatus::Active);
    assert_eq!(new_tl.groups[0].tasks[0].status, TaskStatus::Pending);
}

#[tokio::test]
async fn replay_failed_tasklist_succeeds() {
    // Replay from Failed is allowed alongside Continue: Continue retries
    // only failed tasks (preserving completed work); Replay starts fresh.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-failed",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1"), task("t2", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Completed;
    tl.groups[0].tasks[1].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();

    let new_tl = feeder
        .replay_tasklist(&"team-a".to_string(), &"tl-failed".to_string())
        .await
        .unwrap();

    assert_eq!(new_tl.status, TasklistStatus::Active);
    // Both tasks reset to Pending (no preservation of the previously-
    // completed task — that's Continue's job).
    for t in &new_tl.groups[0].tasks {
        assert_eq!(t.status, TaskStatus::Pending);
    }

    // Original Failed tasklist untouched.
    let source = store.get("team-a", "tl-failed").await.unwrap().unwrap();
    assert_eq!(source.status, TasklistStatus::Failed);
}

#[tokio::test]
async fn replay_rejects_active_or_paused_tasklist() {
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    for non_terminal in [TasklistStatus::Active, TasklistStatus::Paused] {
        let id = format!("tl-replay-non-term-{:?}", non_terminal).to_lowercase();
        let mut tl = tasklist(
            "team-a",
            &id,
            vec![group(
                "g1",
                TaskGroupMode::Seq,
                vec![task("t1", "worker", "g1")],
            )],
        );
        tl.status = non_terminal;
        store.create(&tl).await.unwrap();

        let err = feeder
            .replay_tasklist(&"team-a".to_string(), &id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AoError::InvalidTasklistTransition(_)),
            "expected InvalidTasklistTransition for {:?}, got {err:?}",
            non_terminal
        );

        // Discard the active/paused so the next loop iteration can claim
        // the slot (otherwise the second create() would 409).
        feeder
            .discard_tasklist(&"team-a".to_string(), &id)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn replay_rejects_when_team_has_other_active_tasklist() {
    // Another Active/Paused tasklist for the same team must be resolved
    // before replay can claim the active slot.
    let (_tmp, store, _dispatcher, feeder) = setup().await;

    let mut source = tasklist(
        "team-a",
        "tl-replay-source",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    source.status = TasklistStatus::Completed;
    source.groups[0].tasks[0].status = TaskStatus::Completed;
    store.create(&source).await.unwrap();

    let occupier = tasklist(
        "team-a",
        "tl-occupier",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    // Default tasklist() builder gives Active status — exactly what we want.
    store.create(&occupier).await.unwrap();

    let err = feeder
        .replay_tasklist(&"team-a".to_string(), &"tl-replay-source".to_string())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AoError::TasklistAlreadyActive { .. }),
        "expected TasklistAlreadyActive, got {err:?}"
    );

    // Source tasklist still Completed (not mutated).
    assert_eq!(
        store
            .get("team-a", "tl-replay-source")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Completed
    );
}

#[tokio::test]
async fn replay_returns_tasklist_not_found_for_missing_source() {
    let (_tmp, _store, _dispatcher, feeder) = setup().await;
    let err = feeder
        .replay_tasklist(&"team-a".to_string(), &"does-not-exist".to_string())
        .await
        .unwrap_err();
    assert!(matches!(err, AoError::TasklistNotFound(_)), "got {err:?}");
}

// --- append-to-Active continues running through new tasks ----------------

#[tokio::test]
async fn advance_after_par_append_dispatches_new_task_for_free_agent() {
    // PAR group with t1 already InProgress (agent "a" claimed). User
    // appends t2 owned by a different agent. advance() must dispatch t2
    // immediately (free agent slot in the same PAR group), without
    // re-dispatching t1 and without changing the tasklist's status.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-par-append",
        vec![group("g1", TaskGroupMode::Par, vec![task("t1", "a", "g1")])],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-par-append".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
    assert_eq!(
        task_status(&store, "tl-par-append", "t1").await,
        TaskStatus::InProgress
    );

    // Simulate what the HTTP append handler does: persist a new Pending
    // task in the same PAR group, then ask the feeder to advance.
    let appended = task("t2", "b", "g1");
    let updated = store
        .mutate("team-a", "tl-par-append", |tl| {
            tl.groups[0].tasks.push(appended);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(updated.status, TasklistStatus::Active);

    feeder.advance(&updated).await.unwrap();

    assert_eq!(
        dispatcher.calls().len(),
        2,
        "PAR append for a free agent must dispatch immediately"
    );
    assert_eq!(dispatcher.calls()[1].1, "t2");
    assert_eq!(
        task_status(&store, "tl-par-append", "t2").await,
        TaskStatus::InProgress
    );
    // t1 is NOT re-dispatched: registry still maps its agent to t1.
    assert_eq!(
        task_status(&store, "tl-par-append", "t1").await,
        TaskStatus::InProgress
    );
    // Tasklist status unchanged.
    assert_eq!(
        store
            .get("team-a", "tl-par-append")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Active
    );
}

#[tokio::test]
async fn advance_after_seq_append_does_not_double_dispatch_when_in_flight() {
    // SEQ group with t1 InProgress. User appends t2 to the same SEQ group.
    // advance() must NOT dispatch t2 yet (SEQ in_flight guard); the next
    // on_task_terminal after t1 completes is what picks t2 up. This is the
    // "one-pass continuation" promise: no group restart, just the natural
    // SEQ progression once the current task finishes.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-seq-append",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-seq-append".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");

    // Append t2 to the in-flight SEQ group, then kick advance.
    let appended = task("t2", "worker", "g1");
    let updated = store
        .mutate("team-a", "tl-seq-append", |tl| {
            tl.groups[0].tasks.push(appended);
            Ok(())
        })
        .await
        .unwrap();
    feeder.advance(&updated).await.unwrap();

    // No double-dispatch: t1 still in flight, t2 stays Pending.
    assert_eq!(
        dispatcher.calls().len(),
        1,
        "SEQ in_flight guard must prevent double-dispatch on append"
    );
    assert_eq!(
        task_status(&store, "tl-seq-append", "t2").await,
        TaskStatus::Pending
    );
    // Tasklist remains Active and was not restarted.
    assert_eq!(
        store
            .get("team-a", "tl-seq-append")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Active
    );

    // When t1 completes, on_task_terminal advances and t2 dispatches —
    // proving the new task continues in the same run without manual
    // intervention.
    store
        .set_task_status("team-a", "tl-seq-append", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-a".to_string(),
            },
            &"tl-seq-append".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].1, "t2");
    assert_eq!(
        task_status(&store, "tl-seq-append", "t2").await,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn advance_on_paused_tasklist_does_not_dispatch_appended_task() {
    // The HTTP append handler calls advance() unconditionally; for Paused
    // tasklists the status guard inside advance() must keep it inert until
    // the user resumes.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut tl = tasklist(
        "team-a",
        "tl-paused-append",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Paused;
    store.create(&tl).await.unwrap();

    let appended = task("t2", "worker", "g1");
    let updated = store
        .mutate("team-a", "tl-paused-append", |tl| {
            tl.groups[0].tasks.push(appended);
            Ok(())
        })
        .await
        .unwrap();
    feeder.advance(&updated).await.unwrap();

    assert!(
        dispatcher.calls().is_empty(),
        "Paused tasklist must not dispatch on advance; got {:?}",
        dispatcher.calls()
    );
    assert_eq!(
        store
            .get("team-a", "tl-paused-append")
            .await
            .unwrap()
            .unwrap()
            .status,
        TasklistStatus::Paused
    );
    assert_eq!(
        task_status(&store, "tl-paused-append", "t2").await,
        TaskStatus::Pending
    );
}

// ---- comments folded into dispatched prompt -----------------------------

use ao_protocol::tasklist::TaskComment;
use chrono::TimeZone;

fn user_comment(id: &str, body: &str, ts_secs: i64) -> TaskComment {
    TaskComment {
        id: id.to_string(),
        author_id: "user".to_string(),
        author_kind: TaskCommentAuthorKind::User,
        body: body.to_string(),
        created_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
    }
}

fn agent_comment(id: &str, agent: &str, body: &str, ts_secs: i64) -> TaskComment {
    TaskComment {
        id: id.to_string(),
        author_id: agent.to_string(),
        author_kind: TaskCommentAuthorKind::Agent,
        body: body.to_string(),
        created_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
    }
}

#[test]
fn build_dispatch_prompt_with_no_comments_returns_task_prompt_byte_for_byte() {
    let mut t = task("t1", "worker", "g1");
    t.prompt = "Do the thing.\nWith multiple lines.".to_string();
    // Sanity: ensure the test fixture starts with an empty comments vec.
    assert!(t.comments.is_empty());
    let rendered = build_dispatch_prompt(&t);
    assert_eq!(rendered, t.prompt);
}

#[test]
fn build_dispatch_prompt_appends_user_comment_block() {
    let mut t = task("t1", "worker", "g1");
    t.prompt = "Summarize the report.".to_string();
    t.comments = vec![user_comment("c1", "Focus on Q4 numbers.", 1_700_000_000)];
    let expected = "Summarize the report.\n\n---\nAdditional context (in chronological order):\n- [user: user] Focus on Q4 numbers.\n";
    assert_eq!(build_dispatch_prompt(&t), expected);
}

#[test]
fn build_dispatch_prompt_appends_agent_comment_with_author_id() {
    let mut t = task("t1", "worker", "g1");
    t.prompt = "Draft the spec.".to_string();
    t.comments = vec![agent_comment(
        "c1",
        "coordinator",
        "Match the existing template under docs/specs.",
        1_700_000_000,
    )];
    let rendered = build_dispatch_prompt(&t);
    assert!(
        rendered.starts_with(
            "Draft the spec.\n\n---\nAdditional context (in chronological order):\n"
        ),
        "augmentation header missing: {rendered}"
    );
    assert!(
        rendered
            .contains("- [agent: coordinator] Match the existing template under docs/specs.\n"),
        "agent comment line missing: {rendered}"
    );
}

#[test]
fn build_dispatch_prompt_preserves_comment_chronological_order() {
    let mut t = task("t1", "worker", "g1");
    t.prompt = "Investigate the alert.".to_string();
    // Insertion order is the chronological order (comments are stored
    // in insertion order); the helper must emit them in that order.
    t.comments = vec![
        user_comment("c1", "First note.", 1_700_000_000),
        agent_comment("c2", "lead", "Second note.", 1_700_000_100),
        user_comment("c3", "Third note.", 1_700_000_200),
    ];
    let rendered = build_dispatch_prompt(&t);
    let block_start = rendered.find("- [").expect("augmentation block present");
    let block = &rendered[block_start..];
    let pos1 = block.find("First note.").expect("first comment present");
    let pos2 = block.find("Second note.").expect("second comment present");
    let pos3 = block.find("Third note.").expect("third comment present");
    assert!(pos1 < pos2 && pos2 < pos3, "comments out of order: {block}");
}

#[tokio::test]
async fn dispatch_one_uses_augmented_prompt_when_task_has_comments() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut t1 = task("t1", "worker", "g1");
    t1.prompt = "Run the migration.".to_string();
    t1.comments = vec![
        user_comment("c1", "Use the staging DB.", 1_700_000_000),
        agent_comment("c2", "coordinator", "Skip table foo.", 1_700_000_100),
    ];
    let tl = tasklist(
        "team-a",
        "tl-augmented",
        vec![group("g1", TaskGroupMode::Seq, vec![t1])],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-augmented".to_string())
        .await
        .unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    let prompt = &calls[0].2;
    assert!(prompt.starts_with("Run the migration."), "prompt: {prompt}");
    assert!(
        prompt.contains("Additional context (in chronological order):"),
        "augmentation header missing: {prompt}"
    );
    assert!(
        prompt.contains("- [user: user] Use the staging DB."),
        "user comment missing: {prompt}"
    );
    assert!(
        prompt.contains("- [agent: coordinator] Skip table foo."),
        "agent comment missing: {prompt}"
    );
}

#[tokio::test]
async fn dispatch_one_does_not_modify_prompt_when_no_comments() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let mut t1 = task("t1", "worker", "g1");
    t1.prompt = "Plain task prompt.".to_string();
    // no comments
    let tl = tasklist(
        "team-a",
        "tl-plain",
        vec![group("g1", TaskGroupMode::Seq, vec![t1])],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-plain".to_string())
        .await
        .unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, "Plain task prompt.");
}

#[tokio::test]
async fn par_group_skips_unowned_pending_tasks() {
    // Pending tasks with empty owner_agent_id are awaiting
    // coordinator routing. The PAR dispatch loop must skip them entirely
    // — they stay Pending, the regular dispatcher never tries them — so
    // sibling owned tasks dispatch normally.
    let (_tmp, store, dispatcher, feeder) = setup().await;
    let mut unowned = task("t-unowned", "", "g1");
    unowned.prompt = "Unowned, awaiting routing".to_string();
    let tl = tasklist(
        "team-a",
        "tl-par-unowned",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![unowned, task("t2", "researcher-b", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-par-unowned".to_string())
        .await
        .unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1, "only the owned sibling dispatches");
    assert_eq!(calls[0].1, "t2");
    // Unowned task is still Pending; owned sibling is now InProgress.
    assert_eq!(
        task_status(&store, "tl-par-unowned", "t-unowned").await,
        TaskStatus::Pending,
    );
    assert_eq!(
        task_status(&store, "tl-par-unowned", "t2").await,
        TaskStatus::InProgress,
    );
}

#[tokio::test]
async fn seq_group_blocks_on_first_unowned_task() {
    // In SEQ mode the cursor parks at the first unowned Pending
    // task — the dispatcher does NOT skip ahead to a later owned task,
    // since that would violate sequential ordering. The unowned task
    // stays Pending and so does everything after it until the
    // coordinator (or user) sets an owner.
    let (_tmp, store, dispatcher, feeder) = setup().await;
    let mut unowned = task("t-unowned", "", "g1");
    unowned.prompt = "Unowned, blocking SEQ".to_string();
    let tl = tasklist(
        "team-a",
        "tl-seq-blocked",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![unowned, task("t-after", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-seq-blocked".to_string())
        .await
        .unwrap();

    // No dispatch fired: SEQ cursor parked on the unowned head.
    assert!(dispatcher.calls().is_empty());
    assert_eq!(
        task_status(&store, "tl-seq-blocked", "t-unowned").await,
        TaskStatus::Pending,
    );
    assert_eq!(
        task_status(&store, "tl-seq-blocked", "t-after").await,
        TaskStatus::Pending,
    );
}

#[tokio::test]
async fn assigning_owner_to_unowned_seq_task_unblocks_dispatch() {
    // After the coordinator (or user) writes an owner_agent_id on a
    // previously-unowned SEQ task, the next advance() call dispatches it
    // and the SEQ chain resumes. This is the regression check for "tasks
    // with null owner are never picked up by the regular dispatch loop
    // until an owner is assigned" — once assigned, they ARE picked up.
    let (_tmp, store, dispatcher, feeder) = setup().await;
    let mut unowned = task("t-unowned", "", "g1");
    unowned.prompt = "Unowned at first".to_string();
    let tl = tasklist(
        "team-a",
        "tl-seq-assign",
        vec![group("g1", TaskGroupMode::Seq, vec![unowned])],
    );
    store.create(&tl).await.unwrap();

    feeder
        .start(&"team-a".to_string(), &"tl-seq-assign".to_string())
        .await
        .unwrap();
    assert!(dispatcher.calls().is_empty(), "dispatcher skips unowned");

    // Coordinator assigns an owner via mutate (simulating a future
    // assign endpoint or a direct persistence write).
    store
        .mutate("team-a", "tl-seq-assign", |tl| {
            for group in &mut tl.groups {
                for t in &mut group.tasks {
                    if t.id == "t-unowned" {
                        t.owner_agent_id = "worker".to_string();
                    }
                }
            }
            Ok(())
        })
        .await
        .unwrap();

    let updated = store.get("team-a", "tl-seq-assign").await.unwrap().unwrap();
    feeder.advance(&updated).await.unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "worker");
    assert_eq!(calls[0].1, "t-unowned");
    assert_eq!(
        task_status(&store, "tl-seq-assign", "t-unowned").await,
        TaskStatus::InProgress,
    );
}

// === lifecycle wake/sleep wiring ===
//
// The `tasklist_lifecycle` module owns the predicates and exposes
// `emit_wake` / `maybe_emit_sleep` helpers — these tests verify the
// wiring at the call sites (continue_tasklist, skip_task, on_task_terminal).

use crate::event_bus::EventBus;
use ao_protocol::event::{AgentEvent, AgentEventPayload};
use tokio::sync::broadcast;

async fn setup_with_bus() -> (
    tempfile::TempDir,
    Arc<TasklistStore>,
    Arc<RecordingDispatcher>,
    TaskFeeder,
    Arc<EventBus>,
    broadcast::Receiver<AgentEvent>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    (tmp, store, dispatcher, feeder, bus, rx)
}

/// Drain the receiver and return every `TasklistWoke` payload's reason.
async fn collect_wake_reasons(rx: &mut broadcast::Receiver<AgentEvent>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => {
                if let AgentEventPayload::TasklistWoke { reason, .. } = event.payload {
                    out.push(reason);
                }
            }
            Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn continue_tasklist_emits_task_revived_wake() {
    let (_tmp, store, _dispatcher, feeder, _bus, mut rx) = setup_with_bus().await;

    let mut tl = tasklist(
        "team-a",
        "tl-revive",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();

    feeder
        .continue_tasklist(&"team-a".to_string(), &"tl-revive".to_string())
        .await
        .unwrap();

    let reasons = collect_wake_reasons(&mut rx).await;
    assert!(
        reasons.contains(&"task_revived".to_string()),
        "expected task_revived wake, got {:?}",
        reasons
    );
}

#[tokio::test]
async fn skip_task_revival_emits_task_revived_wake() {
    let (_tmp, store, _dispatcher, feeder, _bus, mut rx) = setup_with_bus().await;

    // Build a Failed tasklist with a single Failed task — skipping it will
    // flip the tasklist back to Active.
    let mut tl = tasklist(
        "team-a",
        "tl-skip-revive",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();

    feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-revive".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    let reasons = collect_wake_reasons(&mut rx).await;
    assert!(
        reasons.contains(&"task_revived".to_string()),
        "expected task_revived wake, got {:?}",
        reasons
    );
}

#[tokio::test]
async fn skip_task_without_revival_does_not_emit_wake() {
    let (_tmp, store, _dispatcher, feeder, _bus, mut rx) = setup_with_bus().await;

    // Two failed tasks: skipping one leaves the tasklist Failed, so no wake.
    let mut tl = tasklist(
        "team-a",
        "tl-skip-stay-failed",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1"), task("t2", "worker", "g1")],
        )],
    );
    tl.status = TasklistStatus::Failed;
    tl.groups[0].tasks[0].status = TaskStatus::Failed;
    tl.groups[0].tasks[1].status = TaskStatus::Failed;
    store.create(&tl).await.unwrap();

    feeder
        .skip_task(
            &"team-a".to_string(),
            &"tl-skip-stay-failed".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    let reasons = collect_wake_reasons(&mut rx).await;
    assert!(
        reasons.is_empty(),
        "no wake expected (tasklist still Failed), got {:?}",
        reasons
    );
}

// ---- agent-scope integration -------------------------------------------

/// Build an agent-owned tasklist using the real paths that
/// `store.create_for_agent` will populate.
fn agent_tasklist(
    data_root: &DataRoot,
    agent_id: &str,
    id: &str,
    groups: Vec<TaskGroup>,
) -> Tasklist {
    agent_tasklist_with_project(data_root, agent_id, id, groups, None)
}

fn agent_tasklist_with_project(
    data_root: &DataRoot,
    agent_id: &str,
    id: &str,
    groups: Vec<TaskGroup>,
    project_id: Option<String>,
) -> Tasklist {
    let workspace = data_root.agent_tasklist_workspace_dir(agent_id, id);
    let transcripts = data_root.agent_tasklist_transcripts_dir(agent_id, id);
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Agent {
            agent_id: agent_id.to_string(),
        },
        team_id: None,
        title: format!("Agent Tasklist {id}"),
        description: String::new(),
        status: TasklistStatus::Active,
        groups,
        workspace_dir: workspace.to_string_lossy().to_string(),
        transcripts_dir: transcripts.to_string_lossy().to_string(),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id,
        thread_id: None,
        }
}

/// Helper: snapshot a task from an agent-owned tasklist.
async fn agent_task_snapshot(
    store: &TasklistStore,
    agent_id: &str,
    tl_id: &str,
    task_id: &str,
) -> Task {
    let tl = store.get_for_agent(agent_id, tl_id).await.unwrap().unwrap();
    tl.groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn agent_owned_tasklist_seq_dispatches_and_advances() {
    // Agent-scope smoke test. An agent-owned SEQ tasklist advances
    // identically to a team-owned one — dispatcher receives the correct
    // owner_agent_id and owner variant; on_task_terminal with
    // TasklistOwner::Agent routes to the agent persistence paths.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let owner = TasklistOwner::Agent {
        agent_id: "solo-agent".to_string(),
    };

    let tl = agent_tasklist(
        &data_root,
        "solo-agent",
        "tl-agent-seq",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "solo-agent", "g1"),
                agent_task("t2", "solo-agent", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();

    // Bootstrap: advance() dispatches t1, leaves t2 Pending.
    feeder.advance(&tl).await.unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].0, "solo-agent");
    assert_eq!(dispatcher.calls()[0].1, "t1");

    // Mark t1 completed on disk via the owner-aware API.
    store
        .set_task_status_by_owner(&owner, "tl-agent-seq", "t1", TaskStatus::Completed)
        .await
        .unwrap();

    // on_task_terminal with Agent owner clears registry and advances.
    feeder
        .on_task_terminal(&owner, &"tl-agent-seq".to_string(), &"t1".to_string())
        .await
        .unwrap();

    // t2 must now be dispatched.
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(dispatcher.calls()[1].0, "solo-agent");
    assert_eq!(dispatcher.calls()[1].1, "t2");

    assert_eq!(
        agent_task_snapshot(&store, "solo-agent", "tl-agent-seq", "t2")
            .await
            .status,
        TaskStatus::InProgress,
    );
    assert_eq!(
        feeder
            .current_task_for_agent(&"tl-agent-seq".to_string(), &"solo-agent".to_string())
            .await,
        Some("t2".to_string()),
    );

    // Complete t2: all groups terminal → tasklist auto-completes.
    store
        .set_task_status_by_owner(&owner, "tl-agent-seq", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-agent-seq".to_string(), &"t2".to_string())
        .await
        .unwrap();

    let final_tl = store
        .get_for_agent("solo-agent", "tl-agent-seq")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_tl.status, TasklistStatus::Completed);
    assert_eq!(
        dispatcher.calls().len(),
        2,
        "no extra dispatch after all tasks complete"
    );
}

#[tokio::test]
async fn agent_owned_tasklist_validate_and_complete_passes_owner_to_dispatcher() {
    // validate_and_complete for an agent-owned tasklist must pass
    // TasklistOwner::Agent to the dispatcher, not a Team variant.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));

    // Use a RecordingDispatcher that also captures the owner variant.
    struct OwnerCapture {
        calls: Mutex<Vec<(TasklistOwner, TaskId)>>,
    }
    #[async_trait]
    impl TaskDispatcher for OwnerCapture {
        async fn dispatch_task(
            &self,
            _owner_agent_id: &AgentId,
            _prompt: String,
            owner: &TasklistOwner,
            _tasklist_id: &TasklistId,
            task_id: &TaskId,
        ) -> Result<(), AoError> {
            self.calls
                .lock()
                .unwrap()
                .push((owner.clone(), task_id.clone()));
            Ok(())
        }
    }
    let dispatcher = Arc::new(OwnerCapture {
        calls: Mutex::new(Vec::new()),
    });
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_owner = TasklistOwner::Agent {
        agent_id: "solo".to_string(),
    };

    let tl = agent_tasklist(
        &data_root,
        "solo",
        "tl-agent-validate",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task_with_outputs("t1", "solo", "g1", vec![])],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    // validate_and_complete with no expected outputs → completes immediately.
    feeder
        .validate_and_complete(
            &agent_owner,
            &"tl-agent-validate".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // The initial dispatch (from advance) must have used the Agent owner.
    let calls = dispatcher.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "one dispatch from advance");
    assert!(
        matches!(&calls[0].0, TasklistOwner::Agent { agent_id } if agent_id == "solo"),
        "dispatcher must receive Agent owner, got {:?}",
        calls[0].0,
    );
    assert_eq!(calls[0].1, "t1");

    // Tasklist auto-completed (only task, no expected outputs, immediate).
    let final_tl = store
        .get_for_agent("solo", "tl-agent-validate")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_tl.status, TasklistStatus::Completed);
}

// Sleep-event emission is exercised in `tasklist_lifecycle::tests` against
// `maybe_emit_sleep` directly. There is no synchronous sleep wiring in the
// feeder because `advance()` stamps `last_active_at = now()` on auto-
// complete, which always defeats the grace window. The deferred sleep
// detection lives in the mailbox poller (which calls `maybe_emit_sleep` on
// each tick).

// ---- completion-summary followup ----------------------------------------

struct RecordingNotificationDispatcher {
    messages: Mutex<Vec<(String, String)>>,
}

impl RecordingNotificationDispatcher {
    fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
        }
    }

    fn messages(&self) -> Vec<(String, String)> {
        self.messages.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotificationDispatcher for RecordingNotificationDispatcher {
    async fn submit_to_agent(
        &self,
        agent_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError> {
        self.messages
            .lock()
            .unwrap()
            .push((agent_id.to_string(), message.content));
        Ok(())
    }
}

// ---- TodoListComplete replaces per-task post_completion_summary ------------

/// Drain all TodoListComplete payloads from the broadcast receiver.
async fn collect_todo_list_complete(
    rx: &mut broadcast::Receiver<AgentEvent>,
) -> Vec<AgentEventPayload> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => {
                if matches!(event.payload, AgentEventPayload::TodoListComplete { .. }) {
                    out.push(event.payload);
                }
            }
            Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn sync_one_terminal_event_only() {
    // Agent-owned 5-item *sync* tasklist (TodoCreate awaiting inline): a
    // terminal watcher is registered up front, so the agent receives the
    // TerminalReport via the tool call's return value. Under that path we
    // must NOT also queue a post_completion_summary — it would land as a
    // duplicate turn message. Exactly one TodoListComplete UI event fires.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);

    let owner = TasklistOwner::Agent {
        agent_id: "agent-sync".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-sync",
        "tl-sync-5",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "agent-sync", "g1"),
                agent_task("t2", "agent-sync", "g1"),
                agent_task("t3", "agent-sync", "g1"),
                agent_task("t4", "agent-sync", "g1"),
                agent_task("t5", "agent-sync", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    // Register the watcher BEFORE driving any tasks terminal so the
    // sync-mode contract holds: TodoCreate-style callers always install
    // the watcher before the first dispatch.
    let _watcher = feeder.register_terminal_watcher("tl-sync-5");
    feeder.advance(&tl).await.unwrap();

    for task_id in &["t1", "t2", "t3", "t4", "t5"] {
        store
            .set_task_status_by_owner(&owner, "tl-sync-5", task_id, TaskStatus::Completed)
            .await
            .unwrap();
        feeder
            .on_task_terminal(&owner, &"tl-sync-5".to_string(), &task_id.to_string())
            .await
            .unwrap();
    }

    // Sync waiter caught the terminal → no queue message; the agent
    // already gets the TerminalReport from the awaiting tool call.
    assert!(
        notifier.messages().is_empty(),
        "post_completion_summary suppressed when a sync watcher caught the terminal",
    );

    // Exactly one TodoListComplete on the event bus.
    let complete_events = collect_todo_list_complete(&mut rx).await;
    assert_eq!(
        complete_events.len(),
        1,
        "exactly one TodoListComplete event"
    );
    if let AgentEventPayload::TodoListComplete {
        tasklist_id,
        status,
        counts,
        tasks,
    } = &complete_events[0]
    {
        assert_eq!(tasklist_id, "tl-sync-5");
        assert_eq!(status, "completed");
        assert_eq!(counts.succeeded, 5);
        assert_eq!(counts.failed, 0);
        assert_eq!(tasks.len(), 5);
    } else {
        panic!("expected TodoListComplete payload");
    }
}

#[tokio::test]
async fn agent_terminal_persists_todo_list_complete_marker() {
    // Regression for the vanishing completion pill: the TodoListComplete
    // bus event only reaches clients connected at the moment of completion.
    // The terminal flow must ALSO write a `todo_list_complete` entry to the
    // agent's transcript so the marker survives a navigate-away/back and
    // keeps sitting just before the agent's follow-up reply on reload.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);

    let owner = TasklistOwner::Agent {
        agent_id: "agent-persist".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-persist",
        "tl-persist",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "agent-persist", "g1"),
                agent_task("t2", "agent-persist", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    for task_id in &["t1", "t2"] {
        store
            .set_task_status_by_owner(&owner, "tl-persist", task_id, TaskStatus::Completed)
            .await
            .unwrap();
        feeder
            .on_task_terminal(&owner, &"tl-persist".to_string(), &task_id.to_string())
            .await
            .unwrap();
    }

    let transcripts = ao_persistence::transcript::TranscriptStore::new(data_root.clone());
    let entries = transcripts.read_recent("agent-persist", 50).await.unwrap();
    let marker = entries
        .iter()
        .find(|e| e.event_type == "todo_list_complete")
        .expect("a todo_list_complete marker is persisted to the agent transcript");
    assert!(
        marker.content.contains("Todo list completed"),
        "marker should describe completion, got: {}",
        marker.content
    );
    assert!(
        marker.content.contains("2 done"),
        "marker should report the success count, got: {}",
        marker.content
    );
    assert!(
        !marker.hidden_from_user,
        "the marker must stay user-visible so the reply isn't left unexplained",
    );
}

/// Regression for the thread-scoping gap: a tasklist created from a
/// non-default thread must have its `todo_list_complete` completion
/// marker land in THAT thread's own transcript file, not the agent's
/// legacy agent-keyed transcript. Also asserts the live `TodoListComplete`
/// SSE event carries the thread's raw id as its thread tag.
#[tokio::test]
async fn agent_terminal_thread_scoped_tasklist_persists_marker_to_thread_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let threads =
        Arc::new(ao_persistence::thread_store::ThreadStore::load(data_root.clone()).await.unwrap());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);
    feeder.set_thread_store(Arc::clone(&threads));

    let agent_id = "agent-thread-scoped";
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    // A Fresh thread owns its own transcript file, distinct from the
    // agent's legacy `{agent_id}.jsonl`.
    let fresh_row = threads.build_fresh_thread(agent_id, None);
    let thread = threads.create(fresh_row).await.unwrap();

    let mut tl = agent_tasklist(
        &data_root,
        agent_id,
        "tl-thread-scoped",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", agent_id, "g1")],
        )],
    );
    tl.thread_id = Some(thread.id.clone());
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    store
        .set_task_status_by_owner(&owner, "tl-thread-scoped", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-thread-scoped".to_string(), &"t1".to_string())
        .await
        .unwrap();

    // The live SSE event must carry the thread's raw id as its thread tag
    // (unfiltered — the frontend collapses default-kind ids on its own).
    let mut saw_complete = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                if matches!(event.payload, AgentEventPayload::TodoListComplete { .. }) {
                    assert_eq!(
                        event.thread_id.as_deref(),
                        Some(thread.id.as_str()),
                        "TodoListComplete event must be tagged with the tasklist's thread_id",
                    );
                    saw_complete = true;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_complete, "expected a TodoListComplete event on the bus");

    let transcripts = ao_persistence::transcript::TranscriptStore::new(data_root.clone());

    // The marker must land in the thread's own transcript file...
    let thread_path = std::path::PathBuf::from(&thread.transcript_path);
    let thread_entries = transcripts.read_recent_at(&thread_path, 50).await.unwrap();
    assert!(
        thread_entries.iter().any(|e| e.event_type == "todo_list_complete"),
        "expected a todo_list_complete marker in the thread's own transcript file",
    );

    // ...and NOT in the agent's legacy agent-keyed transcript (no double-write,
    // no silent fallback to the wrong file).
    let legacy_entries = transcripts.read_recent(agent_id, 50).await.unwrap();
    assert!(
        !legacy_entries.iter().any(|e| e.event_type == "todo_list_complete"),
        "todo_list_complete marker must NOT be persisted to the agent's legacy transcript \
             when the tasklist was created on a non-default thread",
    );
}

/// Companion to the thread-scoped regression above: when a tasklist's
/// `thread_id` resolves to the agent's `Default`-kind thread (which
/// aliases the legacy transcript file in place), the completion marker
/// must still land at the legacy agent-keyed path — even with a real
/// `ThreadStore` wired in. This guards against `resolve_non_default`
/// treating every `Some(thread_id)` as "route away from the legacy path",
/// which would break every single-thread agent.
#[tokio::test]
async fn agent_terminal_default_thread_tasklist_still_uses_legacy_transcript_path() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let threads =
        Arc::new(ao_persistence::thread_store::ThreadStore::load(data_root.clone()).await.unwrap());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);
    feeder.set_thread_store(Arc::clone(&threads));

    let agent_id = "agent-default-thread";
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    let default_thread = threads.ensure_default_thread(agent_id).await.unwrap();

    let mut tl = agent_tasklist(
        &data_root,
        agent_id,
        "tl-default-thread",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", agent_id, "g1")],
        )],
    );
    tl.thread_id = Some(default_thread.id.clone());
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    store
        .set_task_status_by_owner(&owner, "tl-default-thread", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-default-thread".to_string(), &"t1".to_string())
        .await
        .unwrap();

    let transcripts = ao_persistence::transcript::TranscriptStore::new(data_root.clone());
    let legacy_entries = transcripts.read_recent(agent_id, 50).await.unwrap();
    assert!(
        legacy_entries.iter().any(|e| e.event_type == "todo_list_complete"),
        "a Default-kind thread_id must still resolve to the legacy agent-keyed transcript path",
    );
}

#[tokio::test]
async fn async_one_terminal_event_only() {
    // Agent-owned async tasklist (TodoCreate fire-and-forget): no terminal
    // watcher is ever registered, so on terminal we MUST queue exactly one
    // post_completion_summary into the agent's mailbox — that's the wake
    // signal the agent uses to follow up. Exactly one TodoListComplete
    // UI event fires alongside.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);

    let owner = TasklistOwner::Agent {
        agent_id: "agent-async".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-async",
        "tl-async-5",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "agent-async", "g1"),
                agent_task("t2", "agent-async", "g1"),
                agent_task("t3", "agent-async", "g1"),
                agent_task("t4", "agent-async", "g1"),
                agent_task("t5", "agent-async", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    for task_id in &["t1", "t2", "t3", "t4", "t5"] {
        store
            .set_task_status_by_owner(&owner, "tl-async-5", task_id, TaskStatus::Completed)
            .await
            .unwrap();
        feeder
            .on_task_terminal(&owner, &"tl-async-5".to_string(), &task_id.to_string())
            .await
            .unwrap();
    }

    let messages = notifier.messages();
    assert_eq!(
        messages.len(),
        1,
        "exactly one post_completion_summary queued for async agent scope",
    );
    let (target_agent, content) = &messages[0];
    assert_eq!(
        target_agent, "agent-async",
        "queued to the owning agent's own mailbox"
    );
    assert!(
        content.contains("5 succeeded") && content.contains("0 failed"),
        "summary reflects terminal counts: {content}",
    );

    let complete_events = collect_todo_list_complete(&mut rx).await;
    assert_eq!(
        complete_events.len(),
        1,
        "exactly one TodoListComplete for async mode"
    );
    if let AgentEventPayload::TodoListComplete { status, counts, .. } = &complete_events[0] {
        assert_eq!(status, "completed");
        assert_eq!(counts.succeeded, 5);
    } else {
        panic!("expected TodoListComplete payload");
    }
}

#[tokio::test]
async fn todo_list_complete_routes_to_project_surface_only_when_project_id_is_some() {
    // A project-owned tasklist's completion pill belongs to the project
    // chat, not the coordinator agent's own chat. TodoListComplete must be
    // emitted ONLY on `project:{pid}` (never the agent channel), and the
    // persisted transcript marker must land in the `project_{pid}`
    // transcript (never the agent's) so it survives a reload in the right
    // place without leaking into the agent's main chat.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));

    let owner = TasklistOwner::Agent {
        agent_id: "agent-proj".to_string(),
    };
    let tl = agent_tasklist_with_project(
        &data_root,
        "agent-proj",
        "tl-proj-1",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", "agent-proj", "g1")],
        )],
        Some("proj-xyz".to_string()),
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    store
        .set_task_status_by_owner(&owner, "tl-proj-1", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-proj-1".to_string(), &"t1".to_string())
        .await
        .unwrap();

    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(e) if matches!(e.payload, AgentEventPayload::TodoListComplete { .. }) => {
                events.push(e);
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert_eq!(
        events.len(),
        1,
        "project-owned tasklist emits on the project channel only"
    );
    assert_eq!(
        events[0].agent_id, "project:proj-xyz",
        "must emit on the project channel, not the agent channel"
    );

    // The persisted marker lands in the project transcript only.
    let transcripts = ao_persistence::transcript::TranscriptStore::new(data_root.clone());
    let project_entries = transcripts
        .read_recent("project_proj-xyz", 50)
        .await
        .unwrap();
    assert!(
        project_entries
            .iter()
            .any(|e| e.event_type == "todo_list_complete"),
        "completion marker persisted to the project transcript"
    );
    let agent_entries = transcripts.read_recent("agent-proj", 50).await.unwrap();
    assert!(
        !agent_entries
            .iter()
            .any(|e| e.event_type == "todo_list_complete"),
        "completion marker must NOT leak into the agent transcript"
    );
}

#[tokio::test]
async fn todo_list_complete_emits_only_agent_channel_when_no_project_id() {
    // Without a project_id, TodoListComplete is emitted only on the agent channel.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));

    let owner = TasklistOwner::Agent {
        agent_id: "agent-solo".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-solo",
        "tl-solo-1",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", "agent-solo", "g1")],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    store
        .set_task_status_by_owner(&owner, "tl-solo-1", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-solo-1".to_string(), &"t1".to_string())
        .await
        .unwrap();

    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(e) if matches!(e.payload, AgentEventPayload::TodoListComplete { .. }) => {
                events.push(e);
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert_eq!(events.len(), 1, "must emit only on agent channel when no project_id");
    assert_eq!(events[0].agent_id, "agent-solo");
}

#[tokio::test]
async fn mid_flight_append_delays_flush() {
    // After item 1 terminal, append a 4th item; TodoListComplete must NOT fire
    // until the appended item also reaches terminal.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));

    let owner = TasklistOwner::Agent {
        agent_id: "agent-append".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-append",
        "tl-append",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "agent-append", "g1"),
                agent_task("t2", "agent-append", "g1"),
                agent_task("t3", "agent-append", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    // t1 completes.
    store
        .set_task_status_by_owner(&owner, "tl-append", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-append".to_string(), &"t1".to_string())
        .await
        .unwrap();

    // Append t4 to the store BEFORE t2/t3 complete (simulates TodoUpdate mid-run append).
    store
        .mutate_for_agent("agent-append", "tl-append", |tl| {
            if let Some(g) = tl.groups.iter_mut().find(|g| g.id == "g1") {
                g.tasks.push(agent_task("t4", "agent-append", "g1"));
            }
            Ok(())
        })
        .await
        .unwrap();

    // t2 and t3 complete — t4 still pending, so no TodoListComplete yet.
    store
        .set_task_status_by_owner(&owner, "tl-append", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-append".to_string(), &"t2".to_string())
        .await
        .unwrap();
    store
        .set_task_status_by_owner(&owner, "tl-append", "t3", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-append".to_string(), &"t3".to_string())
        .await
        .unwrap();

    let complete_before = collect_todo_list_complete(&mut rx).await;
    assert!(
        complete_before.is_empty(),
        "TodoListComplete must not fire while t4 is still pending"
    );

    // t4 completes — now all 4 tasks terminal → TodoListComplete fires.
    store
        .set_task_status_by_owner(&owner, "tl-append", "t4", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-append".to_string(), &"t4".to_string())
        .await
        .unwrap();

    let complete_after = collect_todo_list_complete(&mut rx).await;
    assert_eq!(
        complete_after.len(),
        1,
        "TodoListComplete fires once all 4 tasks are terminal"
    );
    if let AgentEventPayload::TodoListComplete { counts, tasks, .. } = &complete_after[0] {
        assert_eq!(counts.succeeded, 4);
        assert_eq!(tasks.len(), 4);
    } else {
        panic!("expected TodoListComplete");
    }
}

#[tokio::test]
async fn single_failure_does_not_block_flush() {
    // 5 items, item 3 fails; TodoListComplete fires with counts.failed == 1.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);

    let owner = TasklistOwner::Agent {
        agent_id: "agent-fail".to_string(),
    };
    let tl = agent_tasklist(
        &data_root,
        "agent-fail",
        "tl-fail-5",
        vec![
            group(
                "g1",
                TaskGroupMode::Seq,
                vec![
                    agent_task("t1", "agent-fail", "g1"),
                    agent_task("t2", "agent-fail", "g1"),
                    agent_task("t3", "agent-fail", "g1"),
                ],
            ),
            group(
                "g2",
                TaskGroupMode::Seq,
                vec![
                    agent_task("t4", "agent-fail", "g2"),
                    agent_task("t5", "agent-fail", "g2"),
                ],
            ),
        ],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    // t1 and t2 succeed, t3 fails → tasklist transitions to Failed immediately.
    store
        .set_task_status_by_owner(&owner, "tl-fail-5", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-fail-5".to_string(), &"t1".to_string())
        .await
        .unwrap();
    store
        .set_task_status_by_owner(&owner, "tl-fail-5", "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-fail-5".to_string(), &"t2".to_string())
        .await
        .unwrap();
    store
        .set_task_status_by_owner(&owner, "tl-fail-5", "t3", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &"tl-fail-5".to_string(), &"t3".to_string())
        .await
        .unwrap();

    // Async-mode agent failure: exactly one post_completion_summary
    // queued so the agent wakes up and can decide whether to retry,
    // escalate to the user, or surface the error in its next turn.
    let messages = notifier.messages();
    assert_eq!(
        messages.len(),
        1,
        "exactly one post_completion_summary queued on async agent failure",
    );
    let (target_agent, content) = &messages[0];
    assert_eq!(target_agent, "agent-fail");
    assert!(
        content.contains("2 succeeded") && content.contains("1 failed"),
        "summary surfaces the failure counts: {content}",
    );

    // Exactly one TodoListComplete fires (the failed transition triggers it).
    let complete_events = collect_todo_list_complete(&mut rx).await;
    assert_eq!(
        complete_events.len(),
        1,
        "TodoListComplete fires on failure without blocking"
    );
    if let AgentEventPayload::TodoListComplete {
        status,
        counts,
        tasks,
        ..
    } = &complete_events[0]
    {
        assert_eq!(status, "failed");
        assert_eq!(counts.succeeded, 2, "2 tasks succeeded");
        assert_eq!(counts.failed, 1, "1 task failed");
        let t3_entry = tasks
            .iter()
            .find(|t| t.task_id == "t3")
            .expect("t3 in tasks");
        assert_eq!(t3_entry.status, "failed");
    } else {
        panic!("expected TodoListComplete payload");
    }
}

#[tokio::test]
async fn team_scope_per_task_unchanged() {
    // Team-owned tasklist: no TodoListComplete, no post_completion_summary.
    // Regression gate — team mode is unaffected by the agent-scope suppression.
    let (_tmp, store, _dispatcher, feeder) = setup().await;
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = feeder.with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);

    let tl = tasklist(
        "team-z",
        "tl-team-z2",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-z".to_string(), &"tl-team-z2".to_string())
        .await
        .unwrap();

    store
        .set_task_status("team-z", "tl-team-z2", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(
            &TasklistOwner::Team {
                team_id: "team-z".to_string(),
            },
            &"tl-team-z2".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // Team scope must NOT emit TodoListComplete.
    let complete_events = collect_todo_list_complete(&mut rx).await;
    assert!(
        complete_events.is_empty(),
        "no TodoListComplete for team-owned tasklist"
    );

    // Team scope also has no post_completion_summary (existing behaviour unchanged).
    assert!(
        notifier.messages().is_empty(),
        "no post_completion_summary for team-owned tasklists",
    );
}

// ---- progress.jsonl written on task terminal ----------------------------

#[tokio::test]
async fn progress_log_written_on_seq_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_id = "agent-prog";
    let tl_id = "tl-progress";
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
                agent_task("t3", agent_id, "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();

    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    feeder.advance(&tl).await.unwrap();
    assert_eq!(dispatcher.calls().len(), 1, "t1 dispatched");

    // t1 completes
    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 2, "t2 dispatched");

    // t2 fails
    store
        .set_task_status_by_owner(&owner, tl_id, "t2", TaskStatus::Failed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t2".to_string())
        .await
        .unwrap();
    // tasklist now Failed — no further dispatch

    let progress_path = data_root.agent_tasklist_progress_log(agent_id, tl_id);
    let contents = tokio::fs::read_to_string(&progress_path)
        .await
        .expect("progress.jsonl must exist");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "two terminal tasks → two blocks");

    let b0: ao_persistence::progress_log::ProgressBlock =
        serde_json::from_str(lines[0]).unwrap();
    let b1: ao_persistence::progress_log::ProgressBlock =
        serde_json::from_str(lines[1]).unwrap();

    assert_eq!(b0.task_id.as_deref(), Some("t1"));
    assert_eq!(b0.status, "completed");
    assert!(b0.output_path.is_some(), "output_path populated");
    assert!(b0.ended_at.is_some(), "ended_at populated");
    assert_eq!(b0.attempt_count, Some(0));

    assert_eq!(b1.task_id.as_deref(), Some("t2"));
    assert_eq!(b1.status, "failed");
    assert!(b1.output_path.is_some());
}

#[tokio::test]
async fn progress_log_write_error_is_swallowed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_id = "agent-err";
    let tl_id = "tl-err";
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", agent_id, "g1")],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();

    // Make progress.jsonl a directory so any write to it fails.
    let progress_path = data_root.agent_tasklist_progress_log(agent_id, tl_id);
    tokio::fs::create_dir_all(&progress_path).await.unwrap();

    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    feeder.advance(&tl).await.unwrap();
    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();

    // Must not propagate the write error.
    let result = feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await;
    assert!(
        result.is_ok(),
        "write error must be swallowed, got: {:?}",
        result
    );
}

// ---- per-task meta.json written at dispatch and terminal ------------------

#[tokio::test]
async fn meta_json_written_for_each_task_in_3_item_seq() {
    use ao_persistence::task_meta::TaskMeta;
    use ao_protocol::tasklist::TaskStatus;

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_id = "agent-meta3";
    let tl_id = "tl-meta3";
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
                agent_task("t3", agent_id, "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();

    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };

    // -- t1: dispatch --
    feeder.advance(&tl).await.unwrap();
    assert_eq!(dispatcher.calls().len(), 1, "t1 dispatched");

    let meta1_path = data_root.task_meta_path(agent_id, tl_id, "t1");
    assert!(
        meta1_path.exists(),
        "meta.json must exist after t1 dispatch"
    );
    let meta1: TaskMeta =
        serde_json::from_slice(&tokio::fs::read(&meta1_path).await.unwrap()).unwrap();
    assert_eq!(
        meta1.status,
        TaskStatus::InProgress,
        "t1 meta status after dispatch"
    );
    assert!(meta1.started_at.is_some(), "started_at set at dispatch");

    // -- t1: terminal --
    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await
        .unwrap();
    let meta1_term: TaskMeta =
        serde_json::from_slice(&tokio::fs::read(&meta1_path).await.unwrap()).unwrap();
    assert_eq!(
        meta1_term.status,
        TaskStatus::Completed,
        "t1 meta status after terminal"
    );
    assert!(meta1_term.ended_at.is_some(), "ended_at set at terminal");

    // -- t2: dispatch --
    assert_eq!(dispatcher.calls().len(), 2, "t2 dispatched");
    let meta2_path = data_root.task_meta_path(agent_id, tl_id, "t2");
    assert!(
        meta2_path.exists(),
        "meta.json must exist after t2 dispatch"
    );
    let meta2: TaskMeta =
        serde_json::from_slice(&tokio::fs::read(&meta2_path).await.unwrap()).unwrap();
    assert_eq!(
        meta2.status,
        TaskStatus::InProgress,
        "t2 meta status after dispatch"
    );

    // -- t2: terminal --
    store
        .set_task_status_by_owner(&owner, tl_id, "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t2".to_string())
        .await
        .unwrap();
    let meta2_term: TaskMeta =
        serde_json::from_slice(&tokio::fs::read(&meta2_path).await.unwrap()).unwrap();
    assert_eq!(
        meta2_term.status,
        TaskStatus::Completed,
        "t2 meta status after terminal"
    );

    // -- t3: dispatch --
    assert_eq!(dispatcher.calls().len(), 3, "t3 dispatched");
    let meta3_path = data_root.task_meta_path(agent_id, tl_id, "t3");
    assert!(
        meta3_path.exists(),
        "meta.json must exist after t3 dispatch"
    );

    // -- t3: terminal --
    store
        .set_task_status_by_owner(&owner, tl_id, "t3", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t3".to_string())
        .await
        .unwrap();
    let meta3_term: TaskMeta =
        serde_json::from_slice(&tokio::fs::read(&meta3_path).await.unwrap()).unwrap();
    assert_eq!(
        meta3_term.status,
        TaskStatus::Completed,
        "t3 meta status after terminal"
    );

    // Loop J's progress.jsonl coexists with the new tasks/ subdir.
    let progress_path = data_root.agent_tasklist_progress_log(agent_id, tl_id);
    assert!(
        progress_path.exists(),
        "the legacy progress.jsonl must still exist"
    );
    let progress_text = tokio::fs::read_to_string(&progress_path).await.unwrap();
    assert_eq!(
        progress_text.lines().count(),
        3,
        "three progress entries for three tasks"
    );

    // The output_path in each progress entry uses the legacy path, not the new tasks/ meta path.
    let expected_output_path = data_root.agent_tasklist_task_output_path(agent_id, tl_id, "t1");
    assert!(
        progress_text.contains(&expected_output_path.to_string_lossy().to_string()),
        "progress.jsonl must reference the legacy output_path, not the tasks/ meta subdir",
    );
}

// ---- assignment-based dispatcher routing ------------------------------------

#[tokio::test]
async fn dispatcher_uses_assignment_when_present() {
    // task with assignment: Some({owner: "backend", mode: Classified}) dispatches
    // to the backend agent (not the parent/default agent).
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    // Create a backend agent profile so the safety-net existence check passes.
    let agents_dir = data_root.agents_dir();
    tokio::fs::create_dir_all(&agents_dir).await.unwrap();
    tokio::fs::write(agents_dir.join("backend.yaml"), b"id: backend\n")
        .await
        .unwrap();

    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let mut t1 = task("t1", "", "g1");
    t1.assignment = Some(TaskAssignment {
        owner_agent_id: "backend".to_string(),
        mode: AssignmentMode::Classified,
    });

    let tl = agent_tasklist(
        &data_root,
        "parent-agent",
        "tl-classified",
        vec![group("g1", TaskGroupMode::Seq, vec![t1])],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1, "one dispatch");
    assert_eq!(
        calls[0].0, "backend",
        "dispatched to classified executor, not parent"
    );
    assert_eq!(calls[0].1, "t1");
}

#[tokio::test]
async fn dispatcher_defers_on_null_assignment() {
    // task with assignment: None (awaiting classification) must NOT be dispatched;
    // a TaskDeferred event with reason "awaiting_classification" must be observed.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let mut rx = bus.subscribe();
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));

    // task with assignment: None (the pre-classification state TodoCreate writes)
    let mut t1 = task("t1", "", "g1");
    t1.assignment = None;

    let tl = agent_tasklist(
        &data_root,
        "parent-agent",
        "tl-deferred",
        vec![group("g1", TaskGroupMode::Seq, vec![t1])],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    // No dispatch fired.
    assert!(
        dispatcher.calls().is_empty(),
        "no dispatch on null assignment"
    );

    // A TaskDeferred event with reason "awaiting_classification" must be emitted.
    let mut found_deferred = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                if let AgentEventPayload::TaskDeferred {
                    task_id, reason, ..
                } = event.payload
                {
                    if task_id == "t1" && reason == "awaiting_classification" {
                        found_deferred = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        found_deferred,
        "TaskDeferred event with awaiting_classification reason must be emitted"
    );
}

#[tokio::test]
async fn dispatcher_falls_back_to_parent_on_pinned_to_self() {
    // task with assignment: Some({owner: parent_id, mode: Pinned}) dispatches
    // on the parent agent — happy path for self-assigned tasks.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let tl = agent_tasklist(
        &data_root,
        "parent-agent",
        "tl-self-pinned",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", "parent-agent", "g1")],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0, "parent-agent",
        "self-pinned task dispatches to parent"
    );
    assert_eq!(calls[0].1, "t1");
}

#[tokio::test]
async fn resolve_executor_agent_id_returns_new_owner_after_todo_update_reassign() {
    // Regression test for the TodoUpdate write-path fix: reassigning
    // `owner` on a Pending Classified task in an Agent-owned tasklist
    // must reroute dispatch to the new owner. This drives the same
    // storage mutation `TasklistService::set_assignment` performs (a CAS
    // write of `assignment = Some({owner, Pinned})` that bumps
    // `classifier_token`, mirroring exactly what TodoUpdate's owner-pin
    // now does) and then asserts both (a) `resolve_executor_agent_id`
    // resolves to the new owner and (b) the full feeder dispatch path
    // agrees — with no prompt change involved anywhere.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    // dispatch_one's existence safety-net (fails the task rather than
    // dispatching to a ghost agent) requires a profile on disk for any
    // executor that differs from the tasklist's parent agent.
    let agents_dir = data_root.agents_dir();
    tokio::fs::create_dir_all(&agents_dir).await.unwrap();
    tokio::fs::write(agents_dir.join("old-owner.yaml"), b"id: old-owner\n")
        .await
        .unwrap();
    tokio::fs::write(agents_dir.join("new-owner.yaml"), b"id: new-owner\n")
        .await
        .unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));

    // Pre-update state: a Pending task auto-routed by the classifier to
    // "old-owner" (the shape TodoAdd/the classifier produce day-to-day).
    let mut t1 = task("t1", "old-owner", "g1");
    t1.assignment = Some(TaskAssignment {
        owner_agent_id: "old-owner".to_string(),
        mode: AssignmentMode::Classified,
    });

    let tl = agent_tasklist(
        &data_root,
        "parent-agent",
        "tl-reassign",
        vec![group("g1", TaskGroupMode::Seq, vec![t1])],
    );
    store.create_for_agent(&tl).await.unwrap();

    let pre = store
        .get_for_agent("parent-agent", "tl-reassign")
        .await
        .unwrap()
        .unwrap();
    let pre_task = pre.groups[0].tasks.iter().find(|t| t.id == "t1").unwrap();
    assert_eq!(
        resolve_executor_agent_id(&pre.owner, pre_task),
        "old-owner",
        "sanity: pre-update dispatch resolves to the classified owner"
    );

    // Simulate `TodoUpdate { task_id: "t1", owner: "new-owner" }`: pin a
    // fresh assignment via the same CAS shape as
    // `TasklistService::set_assignment`, touching only `assignment` and
    // `classifier_token` — the prompt is never written.
    let updated = store
        .mutate_for_agent("parent-agent", "tl-reassign", |tl| {
            let task = tl
                .groups
                .iter_mut()
                .flat_map(|g| g.tasks.iter_mut())
                .find(|t| t.id == "t1")
                .expect("task t1 must exist");
            assert_eq!(task.classifier_token, 0, "CAS precondition: original token");
            task.assignment = Some(TaskAssignment {
                owner_agent_id: "new-owner".to_string(),
                mode: AssignmentMode::Pinned,
            });
            task.classifier_token += 1;
            Ok(())
        })
        .await
        .unwrap();

    let post_task = updated.groups[0].tasks.iter().find(|t| t.id == "t1").unwrap();
    assert_eq!(
        post_task.prompt, "prompt for t1",
        "owner reassignment must not require or imply a prompt change"
    );
    assert_eq!(
        resolve_executor_agent_id(&updated.owner, post_task),
        "new-owner",
        "resolve_executor_agent_id must resolve to the newly pinned owner"
    );

    // Full round-trip: the feeder's real dispatch path must agree.
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());
    feeder.advance(&updated).await.unwrap();

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1, "reassigned task must dispatch exactly once");
    assert_eq!(
        calls[0].0, "new-owner",
        "feeder must dispatch to the new pinned owner, not the stale classified owner"
    );
    assert_eq!(calls[0].1, "t1");
    assert_eq!(
        calls[0].2, "prompt for t1",
        "dispatched prompt must be unchanged from the original"
    );
}

#[tokio::test]
async fn team_scope_routing_unchanged() {
    // Team-owned tasklists still dispatch via task.owner_agent_id (no assignment
    // lookup). This is the regression gate for the team scope path.
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-a",
        "tl-team-unchanged",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            // task() uses owner_agent_id = "worker", assignment = None
            vec![task("t1", "worker", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-a".to_string(), &"tl-team-unchanged".to_string())
        .await
        .unwrap();

    let calls = dispatcher.calls();
    assert_eq!(
        calls.len(),
        1,
        "team-owned task dispatches via owner_agent_id"
    );
    assert_eq!(calls[0].0, "worker");
    assert_eq!(calls[0].1, "t1");
}

/// Team-owned tasklists have no auto-routing classifier in this build
/// (the per-team coordinator channel was retired). An unowned task must
/// stay Pending — never silently dispatched, and never mistaken for the
/// agent-owned "channel not wired" case — and `dispatch_group` must not
/// panic or hang trying to find a routing channel that no longer exists.
#[tokio::test]
async fn team_scope_unowned_task_has_no_routing_and_stays_pending() {
    let (_tmp, store, dispatcher, feeder) = setup().await;

    let tl = tasklist(
        "team-b",
        "tl-team-unowned",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            // owner_agent_id == "" — unowned, would previously have been
            // submitted to the (now-deleted) team routing queue.
            vec![task("t1", "", "g1")],
        )],
    );
    store.create(&tl).await.unwrap();
    feeder
        .start(&"team-b".to_string(), &"tl-team-unowned".to_string())
        .await
        .unwrap();

    assert!(
        dispatcher.calls().is_empty(),
        "unowned team-owned task must not be dispatched"
    );
    let updated = store.get("team-b", "tl-team-unowned").await.unwrap().unwrap();
    assert_eq!(
        updated.groups[0].tasks[0].status,
        TaskStatus::Pending,
        "unowned team-owned task must remain Pending, not silently advance"
    );
}

/// The terminal report attaches each task's changelog notification summary
/// (and optional details) so a sync TodoCreate caller sees what each
/// subagent concluded, not just task titles. Tasks without a changelog
/// entry keep `summary`/`details` as `None`.
#[tokio::test]
async fn terminal_report_carries_changelog_summaries() {
    use ao_protocol::changelog::ChangelogEntry;
    use ao_protocol::tasklist::TasklistOwner;

    let (_tmp, store, _dispatcher, feeder) = setup().await;
    let agent_id = "agent-1";

    // Agent-owned tasklist with two tasks; only t1 reports a notification.
    let mut tl = tasklist(
        "",
        "tl-summaries",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
            ],
        )],
    );
    tl.owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    tl.team_id = None;

    // A subagent emitted a <task-item-notification> for t1 → changelog.
    let changelog = ChangelogStore::new(store.data_root().clone());
    changelog
        .append(
            &tl.owner,
            &tl.id,
            &ChangelogEntry {
                task_id: "t1".to_string(),
                tasklist_id: tl.id.clone(),
                agent_id: agent_id.to_string(),
                status: "complete".to_string(),
                summary: "Implemented the parser".to_string(),
                details: Some("Recursive descent with error recovery".to_string()),
                ts: Utc::now(),
            },
        )
        .await
        .unwrap();

    // Drive the snapshot to terminal so the report reflects completion.
    tl.status = TasklistStatus::Completed;
    for g in &mut tl.groups {
        for t in &mut g.tasks {
            t.status = TaskStatus::Completed;
        }
    }

    // Register a sync waiter, fire, and await the aggregated report.
    let guard = feeder.register_terminal_watcher(&tl.id);
    let caught = feeder.fire_terminal_watcher(&tl).await;
    assert!(
        caught,
        "a registered watcher must catch the terminal report"
    );
    let report = guard.wait().await.unwrap();

    let t1 = report.tasks.iter().find(|e| e.id == "t1").unwrap();
    assert_eq!(
        t1.summary.as_deref(),
        Some("Implemented the parser"),
        "t1 carries its changelog summary"
    );
    assert_eq!(
        t1.details.as_deref(),
        Some("Recursive descent with error recovery"),
        "t1 carries its changelog details"
    );

    let t2 = report.tasks.iter().find(|e| e.id == "t2").unwrap();
    assert!(
        t2.summary.is_none(),
        "a task with no notification has no summary"
    );
    assert!(
        t2.details.is_none(),
        "a task with no notification has no details"
    );
}

/// `on_task_terminal` carries the changelog summary onto the durable
/// per-task records (progress.jsonl block + meta.json). The summary is
/// written to the changelog *before* the terminal transition (by
/// `cli::record_task_item_changelog`), which is what makes it readable here.
/// A task with no changelog entry leaves both records' summary `None`.
#[tokio::test]
async fn on_task_terminal_writes_changelog_summary_to_progress_and_meta() {
    use ao_protocol::changelog::ChangelogEntry;

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_id = "agent-sum";
    let tl_id = "tl-sum";
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };

    // Dispatch t1 so its meta.json exists with dispatch-time timestamps.
    feeder.advance(&tl).await.unwrap();

    // Simulate the producing agent's notification landing in the changelog
    // ahead of the terminal transition.
    ChangelogStore::new(data_root.clone())
        .append(
            &tl.owner,
            tl_id,
            &ChangelogEntry {
                task_id: "t1".to_string(),
                tasklist_id: tl_id.to_string(),
                agent_id: agent_id.to_string(),
                status: "complete".to_string(),
                summary: "Wrote the parser module".to_string(),
                details: Some("recursive descent".to_string()),
                ts: Utc::now(),
            },
        )
        .await
        .unwrap();

    // t1 reaches terminal.
    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await
        .unwrap();

    // progress.jsonl block for t1 carries the summary.
    let progress_path = data_root.agent_tasklist_progress_log(agent_id, tl_id);
    let contents = tokio::fs::read_to_string(&progress_path)
        .await
        .expect("progress.jsonl must exist");
    let block: ProgressBlock =
        serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert_eq!(block.task_id.as_deref(), Some("t1"));
    assert_eq!(
        block.summary.as_deref(),
        Some("Wrote the parser module"),
        "progress block carries the changelog summary"
    );

    // meta.json for t1 carries the summary too.
    let meta_path = data_root.task_meta_path(agent_id, tl_id, "t1");
    let meta = read_task_meta(&meta_path).await.unwrap().unwrap();
    assert_eq!(
        meta.summary.as_deref(),
        Some("Wrote the parser module"),
        "meta.json carries the changelog summary"
    );

    // t2 has no changelog entry → both records leave summary None.
    store
        .set_task_status_by_owner(&owner, tl_id, "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t2".to_string())
        .await
        .unwrap();

    let contents = tokio::fs::read_to_string(&progress_path).await.unwrap();
    let t2_block: ProgressBlock = contents
        .lines()
        .map(|l| serde_json::from_str::<ProgressBlock>(l).unwrap())
        .find(|b| b.task_id.as_deref() == Some("t2"))
        .expect("t2 progress block");
    assert!(
        t2_block.summary.is_none(),
        "task with no changelog entry has no progress summary"
    );
    let t2_meta = read_task_meta(&data_root.task_meta_path(agent_id, tl_id, "t2"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        t2_meta.summary.is_none(),
        "task with no changelog entry has no meta summary"
    );
}

/// `on_task_terminal` appends a `TaskComment` with the executor's changelog
/// summary so the Task Detail modal's Comments section shows it. A task with
/// no summary must get no comment.
#[tokio::test]
async fn on_task_terminal_appends_summary_as_task_comment() {
    use ao_protocol::changelog::ChangelogEntry;
    use ao_protocol::tasklist::TaskCommentAuthorKind;

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let agent_id = "agent-comment";
    let tl_id = "tl-comment";
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };

    // Dispatch t1 so its meta.json exists.
    feeder.advance(&tl).await.unwrap();

    // Simulate the producing agent's notification in the changelog.
    ChangelogStore::new(data_root.clone())
        .append(
            &tl.owner,
            tl_id,
            &ChangelogEntry {
                task_id: "t1".to_string(),
                tasklist_id: tl_id.to_string(),
                agent_id: agent_id.to_string(),
                status: "complete".to_string(),
                summary: "Implemented the feature".to_string(),
                details: None,
                ts: Utc::now(),
            },
        )
        .await
        .unwrap();

    // t1 reaches terminal.
    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await
        .unwrap();

    // t1 must carry exactly one comment with the changelog summary.
    let t1 = agent_task_snapshot(&store, agent_id, tl_id, "t1").await;
    assert_eq!(t1.comments.len(), 1, "t1 should have exactly one comment");
    let c = &t1.comments[0];
    assert_eq!(
        c.body, "Implemented the feature",
        "comment body matches changelog summary"
    );
    assert!(
        matches!(c.author_kind, TaskCommentAuthorKind::Agent),
        "comment author kind must be Agent"
    );

    // t2 has no changelog entry → no comment.
    store
        .set_task_status_by_owner(&owner, tl_id, "t2", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t2".to_string())
        .await
        .unwrap();

    let t2 = agent_task_snapshot(&store, agent_id, tl_id, "t2").await;
    assert!(
        t2.comments.is_empty(),
        "t2 with no changelog summary must have no comment"
    );
}

// ── FIX-1: reconcile_zombies_on_start tests ──────────────────────────────

#[tokio::test]
async fn reconcile_zombies_on_start_recovers_team_owned_zombie() {
    // "feeder-restart-after-panic": a team-owned task is InProgress on disk
    // with no live runner and no dispatch timestamp (server restart).
    // reconcile_zombies_on_start must recover it.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_secs(300)); // large grace — on-restart path bypasses it

    let mut tl = tasklist(
        "team-a",
        "tl-team-zombie",
        vec![group("g1", TaskGroupMode::Seq, vec![task("t1", "worker", "g1")])],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create(&tl).await.unwrap();

    // No dispatch timestamp (server just restarted) — should recover immediately.
    let recovered = feeder.reconcile_zombies_on_start().await.unwrap();
    assert_eq!(recovered, 1, "should recover 1 zombie");
    // After recovery the task is either reprompted (attempt_count bumped) or Failed.
    let snap = task_snapshot(&store, "tl-team-zombie", "t1").await;
    assert!(
        snap.attempt_count >= 1 || snap.status == TaskStatus::Failed,
        "recovered task must have been processed: {:?}",
        snap
    );
}

#[tokio::test]
async fn reconcile_zombies_on_start_recovers_agent_owned_zombie() {
    // "reconcile-zombie-on-restart" (agent-owned path): agent-owned tasklist
    // with an InProgress task and no dispatch timestamp. The previous watchdog
    // never covered agent-owned lists. Both reconcile_zombies_on_start and
    // watchdog_tick should now recover it.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_secs(300));

    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-agent-zombie",
        vec![group("g1", TaskGroupMode::Seq, vec![agent_task("t1", "worker", "g1")])],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    let recovered = feeder.reconcile_zombies_on_start().await.unwrap();
    assert_eq!(recovered, 1, "agent-owned zombie should be recovered");
    let snap = agent_task_snapshot(&store, "coordinator", "tl-agent-zombie", "t1").await;
    assert!(
        snap.attempt_count >= 1 || snap.status == TaskStatus::Failed,
        "recovered agent-owned task must have been processed: {:?}",
        snap
    );
}

#[tokio::test]
async fn watchdog_tick_now_covers_agent_owned_tasklists() {
    // Regression test for the coverage gap reported in INV-1: previously
    // watchdog_tick only scanned team-owned tasklists, leaving agent-owned
    // zombies invisible. After the fix it must recover both.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0)); // zero grace → immediate detection

    // Agent-owned tasklist with a stale InProgress task and no dispatch ts.
    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-watchdog-agent",
        vec![group("g1", TaskGroupMode::Seq, vec![agent_task("t1", "worker", "g1")])],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(
        recovered, 1,
        "watchdog_tick must recover agent-owned zombie (was invisible before fix)"
    );
}

// ── FIX-2: control tools advance the queue (no silent no-op) ─────────────

#[tokio::test]
async fn force_complete_advances_seq_head_even_when_feeder_stalled() {
    // "complete-advances-next": the head of a SEQ list is InProgress and its
    // background runner has stalled (never reports terminal). The previous
    // complete_task path called on_task_terminal WITHOUT first writing
    // Completed, so the SEQ guard saw in_progress>0 and never dispatched the
    // next task — TodoComplete "succeeded" against a frozen queue.
    // force_complete_and_advance must reliably advance to t2.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone());

    let owner = TasklistOwner::Agent {
        agent_id: "coordinator".to_string(),
    };
    // Self-assigned executor (== the owning agent) so dispatch_one's
    // missing-profile safety net is bypassed without writing agent YAMLs.
    let tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-fix2-complete",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "coordinator", "g1"),
                agent_task("t2", "coordinator", "g1"),
            ],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();

    // Bootstrap: advance dispatches t1 and leaves t2 Pending. We then let
    // the run "stall" — nothing fires on_run_ended, so t1 stays InProgress.
    feeder.advance(&tl).await.unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(dispatcher.calls()[0].1, "t1");
    assert_eq!(
        agent_task_snapshot(&store, "coordinator", "tl-fix2-complete", "t1")
            .await
            .status,
        TaskStatus::InProgress,
    );

    // Force-complete the stalled head via the control-tool path.
    feeder
        .force_complete_and_advance(
            &owner,
            &"tl-fix2-complete".to_string(),
            &"t1".to_string(),
        )
        .await
        .unwrap();

    // t1 must be Completed and t2 must now be dispatched (InProgress).
    assert_eq!(
        agent_task_snapshot(&store, "coordinator", "tl-fix2-complete", "t1")
            .await
            .status,
        TaskStatus::Completed,
        "head must be marked Completed",
    );
    assert_eq!(
        agent_task_snapshot(&store, "coordinator", "tl-fix2-complete", "t2")
            .await
            .status,
        TaskStatus::InProgress,
        "next pending task must advance to InProgress",
    );
    assert_eq!(
        dispatcher.calls().len(),
        2,
        "t2 must have been dispatched after completing t1",
    );
    assert_eq!(dispatcher.calls()[1].1, "t2");
}

#[tokio::test]
async fn start_rekick_respawns_dead_feeder_on_control_action() {
    // "restart-dead-feeder-on-control-action": a SEQ list whose head is a
    // zombie — InProgress on disk but with zero live runs in the
    // InstanceRegistry. A plain advance() would no-op (SEQ guard counts the
    // zombie as in-flight). kick_and_reconcile must detect the dead runner
    // and recover/redispatch it.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0));

    let owner = TasklistOwner::Agent {
        agent_id: "coordinator".to_string(),
    };
    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-fix2-rekick",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "worker", "g1"),
                agent_task("t2", "worker", "g1"),
            ],
        )],
    );
    // Head is a zombie: InProgress with no live runner registered.
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    let recovered = feeder
        .kick_and_reconcile(&owner, &"tl-fix2-rekick".to_string())
        .await
        .unwrap();
    assert_eq!(recovered, 1, "the dead-runner zombie must be recovered");

    // Recovery reprompts the head (attempt_count<max): it stays InProgress
    // and is re-dispatched to its agent — the "respawn" of the dead runner.
    let t1 = agent_task_snapshot(&store, "coordinator", "tl-fix2-rekick", "t1").await;
    assert!(
        t1.attempt_count >= 1 || t1.status == TaskStatus::Failed,
        "recovered head must have been reprompted or failed: {:?}",
        t1
    );
    assert!(
        dispatcher.calls().iter().any(|(_, tid, _)| tid == "t1"),
        "the recovered head must have been re-dispatched",
    );
}

#[tokio::test]
async fn kick_and_reconcile_leaves_live_runner_untouched() {
    // Liveness guard: kick_and_reconcile must NOT tear down a healthy run.
    // An InProgress head with a LIVE entry in the InstanceRegistry is not a
    // zombie — re-kick must recover nothing and dispatch nothing new.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0));

    let owner = TasklistOwner::Agent {
        agent_id: "coordinator".to_string(),
    };
    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-fix2-live",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", "worker", "g1"),
                agent_task("t2", "worker", "g1"),
            ],
        )],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    // Register a live run under the same key the feeder derives.
    instance_registry
        .register_run(&"tasklist:tl-fix2-live:worker".to_string(), "run-live")
        .await;

    let recovered = feeder
        .kick_and_reconcile(&owner, &"tl-fix2-live".to_string())
        .await
        .unwrap();
    assert_eq!(recovered, 0, "a live runner must not be reconciled away");
    assert!(
        dispatcher.calls().is_empty(),
        "no new dispatch while the head is genuinely running",
    );
    assert_eq!(
        agent_task_snapshot(&store, "coordinator", "tl-fix2-live", "t1")
            .await
            .status,
        TaskStatus::InProgress,
        "live head must stay InProgress",
    );
}

// ── FIX-3: liveness probes resolve the executor key, not the empty
//           top-level owner_agent_id of a classifier-assigned task ────────

#[tokio::test]
async fn watchdog_resolves_executor_key_for_classified_task() {
    // Regression for the reported false-failure: a classifier-assigned task
    // carries an EMPTY top-level owner_agent_id and stores the chosen
    // executor only in `assignment.owner_agent_id`. The agent run registers
    // under `tasklist:{id}:{executor}`. Before the fix the watchdog derived
    // its registry key from the empty owner_agent_id (`tasklist:{id}:`),
    // read zero live runs, and reaped a healthy, still-working task to
    // Failed. After the fix the probe resolves the executor key, sees the
    // live run, and leaves it alone.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    // Zero grace: nothing protects the task except a correctly-resolved
    // live-run lookup, so this fails loudly if the key is wrong.
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0));

    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-fix3-watchdog",
        vec![group(
            "g1",
            TaskGroupMode::Par,
            vec![classified_task("t1", "worker", "g1")],
        )],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    // Live run registered under the EXECUTOR key — never the empty owner.
    instance_registry
        .register_run(&"tasklist:tl-fix3-watchdog:worker".to_string(), "run-live")
        .await;

    let recovered = feeder.watchdog_tick().await.unwrap();
    assert_eq!(
        recovered, 0,
        "watchdog must resolve the executor key and see the live run \
             (was reaping the healthy task via the empty owner key before the fix)"
    );
    assert!(
        dispatcher.calls().is_empty(),
        "no re-dispatch while the executor's run is genuinely live",
    );
    assert_eq!(
        agent_task_snapshot(&store, "coordinator", "tl-fix3-watchdog", "t1")
            .await
            .status,
        TaskStatus::InProgress,
        "live classifier-assigned task must stay InProgress, not be failed",
    );
}

#[tokio::test]
async fn kick_and_reconcile_resolves_executor_key_for_classified_task() {
    // Same executor-key resolution, exercised through the control-action
    // re-kick path (kick_and_reconcile) rather than the periodic watchdog.
    // Contrast with `kick_and_reconcile_leaves_live_runner_untouched`, which
    // uses agent_task (owner == executor) and so cannot catch this bug.
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let instance_registry = Arc::new(InstanceRegistry::new());
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_instance_registry(Arc::clone(&instance_registry))
        .with_watchdog_grace(Duration::from_millis(0));

    let owner = TasklistOwner::Agent {
        agent_id: "coordinator".to_string(),
    };
    let mut tl = agent_tasklist(
        &data_root,
        "coordinator",
        "tl-fix3-kick",
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![classified_task("t1", "worker", "g1")],
        )],
    );
    tl.groups[0].tasks[0].status = TaskStatus::InProgress;
    store.create_for_agent(&tl).await.unwrap();

    instance_registry
        .register_run(&"tasklist:tl-fix3-kick:worker".to_string(), "run-live")
        .await;

    let recovered = feeder
        .kick_and_reconcile(&owner, &"tl-fix3-kick".to_string())
        .await
        .unwrap();
    assert_eq!(
        recovered, 0,
        "re-kick must resolve the executor key and leave the live run alone",
    );
    assert!(
        dispatcher.calls().is_empty(),
        "no re-dispatch while the executor's run is genuinely live",
    );
}

// ---- project completion loop routing ------------------------------------

/// Recording project dispatcher: captures (project_id, message_content) pairs.
struct RecordingProjectDispatcher {
    messages: Mutex<Vec<(String, String)>>,
}

impl RecordingProjectDispatcher {
    fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
        }
    }

    fn messages(&self) -> Vec<(String, String)> {
        self.messages.lock().unwrap().clone()
    }
}

#[async_trait]
impl ProjectDispatcher for RecordingProjectDispatcher {
    async fn submit_to_project(
        &self,
        project_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError> {
        self.messages
            .lock()
            .unwrap()
            .push((project_id.to_string(), message.content));
        Ok(())
    }
}

/// Project-tagged async tasklist: completion summary must route to the
/// project dispatcher (not the personal agent queue), and the content must
/// include both the standard tasklist guidance and the project-specific
/// loop-continuation guidance.
#[tokio::test]
async fn project_tagged_tasklist_routes_summary_to_project_channel() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let proj_dispatcher = Arc::new(RecordingProjectDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);
    feeder.set_project_dispatcher(
        Arc::clone(&proj_dispatcher) as Arc<dyn ProjectDispatcher>,
    );

    let agent_id = "agent-proj-loop";
    let project_id = "proj-xyz";
    let tl_id = "tl-proj-tagged";

    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    let tl = agent_tasklist_with_project(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![
                agent_task("t1", agent_id, "g1"),
                agent_task("t2", agent_id, "g1"),
            ],
        )],
        Some(project_id.to_string()),
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    for task_id in &["t1", "t2"] {
        store
            .set_task_status_by_owner(&owner, tl_id, task_id, TaskStatus::Completed)
            .await
            .unwrap();
        feeder
            .on_task_terminal(&owner, &tl_id.to_string(), &task_id.to_string())
            .await
            .unwrap();
    }

    // Personal queue must NOT receive the summary — it goes to the project channel.
    assert!(
        notifier.messages().is_empty(),
        "project-tagged tasklist must not post to personal agent queue",
    );

    // Project dispatcher must receive exactly one message.
    let proj_messages = proj_dispatcher.messages();
    assert_eq!(
        proj_messages.len(),
        1,
        "exactly one completion summary routed to the project channel",
    );
    let (target_project, content) = &proj_messages[0];
    assert_eq!(
        target_project, project_id,
        "summary routed to the correct project id",
    );
    assert!(
        content.contains("2 succeeded"),
        "summary reports succeeded count: {content}",
    );
    assert!(
        content.contains("validate") || content.contains("project goal") || content.contains("gaps"),
        "summary includes project loop guidance: {content}",
    );
}

/// Non-project agent tasklist still routes to personal queue unchanged.
#[tokio::test]
async fn untagged_tasklist_routes_to_personal_queue_when_project_dispatcher_wired() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(TasklistStore::new(data_root.clone()));
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotificationDispatcher::new());
    let proj_dispatcher = Arc::new(RecordingProjectDispatcher::new());
    let bus = Arc::new(EventBus::new(256));
    let feeder = TaskFeeder::new(Arc::clone(&store), dispatcher.clone())
        .with_event_bus(Arc::clone(&bus));
    feeder
        .set_notification_dispatcher(Arc::clone(&notifier) as Arc<dyn NotificationDispatcher>);
    feeder.set_project_dispatcher(
        Arc::clone(&proj_dispatcher) as Arc<dyn ProjectDispatcher>,
    );

    let agent_id = "agent-untagged";
    let tl_id = "tl-untagged";

    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    // project_id: None — not a project-tagged tasklist.
    let tl = agent_tasklist(
        &data_root,
        agent_id,
        tl_id,
        vec![group(
            "g1",
            TaskGroupMode::Seq,
            vec![agent_task("t1", agent_id, "g1")],
        )],
    );
    store.create_for_agent(&tl).await.unwrap();
    feeder.advance(&tl).await.unwrap();

    store
        .set_task_status_by_owner(&owner, tl_id, "t1", TaskStatus::Completed)
        .await
        .unwrap();
    feeder
        .on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string())
        .await
        .unwrap();

    assert_eq!(
        notifier.messages().len(),
        1,
        "untagged tasklist still posts to personal queue",
    );
    assert!(
        proj_dispatcher.messages().is_empty(),
        "project dispatcher not touched for untagged tasklist",
    );
}
