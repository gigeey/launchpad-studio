use super::*;

use ao_persistence::paths::DataRoot;
use ao_persistence::task_meta::TaskMeta;
use ao_protocol::tasklist::TaskStatus;
use chrono::{Duration, Utc};
use tempfile::tempdir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_meta(task_id: &str, status: TaskStatus, days_ago: Option<i64>) -> TaskMeta {
    TaskMeta {
        task_id: task_id.to_string(),
        tasklist_id: "tl-1".to_string(),
        parent_agent_id: "agent-a".to_string(),
        owner_agent_id: None,
        assignment_mode: None,
        title: format!("Task {task_id}"),
        status,
        created_at: Utc::now() - Duration::days(days_ago.unwrap_or(0) + 1),
        started_at: days_ago.map(|d| Utc::now() - Duration::days(d)),
        ended_at: days_ago.map(|d| Utc::now() - Duration::days(d)),
        summary: None,
        model_used: None,
    }
}

/// Create a task dir under the canonical path:
/// `{root}/tasks/agents/{agent_id}/tasklists/{tl_id}/workspace/tasks/{task_id}/`
/// and write a `meta.json` into it.
async fn seed_task_dir(
    data_root: &DataRoot,
    agent_id: &str,
    tl_id: &str,
    task_id: &str,
    meta: &TaskMeta,
) -> std::path::PathBuf {
    let task_dir = data_root
        .root()
        .join("tasks")
        .join("agents")
        .join(agent_id)
        .join("tasklists")
        .join(tl_id)
        .join("workspace")
        .join("tasks")
        .join(task_id);
    tokio::fs::create_dir_all(&task_dir).await.unwrap();
    let meta_path = task_dir.join("meta.json");
    let json = serde_json::to_string_pretty(meta).unwrap();
    tokio::fs::write(&meta_path, json).await.unwrap();
    task_dir
}

fn enabled_config() -> PrunerConfig {
    PrunerConfig {
        enabled: true,
        dry_run: false,
        ..PrunerConfig::default()
    }
}

fn dry_run_config() -> PrunerConfig {
    PrunerConfig {
        enabled: true,
        dry_run: true,
        ..PrunerConfig::default()
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pruner_dry_run_lists_exact_dirs() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    // Old completed (40 days ago — exceeds default 30-day window).
    let dir_old_completed = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old-completed",
        &make_meta("t-old-completed", TaskStatus::Completed, Some(40)),
    )
    .await;

    // Old failed (70 days ago — exceeds default 60-day window).
    let dir_old_failed = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old-failed",
        &make_meta("t-old-failed", TaskStatus::Failed, Some(70)),
    )
    .await;

    // Recent completed (10 days ago — within 30-day window, keep).
    seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-recent-completed",
        &make_meta("t-recent-completed", TaskStatus::Completed, Some(10)),
    )
    .await;

    // NotStarted with ancient created_at — must never be pruned.
    seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-pending",
        &make_meta("t-pending", TaskStatus::Pending, None),
    )
    .await;

    // Old skipped (70 days ago — treated like Failed).
    let dir_old_skipped = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old-skipped",
        &make_meta("t-old-skipped", TaskStatus::Skipped, Some(70)),
    )
    .await;

    let report = pruner.sweep(&data_root, &dry_run_config()).await;

    // Dry-run: nothing deleted.
    assert!(report.dirs_deleted.is_empty());
    assert!(report.dirs_failed.is_empty());

    // Exactly the three old terminal dirs should be listed.
    let mut listed = report.dirs_to_delete.clone();
    listed.sort();
    let mut expected = vec![dir_old_completed, dir_old_failed, dir_old_skipped];
    expected.sort();
    assert_eq!(listed, expected, "dry-run must list exactly the eligible dirs");

    // Dirs still exist on disk.
    assert!(tmp.path().exists());
}

#[tokio::test]
async fn pruner_real_delete_removes_dirs() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    let dir_del = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-delete",
        &make_meta("t-delete", TaskStatus::Completed, Some(40)),
    )
    .await;

    let dir_keep = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-keep",
        &make_meta("t-keep", TaskStatus::Completed, Some(10)),
    )
    .await;

    let report = pruner.sweep(&data_root, &enabled_config()).await;

    assert_eq!(report.dirs_deleted.len(), 1);
    assert_eq!(report.dirs_to_delete.len(), 0);
    assert_eq!(report.dirs_deleted[0], dir_del);

    assert!(!dir_del.exists(), "old dir should be deleted");
    assert!(dir_keep.exists(), "recent dir must survive");
}

#[tokio::test]
async fn pruner_failed_retention_longer_than_completed() {
    // A task ended exactly 45 days ago:
    // - Completed → pruned   (default 30-day window)
    // - Failed    → kept     (default 60-day window)
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    let dir_completed = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-completed-45",
        &make_meta("t-completed-45", TaskStatus::Completed, Some(45)),
    )
    .await;

    let dir_failed = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-failed-45",
        &make_meta("t-failed-45", TaskStatus::Failed, Some(45)),
    )
    .await;

    let report = pruner.sweep(&data_root, &enabled_config()).await;

    assert!(dir_completed.exists() == false, "completed@45d must be deleted");
    assert!(dir_failed.exists(), "failed@45d must survive (60-day window)");
    assert_eq!(report.dirs_deleted.len(), 1);
    assert_eq!(report.dirs_deleted[0], dir_completed);
}

#[tokio::test]
async fn pruner_never_touches_in_flight() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    // Seed Pending with a fake ancient ended_at — must not be pruned.
    let mut meta_pending = make_meta("t-pending", TaskStatus::Pending, None);
    meta_pending.ended_at = Some(Utc::now() - Duration::days(999));
    let dir_pending = seed_task_dir(&data_root, "agent-a", "tl-1", "t-pending", &meta_pending).await;

    // Seed InProgress with ancient ended_at.
    let mut meta_in_progress = make_meta("t-in-progress", TaskStatus::InProgress, None);
    meta_in_progress.ended_at = Some(Utc::now() - Duration::days(999));
    let dir_in_progress =
        seed_task_dir(&data_root, "agent-a", "tl-1", "t-in-progress", &meta_in_progress).await;

    let report = pruner.sweep(&data_root, &enabled_config()).await;

    assert!(dir_pending.exists(), "Pending task must never be pruned");
    assert!(dir_in_progress.exists(), "InProgress task must never be pruned");
    assert!(report.dirs_deleted.is_empty());
    assert_eq!(report.total_scanned, 2);
}

#[tokio::test]
async fn pruner_swallows_corrupt_meta() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    // Seed one corrupt meta.json.
    let corrupt_dir = data_root
        .root()
        .join("tasks")
        .join("agents")
        .join("agent-a")
        .join("tasklists")
        .join("tl-1")
        .join("workspace")
        .join("tasks")
        .join("t-corrupt");
    tokio::fs::create_dir_all(&corrupt_dir).await.unwrap();
    tokio::fs::write(corrupt_dir.join("meta.json"), b"not json!!!").await.unwrap();

    // Seed one valid old completed dir that should be pruned.
    let dir_valid = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-valid",
        &make_meta("t-valid", TaskStatus::Completed, Some(40)),
    )
    .await;

    // Sweep should continue past the corrupt entry and prune the valid one.
    let report = pruner.sweep(&data_root, &enabled_config()).await;

    assert!(corrupt_dir.exists(), "corrupt dir must not be deleted");
    assert!(!dir_valid.exists(), "valid old dir must be deleted");
    assert_eq!(report.dirs_deleted.len(), 1);
    assert!(report.dirs_failed.is_empty());
}

#[tokio::test]
async fn pruner_is_idempotent() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old",
        &make_meta("t-old", TaskStatus::Completed, Some(40)),
    )
    .await;

    let report1 = pruner.sweep(&data_root, &enabled_config()).await;
    assert_eq!(report1.dirs_deleted.len(), 1);

    // Second run: dir is already gone, nothing to do.
    let report2 = pruner.sweep(&data_root, &enabled_config()).await;
    assert_eq!(report2.dirs_deleted.len(), 0);
    assert_eq!(report2.dirs_failed.len(), 0);
}

#[tokio::test]
async fn pruner_boot_run_disabled_by_default() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    // Seed an old dir that *would* be pruned if enabled.
    let dir = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old",
        &make_meta("t-old", TaskStatus::Completed, Some(40)),
    )
    .await;

    // Default config has enabled: false.
    let report = pruner.sweep(&data_root, &PrunerConfig::default()).await;

    assert!(dir.exists(), "pruner must not delete anything when disabled");
    assert!(report.dirs_deleted.is_empty());
    assert!(report.dirs_to_delete.is_empty());
    assert_eq!(report.total_scanned, 0);
}

#[tokio::test]
async fn pruner_boot_run_enabled_scans_and_deletes() {
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    let dir = seed_task_dir(
        &data_root,
        "agent-a",
        "tl-1",
        "t-old",
        &make_meta("t-old", TaskStatus::Completed, Some(40)),
    )
    .await;

    let config = PrunerConfig {
        enabled: true,
        dry_run: false,
        ..PrunerConfig::default()
    };
    let report = pruner.sweep(&data_root, &config).await;

    assert!(!dir.exists(), "enabled pruner must delete eligible dirs");
    assert_eq!(report.dirs_deleted.len(), 1);
}

#[tokio::test]
async fn pruner_task_without_ended_at_is_kept() {
    // A terminal task with no ended_at is kept (defensive — avoids pruning
    // a task that somehow reached Completed without the ended_at being set).
    let tmp = tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let pruner = TaskTranscriptPruner::new();

    let mut meta = make_meta("t-no-ended", TaskStatus::Completed, None);
    meta.ended_at = None; // explicitly clear
    let dir = seed_task_dir(&data_root, "agent-a", "tl-1", "t-no-ended", &meta).await;

    let report = pruner.sweep(&data_root, &enabled_config()).await;

    assert!(dir.exists(), "task with missing ended_at must not be pruned");
    assert!(report.dirs_deleted.is_empty());
}
