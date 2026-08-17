use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing;

use ao_persistence::PersistenceLayer;
use ao_protocol::event::{AgentEventPayload, RunEndReason};
use ao_protocol::message::QueuedMessage;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use ao_protocol::error::AoError;

use crate::agent_runner::{CliAgentRunner, RunComplete, RunScope};
use crate::event_bus::EventBus;
use crate::task_feeder::ProjectDispatcher;

pub enum ProjectMessage {
    User(QueuedMessage),
    Cancel,
}

#[derive(Clone)]
pub struct ProjectQueueManagerHandle {
    pub message_tx: mpsc::Sender<ProjectMessage>,
}

fn project_transcript_key(project_id: &str) -> String {
    format!("project_{}", project_id)
}

fn project_event_agent_id(project_id: &str) -> String {
    format!("project:{}", project_id)
}

struct ProjectRunResult {
    run_id: String,
    output_text: String,
    cancelled: bool,
}

/// Per-project queue manager: serialises user↔agent turns for one project.
pub struct ProjectQueueManager {
    project_id: String,
    queue: VecDeque<ProjectMessage>,
    message_rx: mpsc::Receiver<ProjectMessage>,
    run_complete_tx: mpsc::Sender<ProjectRunResult>,
    run_complete_rx: mpsc::Receiver<ProjectRunResult>,
    is_running: bool,
    cancellation_token: CancellationToken,
    agent_runner: Arc<CliAgentRunner>,
    event_bus: Arc<EventBus>,
    persistence: Arc<PersistenceLayer>,
    heartbeat_interval: Duration,
}

impl ProjectQueueManager {
    fn new(
        project_id: String,
        message_rx: mpsc::Receiver<ProjectMessage>,
        agent_runner: Arc<CliAgentRunner>,
        event_bus: Arc<EventBus>,
        persistence: Arc<PersistenceLayer>,
    ) -> Self {
        let (run_complete_tx, run_complete_rx) = mpsc::channel(8);
        Self {
            project_id,
            queue: VecDeque::new(),
            message_rx,
            run_complete_tx,
            run_complete_rx,
            is_running: false,
            cancellation_token: CancellationToken::new(),
            agent_runner,
            event_bus,
            persistence,
            heartbeat_interval: Duration::from_secs(5),
        }
    }

    pub async fn run(mut self) {
        let mut heartbeat = tokio::time::interval(self.heartbeat_interval);
        heartbeat.tick().await;

        loop {
            tokio::select! {
                msg = self.message_rx.recv() => {
                    match msg {
                        Some(ProjectMessage::Cancel) => {
                            self.cancel_run().await;
                            self.pump().await;
                        }
                        Some(msg) => {
                            self.queue.push_back(msg);
                            self.pump().await;
                        }
                        None => break,
                    }
                }
                result = self.run_complete_rx.recv() => {
                    match result {
                        Some(r) if r.cancelled => {
                            self.pump().await;
                        }
                        Some(r) => {
                            self.is_running = false;
                            self.handle_run_result(r).await;
                            self.pump().await;
                        }
                        None => break,
                    }
                }
                _ = heartbeat.tick() => {
                    tracing::debug!(
                        project_id = %self.project_id,
                        queue_depth = self.queue.len(),
                        is_running = self.is_running,
                        "Project queue manager heartbeat"
                    );
                    self.pump().await;
                }
            }
        }
    }

    async fn pump(&mut self) {
        if self.is_running || self.queue.is_empty() {
            return;
        }

        let message = self.queue.pop_front().unwrap();
        let ProjectMessage::User(queued_msg) = message else {
            return;
        };

        let project = match self.persistence.projects.get(&self.project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(project_id = %self.project_id, "Project not found during pump");
                return;
            }
            Err(e) => {
                tracing::error!(project_id = %self.project_id, "Failed to load project: {}", e);
                return;
            }
        };

        let agent_profile = match self.persistence.agents.get(&project.agent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::error!(
                    project_id = %self.project_id,
                    agent_id = %project.agent_id,
                    "Project agent not found"
                );
                return;
            }
            Err(e) => {
                tracing::error!(
                    project_id = %self.project_id,
                    "Failed to load project agent: {}",
                    e
                );
                return;
            }
        };

        let effective_cwd = queued_msg
            .focus_path
            .clone()
            .or_else(|| project.working_dir.clone())
            .or_else(|| agent_profile.working_dir.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".to_string())
            });

        // NOTE: project context (goal/spec + status role block) is NOT
        // injected here. The runner appends it after composing the system
        // prompt — see `project_context::append_project_context`, invoked for
        // `RunScope::Project` runs. Mutating `system_prompt` on the profile
        // here would be a silent no-op: the composer rebuilds the prompt from
        // persona/special_instructions and discards the legacy field.
        self.is_running = true;
        self.cancellation_token = CancellationToken::new();
        let cancel_token = self.cancellation_token.clone();

        let (bridge_tx, mut bridge_rx) = mpsc::channel::<RunComplete>(1);
        let run_complete_tx = self.run_complete_tx.clone();
        let project_id = self.project_id.clone();
        let prompt = queued_msg.content.clone();
        let attachments = queued_msg.attachments.clone();

        let scope = RunScope::Project {
            project_id: self.project_id.clone(),
        };

        match self
            .agent_runner
            .run_with_scope(
                &agent_profile,
                &prompt,
                &attachments,
                bridge_tx,
                scope,
                Some(&effective_cwd),
            )
            .await
        {
            Ok(run_id) => {
                tracing::debug!(
                    project_id = %self.project_id,
                    run_id = %run_id,
                    "Project agent run started"
                );
                let collector_run_id = run_id;
                tokio::spawn(async move {
                    tokio::select! {
                        result = bridge_rx.recv() => {
                            let output_text = result.map(|rc| rc.output_text).unwrap_or_default();
                            let _ = run_complete_tx.send(ProjectRunResult {
                                run_id: collector_run_id,
                                output_text,
                                cancelled: false,
                            }).await;
                        }
                        _ = cancel_token.cancelled() => {
                            let _ = run_complete_tx.send(ProjectRunResult {
                                run_id: collector_run_id,
                                output_text: String::new(),
                                cancelled: true,
                            }).await;
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!(
                    project_id = %project_id,
                    "Failed to start project agent run: {}",
                    e
                );
                self.is_running = false;
            }
        }
    }

    async fn handle_run_result(&self, result: ProjectRunResult) {
        let transcript_key = project_transcript_key(&self.project_id);
        let project_agent_id = project_event_agent_id(&self.project_id);

        let project_id_for_log = self.project_id.clone();
        let project = match self.persistence.projects.get(&self.project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(project_id = %project_id_for_log, "Project not found when recording response");
                return;
            }
            Err(e) => {
                tracing::error!(project_id = %project_id_for_log, "Failed to load project for response: {}", e);
                return;
            }
        };

        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent {
                agent: project.agent_id.clone(),
            },
            content: result.output_text.clone(),
            event_type: "response".to_string(),
            metadata: None,
            hidden_from_user: false,
        };

        if let Err(e) = self.persistence.transcripts.append(&transcript_key, &entry).await {
            tracing::error!(
                project_id = %self.project_id,
                "Failed to persist project agent response: {}",
                e
            );
        }

        self.event_bus
            .emit(
                &result.run_id,
                &project_agent_id,
                None,
                AgentEventPayload::TextComplete {
                    text: result.output_text,
                },
            )
            .await;

        self.event_bus
            .emit(
                &result.run_id,
                &project_agent_id,
                None,
                AgentEventPayload::RunEnded {
                    reason: RunEndReason::Completed,
                },
            )
            .await;

        self.event_bus
            .emit(
                &result.run_id,
                &project_agent_id,
                None,
                AgentEventPayload::TeamRoundCompleted {
                    round: 0,
                    has_more_delegations: false,
                },
            )
            .await;
    }

    async fn cancel_run(&mut self) {
        if self.is_running {
            self.cancellation_token.cancel();
            self.cancellation_token = CancellationToken::new();
            self.is_running = false;
        }
        self.queue.clear();
    }
}

/// Registry of per-project queue managers, created lazily.
pub struct ProjectQueueManagerRegistry {
    handles: Arc<RwLock<HashMap<String, ProjectQueueManagerHandle>>>,
    agent_runner: Arc<CliAgentRunner>,
    event_bus: Arc<EventBus>,
    persistence: Arc<PersistenceLayer>,
}

impl ProjectQueueManagerRegistry {
    pub fn new(
        agent_runner: Arc<CliAgentRunner>,
        _instance_registry: Arc<crate::instance_registry::InstanceRegistry>,
        event_bus: Arc<EventBus>,
        persistence: Arc<PersistenceLayer>,
    ) -> Self {
        Self {
            handles: Arc::new(RwLock::new(HashMap::new())),
            agent_runner,
            event_bus,
            persistence,
        }
    }

    pub async fn get_or_create(&self, project_id: &str) -> ProjectQueueManagerHandle {
        let mut handles = self.handles.write().await;
        if let Some(h) = handles.get(project_id) {
            return h.clone();
        }

        let (message_tx, message_rx) = mpsc::channel::<ProjectMessage>(128);
        let handle = ProjectQueueManagerHandle { message_tx };

        let manager = ProjectQueueManager::new(
            project_id.to_string(),
            message_rx,
            Arc::clone(&self.agent_runner),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.persistence),
        );

        tokio::spawn(manager.run());
        handles.insert(project_id.to_string(), handle.clone());
        handle
    }

    pub async fn submit_message(
        &self,
        project_id: &str,
        message: ProjectMessage,
    ) -> Result<(), ao_protocol::error::AoError> {
        let handle = self.get_or_create(project_id).await;
        handle
            .message_tx
            .send(message)
            .await
            .map_err(|e| ao_protocol::error::AoError::Internal(format!("Project queue send error: {}", e)))
    }

    pub async fn cancel_project(&self, project_id: &str) -> bool {
        let handles = self.handles.read().await;
        if let Some(h) = handles.get(project_id) {
            let _ = h.message_tx.send(ProjectMessage::Cancel).await;
            true
        } else {
            false
        }
    }

    pub async fn remove_project(&self, project_id: &str) {
        self.handles.write().await.remove(project_id);
    }
}

#[async_trait::async_trait]
impl ProjectDispatcher for ProjectQueueManagerRegistry {
    async fn submit_to_project(
        &self,
        project_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError> {
        self.submit_message(project_id, ProjectMessage::User(message)).await
    }
}
