use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::registry::{RunRecord, RunRegistry, RunStatus};
use crate::supervisor::{
    ManagedRun, ProcessSupervisor, RunExit, SpawnInput, TerminationReason,
};

/// Configuration for a single mock process run.
#[derive(Debug, Clone)]
pub struct MockScenario {
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    pub exit_code: i32,
    pub delay_per_line_ms: u64,
}

/// A mock process supervisor that uses predefined scenarios instead of real processes.
pub struct MockProcessSupervisor {
    scenarios: Arc<Mutex<Vec<MockScenario>>>,
    registry: RunRegistry,
}

impl MockProcessSupervisor {
    pub fn new(scenarios: Vec<MockScenario>) -> Self {
        Self {
            scenarios: Arc::new(Mutex::new(scenarios)),
            registry: RunRegistry::new(),
        }
    }
}

#[async_trait]
impl ProcessSupervisor for MockProcessSupervisor {
    async fn spawn(&self, input: SpawnInput) -> Result<ManagedRun, ao_protocol::error::AoError> {
        let scenario = {
            let mut scenarios = self.scenarios.lock().unwrap();
            if scenarios.is_empty() {
                return Err(ao_protocol::error::AoError::Process(
                    "No mock scenarios remaining".to_string(),
                ));
            }
            scenarios.remove(0)
        };

        let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let started_at = Utc::now();

        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel::<TerminationReason>();

        self.registry.register(RunRecord {
            run_id: run_id.clone(),
            backend_id: input.backend_id.clone(),
            pid: None,
            started_at,
            scope_key: input.scope_key.clone(),
            status: RunStatus::Running,
        });

        let registry = self.registry.clone();
        let run_id_clone = run_id.clone();

        let wait_handle = tokio::spawn(async move {
            let start = std::time::Instant::now();
            let delay = std::time::Duration::from_millis(scenario.delay_per_line_ms);

            tokio::select! {
                reason = cancel_rx => {
                    let reason = reason.unwrap_or(TerminationReason::Cancelled);
                    registry.update_status(&run_id_clone, RunStatus::Cancelled);
                    RunExit {
                        reason,
                        exit_code: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        no_output_timed_out: false,
                    }
                }
                _ = async {
                    for line in &scenario.stdout_lines {
                        tokio::time::sleep(delay).await;
                        let _ = stdout_tx.send(line.clone());
                    }
                    for line in &scenario.stderr_lines {
                        tokio::time::sleep(delay).await;
                        let _ = stderr_tx.send(line.clone());
                    }
                } => {
                    registry.update_status(&run_id_clone, RunStatus::Completed);
                    RunExit {
                        reason: TerminationReason::Natural,
                        exit_code: Some(scenario.exit_code),
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        no_output_timed_out: false,
                    }
                }
            }
        });

        Ok(ManagedRun {
            run_id,
            pid: None,
            started_at,
            stdout_rx,
            stderr_rx,
            wait_handle,
            cancel_tx,
        })
    }

    async fn cancel(&self, run_id: &str) -> Result<(), ao_protocol::error::AoError> {
        // Cancel is handled by the caller via the cancel_tx on ManagedRun.
        // Here we just update the registry status.
        self.registry
            .update_status(run_id, RunStatus::Cancelled);
        Ok(())
    }

    fn get_record(&self, run_id: &str) -> Option<RunRecord> {
        self.registry.get(run_id)
    }

    fn list_active(&self) -> Vec<RunRecord> {
        self.registry.list_active()
    }
}
