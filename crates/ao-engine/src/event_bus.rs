use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

use ao_engine_tools_core::context::{EventSink, UserEvent};
use ao_protocol::agent::AgentId;
use ao_protocol::error::AoError;
use ao_protocol::event::{
    AgentEvent, AgentEventPayload, FormFieldEventPayload, FormOptionEventPayload,
    TodoListCreatedItem,
};

pub struct EventBus {
    seq_by_run: Arc<RwLock<HashMap<String, Arc<AtomicU64>>>>,
    /// Separate sync-safe sequence tracker for `emit_sync` callers (NativeAgentRunner path).
    /// Uses different run_ids than the async path so the counters never collide.
    seq_by_run_sync: Arc<std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>>,
    tx: broadcast::Sender<AgentEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            seq_by_run: Arc::new(RwLock::new(HashMap::new())),
            seq_by_run_sync: Arc::new(std::sync::Mutex::new(HashMap::new())),
            tx,
        }
    }

    pub async fn emit(
        &self,
        run_id: &str,
        agent_id: &AgentId,
        thread_id: Option<String>,
        payload: AgentEventPayload,
    ) -> AgentEvent {
        let seq_counter = {
            let mut map = self.seq_by_run.write().await;
            map.entry(run_id.to_string())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                .clone()
        };

        let seq = seq_counter.fetch_add(1, Ordering::SeqCst);

        let event = AgentEvent {
            event_id: Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            seq,
            ts: Utc::now(),
            agent_id: agent_id.clone(),
            thread_id,
            payload,
        };

        // Ignore send error (no receivers is fine)
        let _ = self.tx.send(event.clone());

        event
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    pub async fn cleanup_run(&self, run_id: &str) {
        let mut map = self.seq_by_run.write().await;
        map.remove(run_id);
    }

    /// Synchronous, non-blocking emit for use from `SessionEventSink::emit`
    /// implementations (e.g. `TimelineAdapter`) that run on the hot path of
    /// a streaming turn and cannot `.await`.
    pub fn emit_sync(
        &self,
        run_id: &str,
        agent_id: &AgentId,
        thread_id: Option<String>,
        payload: AgentEventPayload,
    ) {
        let seq_counter = {
            let mut map = self.seq_by_run_sync.lock().expect("seq_by_run_sync poisoned");
            map.entry(run_id.to_string())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                .clone()
        };
        let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
        let event = AgentEvent {
            event_id: Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            seq,
            ts: Utc::now(),
            agent_id: agent_id.clone(),
            thread_id,
            payload,
        };
        let _ = self.tx.send(event);
    }

    pub fn cleanup_run_sync(&self, run_id: &str) {
        let mut map = self.seq_by_run_sync.lock().expect("seq_by_run_sync poisoned");
        map.remove(run_id);
    }
}

/// Routes tool-emitted [`UserEvent`]s to the [`EventBus`] for a specific agent.
///
/// `Brief` events become `AgentEventPayload::SystemMessage`; all other variants
/// are silently discarded (no error, no panic).
pub struct EventBusAgentSink {
    pub bus: Arc<EventBus>,
    pub agent_id: AgentId,
    /// The thread the invoking run belongs to, `None` for the default thread.
    /// A sink instance is scoped to exactly one run, and a run belongs to
    /// exactly one thread, so every event this sink forwards is stamped with
    /// the same value.
    pub thread_id: Option<String>,
}

#[async_trait]
impl EventSink for EventBusAgentSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        let payload = match event {
            UserEvent::Brief { content } => AgentEventPayload::SystemMessage { text: content, severity: None },
            UserEvent::ToolProgress {
                tasklist_id,
                items_done,
                items_total,
                last_terminal_task_title,
            } => AgentEventPayload::ToolProgress {
                tasklist_id,
                items_done,
                items_total,
                last_terminal_task_title,
            },
            UserEvent::TodoListCreated {
                tasklist_id,
                item_count,
                items,
            } => AgentEventPayload::TodoListCreated {
                tasklist_id,
                item_count,
                items: items
                    .into_iter()
                    .map(|i| TodoListCreatedItem {
                        task_id: i.task_id,
                        title: i.title,
                        assignment: i.assignment,
                    })
                    .collect(),
            },
            UserEvent::FormRequest {
                id,
                agent_id,
                session_id,
                title,
                intro,
                fields,
            } => AgentEventPayload::FormRequest {
                form_id: id,
                agent_id,
                session_id,
                title,
                intro,
                fields: fields
                    .into_iter()
                    .map(|f| FormFieldEventPayload {
                        id: f.id,
                        kind: f.kind,
                        label: f.label,
                        description: f.description,
                        required: f.required,
                        options: f.options.map(|opts| {
                            opts.into_iter()
                                .map(|o| FormOptionEventPayload {
                                    id: o.id,
                                    label: o.label,
                                    description: o.description,
                                })
                                .collect()
                        }),
                        placeholder: f.placeholder,
                        max_files: f.max_files,
                        accept: f.accept,
                    })
                    .collect(),
            },
            UserEvent::FormPosted { form_id, spec } => {
                AgentEventPayload::FormPosted { form_id, spec: spec.into() }
            }
            UserEvent::ThreadRenamed { thread_id, title } => AgentEventPayload::ThreadRenamed {
                thread_id,
                title: Some(title),
                auto_title: None,
            },
            UserEvent::ProjectStateChanged {
                project_id,
                status,
                name,
            } => AgentEventPayload::ProjectStateChanged {
                project_id,
                status,
                name,
            },
            _ => return Ok(()),
        };
        self.bus
            .emit_sync(&self.agent_id, &self.agent_id, self.thread_id.clone(), payload);
        Ok(())
    }
}
