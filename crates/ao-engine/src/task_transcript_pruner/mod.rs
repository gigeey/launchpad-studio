use std::path::PathBuf;
use std::time::Duration;

use ao_persistence::paths::DataRoot;
use ao_persistence::task_meta::TaskMeta;
use ao_protocol::tasklist::TaskStatus;
use chrono::Utc;
use tokio::sync::watch;

/// Interval between periodic sweeps — shared cadence with the classifier boot sweep.
const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Configuration for the task transcript pruner.
///
/// Ships behind `enabled: false` for the first dogfood window. Flip to
/// `enabled: true` post-window via the engine.toml config default change.
#[derive(Debug, Clone)]
pub struct PrunerConfig {
    /// Delete completed task dirs older than this many days.
    pub completed_retention_days: u32,
    /// Delete failed / cancelled task dirs older than this many days.
    pub failed_retention_days: u32,
    /// Master switch: when false the sweep is a no-op.
    pub enabled: bool,
    /// When true the sweep reports what it *would* delete without touching disk.
    pub dry_run: bool,
}

impl Default for PrunerConfig {
    fn default() -> Self {
        Self {
            completed_retention_days: 30,
            failed_retention_days: 60,
            enabled: false,
            dry_run: false,
        }
    }
}

/// Summary returned by each `sweep` invocation.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Dirs that would be deleted (dry-run only).
    pub dirs_to_delete: Vec<PathBuf>,
    /// Dirs that were successfully deleted (real run only).
    pub dirs_deleted: Vec<PathBuf>,
    /// Dirs where deletion failed — logged at warn, never fatal.
    pub dirs_failed: Vec<(PathBuf, String)>,
    /// Total task dirs inspected (including skipped non-terminal tasks).
    pub total_scanned: usize,
}

/// Prunes stale per-task content directories by walking every agent's
/// tasklist workspace and checking each task's `meta.json` sidecar.
///
/// Non-terminal tasks (`Pending`, `InProgress`, `Blocked`) are never pruned.
#[derive(Clone)]
pub struct TaskTranscriptPruner;

impl TaskTranscriptPruner {
    pub fn new() -> Self {
        Self
    }

    /// Scan all task dirs and delete those that exceed the configured retention.
    ///
    /// Idempotent: running twice in a row on the same fixture is a no-op on
    /// the second pass because the dirs are already gone.
    pub async fn sweep(&self, data_root: &DataRoot, config: &PrunerConfig) -> SweepReport {
        if !config.enabled {
            tracing::info!("task_transcript_pruner: disabled, skipping sweep");
            return SweepReport::default();
        }

        let now = Utc::now();
        let completed_cutoff =
            now - chrono::Duration::days(config.completed_retention_days as i64);
        let failed_cutoff = now - chrono::Duration::days(config.failed_retention_days as i64);

        let mut report = SweepReport::default();

        // Walk {root}/tasks/agents/{agent_id}/tasklists/{tl_id}/workspace/tasks/{task_id}/
        let agents_tasks_dir = data_root.root().join("tasks").join("agents");

        let mut agent_entries = match tokio::fs::read_dir(&agents_tasks_dir).await {
            Ok(r) => r,
            Err(_) => {
                tracing::info!(
                    "task_transcript_pruner: no agents tasks dir at {:?}, nothing to prune",
                    agents_tasks_dir
                );
                return report;
            }
        };

        while let Ok(Some(agent_entry)) = agent_entries.next_entry().await {
            if !agent_entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let tasklists_dir = agent_entry.path().join("tasklists");
            let mut tl_entries = match tokio::fs::read_dir(&tasklists_dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(tl_entry)) = tl_entries.next_entry().await {
                if !tl_entry
                    .file_type()
                    .await
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let tasks_dir = tl_entry.path().join("workspace").join("tasks");
                let mut task_entries = match tokio::fs::read_dir(&tasks_dir).await {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                while let Ok(Some(task_entry)) = task_entries.next_entry().await {
                    if !task_entry
                        .file_type()
                        .await
                        .map(|t| t.is_dir())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let task_dir = task_entry.path();
                    let meta_path = task_dir.join("meta.json");
                    report.total_scanned += 1;

                    let meta_bytes = match tokio::fs::read(&meta_path).await {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let meta: TaskMeta = match serde_json::from_slice(&meta_bytes) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(
                                "task_transcript_pruner: corrupt meta at {:?}, skipping: {}",
                                meta_path,
                                e
                            );
                            continue;
                        }
                    };

                    // Defensive guard: never prune tasks that are still running.
                    if !meta.status.is_terminal() {
                        continue;
                    }

                    let ended_at = match meta.ended_at {
                        Some(ts) => ts,
                        None => continue,
                    };

                    let should_delete = match meta.status {
                        TaskStatus::Completed => ended_at < completed_cutoff,
                        TaskStatus::Failed | TaskStatus::Skipped => ended_at < failed_cutoff,
                        _ => false,
                    };

                    if !should_delete {
                        continue;
                    }

                    if config.dry_run {
                        report.dirs_to_delete.push(task_dir);
                    } else {
                        match tokio::fs::remove_dir_all(&task_dir).await {
                            Ok(()) => report.dirs_deleted.push(task_dir),
                            Err(e) => {
                                tracing::warn!(
                                    "task_transcript_pruner: failed to delete {:?}: {}",
                                    task_dir,
                                    e
                                );
                                report.dirs_failed.push((task_dir, e.to_string()));
                            }
                        }
                    }
                }
            }
        }

        if config.dry_run {
            tracing::info!(
                "task_transcript_pruner: dry-run complete — {} dirs to delete (scanned {})",
                report.dirs_to_delete.len(),
                report.total_scanned
            );
        } else {
            tracing::info!(
                "task_transcript_pruner: sweep complete — {} deleted, {} failed (scanned {})",
                report.dirs_deleted.len(),
                report.dirs_failed.len(),
                report.total_scanned
            );
        }

        report
    }
}

/// Runs `TaskTranscriptPruner::sweep` once at startup and then every 6 hours.
///
/// Shares the same interval as the classifier boot sweep so the engine does not
/// accrue a third background timer.
pub struct TranscriptPrunerRunner {
    pruner: TaskTranscriptPruner,
    data_root: DataRoot,
    config: PrunerConfig,
}

impl TranscriptPrunerRunner {
    pub fn new(pruner: TaskTranscriptPruner, data_root: DataRoot, config: PrunerConfig) -> Self {
        Self {
            pruner,
            data_root,
            config,
        }
    }

    /// Spawn the runner as a background tokio task.
    /// Returns a shutdown sender — drop it (or send `()`) to stop the loop.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        tracing::info!(
            "TranscriptPrunerRunner starting (interval {:?}, enabled={})",
            SWEEP_INTERVAL,
            self.config.enabled,
        );

        tokio::spawn(async move {
            self.pruner.sweep(&self.data_root, &self.config).await;

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        tracing::info!("TranscriptPrunerRunner shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(SWEEP_INTERVAL) => {
                        self.pruner.sweep(&self.data_root, &self.config).await;
                    }
                }
            }
        });

        shutdown_tx
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
