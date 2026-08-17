//! Per-task delegate-target classifier.
//!
//! `TaskClassifier::classify` loads the parent agent's delegate targets, short-circuits
//! to self-route when the agent has no targets (no model call), and otherwise dispatches a
//! single-shot CLI call that returns a strict JSON `{ "owner_agent_id": "..." }`.
//!
//! Concurrency is bounded by a `tokio::sync::Semaphore` with `CLASSIFY_POOL_SIZE`
//! permits (default 4, per IRV-1). Each attempt has a hard `CLASSIFY_TIMEOUT_SECS`
//! timeout (default 30 s). Retries and the boot sweep live in callers;
//! this module only performs one attempt per invocation.

pub mod prompt;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use ao_engine_tools_core::{ClassifierHandle, ClassifyOutcome, TasklistServiceHandle};
use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::PersistenceLayer;
use ao_process::supervisor::{ProcessSupervisor, SpawnInput};
use ao_protocol::agent::{AgentProfile, InputMode, ProviderConfig};
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::{AssignmentMode, TaskAssignment, TaskStatus};

use crate::agent_runner::CliAgentRunner;

pub use prompt::build_classify_prompt;

/// Maximum number of concurrent classifier calls (IRV-1 resolution).
pub const CLASSIFY_POOL_SIZE: usize = 4;

/// Hard per-attempt timeout in seconds.
pub const CLASSIFY_TIMEOUT_SECS: u64 = 30;

// ── Error type ───────────────────────────────────────────────────────────────

/// Error from a single classify attempt.
///
/// `ParseFailed` is semantically retryable (the model might do better next time)
/// but is a distinct variant so callers can pattern-match on it without string
/// comparison. Network/timeout failures map to `Retryable`. Schema violations
/// that are clearly non-transient map to `Permanent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyError {
    Retryable(String),
    Permanent(String),
    ParseFailed(String),
}

impl std::fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(r) => write!(f, "retryable: {r}"),
            Self::Permanent(r) => write!(f, "permanent: {r}"),
            Self::ParseFailed(r) => write!(f, "parse_failed: {r}"),
        }
    }
}

// ── Internal provider trait ──────────────────────────────────────────────────

/// Single-shot model dispatch abstraction. Decouples `TaskClassifier` from the
/// CLI process layer so tests can inject a mock without spinning up processes.
#[async_trait]
pub(crate) trait ClassifyCallProvider: Send + Sync {
    async fn single_shot(
        &self,
        agent: &AgentProfile,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, ClassifyError>;
}

// ── TaskClassifier ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TaskClassifier {
    persistence: Arc<PersistenceLayer>,
    call_provider: Arc<dyn ClassifyCallProvider>,
    semaphore: Arc<Semaphore>,
    timeout_secs: u64,
}

impl TaskClassifier {
    /// Production constructor: wires the CLI process supervisor as the provider.
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        process_supervisor: Arc<dyn ProcessSupervisor>,
        normalizer_registry: Arc<NormalizerRegistry>,
    ) -> Self {
        Self::new_with_config(
            persistence,
            Arc::new(CliClassifyCallProvider {
                process_supervisor,
                normalizer_registry,
            }),
            CLASSIFY_POOL_SIZE,
            CLASSIFY_TIMEOUT_SECS,
        )
    }

    /// Configurable constructor used by tests (injectable provider + custom pool/timeout).
    pub(crate) fn new_with_config(
        persistence: Arc<PersistenceLayer>,
        call_provider: Arc<dyn ClassifyCallProvider>,
        pool_size: usize,
        timeout_secs: u64,
    ) -> Self {
        Self {
            persistence,
            call_provider,
            semaphore: Arc::new(Semaphore::new(pool_size)),
            timeout_secs,
        }
    }

    /// Classify ownership for a single task.
    ///
    /// Returns `Ok(TaskAssignment { owner: parent, mode: Classified })` immediately
    /// when the parent agent has no delegate targets (no model call made).
    pub async fn classify(
        &self,
        parent_agent_id: &str,
        _task_id: &str,
        task_title: &str,
        task_description: &str,
    ) -> Result<TaskAssignment, ClassifyError> {
        // ── Load parent agent profile (delegate targets + system-prompt prefix) ─
        let agent = match self.persistence.agents.get(parent_agent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Err(ClassifyError::Permanent(format!(
                    "parent agent '{parent_agent_id}' not found"
                )))
            }
            Err(e) => {
                return Err(ClassifyError::Retryable(format!(
                    "failed to load agent profile: {e}"
                )))
            }
        };

        // ── No-delegate short-circuit (NO model call) ────────────────────────
        if agent.delegates_to.is_empty() {
            return Ok(TaskAssignment {
                owner_agent_id: parent_agent_id.to_string(),
                mode: AssignmentMode::Classified,
            });
        }

        let valid_ids: Vec<String> = agent
            .delegates_to
            .iter()
            .map(|d| d.target_agent_id.clone())
            .collect();

        let (system_prompt, user_prompt) = build_classify_prompt(
            &agent.delegates_to,
            agent.system_prompt.as_deref(),
            task_title,
            task_description,
        );

        // ── Acquire semaphore permit (bounded pool) ──────────────────────────
        let _permit = self.semaphore.acquire().await.map_err(|_| {
            ClassifyError::Retryable("concurrency semaphore closed".to_string())
        })?;

        // ── Dispatch with hard per-attempt timeout ───────────────────────────
        let call_future = self
            .call_provider
            .single_shot(&agent, &system_prompt, &user_prompt);
        let raw_output = match tokio::time::timeout(
            Duration::from_secs(self.timeout_secs),
            call_future,
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(ClassifyError::Retryable(format!(
                    "classify timed out after {}s",
                    self.timeout_secs
                )))
            }
        };

        parse_classify_output(&raw_output, &valid_ids, parent_agent_id)
    }

    /// Scan every agent-owned tasklist for tasks with `assignment: None` and
    /// `status: Pending`, then spawn a background classifier for each orphan.
    ///
    /// Called once at engine boot (to recover from crashes mid-classification)
    /// and on a 6-hour timer (to re-attempt tasks whose retry budget was
    /// exhausted before the previous boot). Idempotent: concurrent writes from
    /// in-flight classifiers are detected by the CAS token and silently
    /// discarded.
    pub async fn run_boot_sweep(&self, svc: Arc<dyn TasklistServiceHandle + Send + Sync>) {
        let agents = match self.persistence.agents.list().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "boot_sweep: agent list failed");
                return;
            }
        };

        // Collect (agent_id, tasklist_id, task_id, title, desc, token) for every orphan.
        let mut orphans: Vec<(String, String, String, String, String, u64)> = vec![];

        for agent in &agents {
            let tasklists = match self.persistence.tasklists.list_for_agent(&agent.id).await {
                Ok(tl) => tl,
                Err(e) => {
                    tracing::warn!(agent_id = %agent.id, error = %e, "boot_sweep: tasklist list failed");
                    continue;
                }
            };
            for tl in &tasklists {
                for group in &tl.groups {
                    for task in &group.tasks {
                        if task.assignment.is_none() && task.status == TaskStatus::Pending {
                            let (title, desc) = {
                                let mut parts = task.prompt.splitn(2, ": ");
                                let t = parts.next().unwrap_or("").to_string();
                                let d = parts.next().unwrap_or("").to_string();
                                (t, d)
                            };
                            orphans.push((
                                agent.id.clone(),
                                tl.id.clone(),
                                task.id.clone(),
                                title,
                                desc,
                                task.classifier_token,
                            ));
                        }
                    }
                }
            }
        }

        tracing::info!(
            orphan_count = orphans.len(),
            "boot_sweep: starting, {} orphaned tasks to re-classify",
            orphans.len()
        );

        let succeeded = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let total = orphans.len();

        let mut handles = Vec::with_capacity(total);
        for (agent_id, tasklist_id, task_id, title, desc, expected_token) in orphans {
            let classifier = self.clone();
            let svc_clone = Arc::clone(&svc);
            let suc = Arc::clone(&succeeded);
            let fail = Arc::clone(&failed);

            handles.push(tokio::spawn(async move {
                match classifier.classify(&agent_id, &task_id, &title, &desc).await {
                    Ok(assignment) => {
                        match svc_clone
                            .set_assignment(
                                &agent_id,
                                &tasklist_id,
                                &task_id,
                                Some(assignment),
                                expected_token,
                            )
                            .await
                        {
                            Ok(true) => {
                                tracing::debug!(task_id = %task_id, "boot_sweep: assignment written");
                                suc.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(false) => {
                                tracing::debug!(task_id = %task_id, "boot_sweep: stale token, discarded");
                                suc.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::warn!(task_id = %task_id, error = %e, "boot_sweep: set_assignment failed");
                                fail.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task_id, error = %e, "boot_sweep: classify failed");
                        fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        tracing::info!(
            total = total,
            succeeded = succeeded.load(Ordering::Relaxed),
            failed = failed.load(Ordering::Relaxed),
            "boot_sweep: complete"
        );
    }
}

// ── ClassifierHandle adapter ─────────────────────────────────────────────────
//
// Implements the engine-tools-core `ClassifierHandle` trait so a `TaskClassifier`
// can be injected into `RunnerContext` and consumed by Todo* tools without those
// tools needing to know about the concrete classifier type. Maps the strongly-
// typed `ClassifyError` variants to the engine-tools-facing `ClassifyOutcome`:
// `Retryable` and `ParseFailed` both signal "try again with backoff" semantics;
// `Permanent` signals "give up and leave assignment empty".
#[async_trait]
impl ClassifierHandle for TaskClassifier {
    async fn classify(
        &self,
        parent_agent_id: &str,
        task_id: &str,
        task_title: &str,
        task_description: &str,
    ) -> ClassifyOutcome {
        match self
            .classify(parent_agent_id, task_id, task_title, task_description)
            .await
        {
            Ok(assignment) => ClassifyOutcome::Assigned(assignment),
            Err(ClassifyError::Retryable(reason)) => ClassifyOutcome::Retryable(reason),
            Err(ClassifyError::ParseFailed(reason)) => ClassifyOutcome::Retryable(reason),
            Err(ClassifyError::Permanent(reason)) => ClassifyOutcome::Permanent(reason),
        }
    }
}

// ── Output parser ────────────────────────────────────────────────────────────

fn parse_classify_output(
    raw: &str,
    valid_ids: &[String],
    parent_agent_id: &str,
) -> Result<TaskAssignment, ClassifyError> {
    // Strip preamble/markdown: find the first `{` and last `}`.
    let start = raw.find('{').ok_or_else(|| {
        ClassifyError::ParseFailed("no JSON object found in output".to_string())
    })?;
    let end = raw.rfind('}').ok_or_else(|| {
        ClassifyError::ParseFailed("no closing brace in output".to_string())
    })?;
    if end < start {
        return Err(ClassifyError::ParseFailed(
            "malformed JSON braces in output".to_string(),
        ));
    }

    let json_str = &raw[start..=end];
    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| {
            ClassifyError::ParseFailed(format!("JSON parse error: {e}"))
        })?;

    let owner_val = parsed.get("owner_agent_id").ok_or_else(|| {
        ClassifyError::ParseFailed("missing 'owner_agent_id' field".to_string())
    })?;

    let owner_agent_id = match owner_val {
        serde_json::Value::Null => parent_agent_id.to_string(),
        serde_json::Value::String(s) => {
            if !valid_ids.contains(s) {
                return Err(ClassifyError::ParseFailed(format!(
                    "owner_agent_id '{s}' is not in the delegate targets"
                )));
            }
            s.clone()
        }
        other => {
            return Err(ClassifyError::ParseFailed(format!(
                "owner_agent_id must be a string or null, got: {other}"
            )));
        }
    };

    Ok(TaskAssignment {
        owner_agent_id,
        mode: AssignmentMode::Classified,
    })
}

// ── Production CLI provider ──────────────────────────────────────────────────

struct CliClassifyCallProvider {
    process_supervisor: Arc<dyn ProcessSupervisor>,
    normalizer_registry: Arc<NormalizerRegistry>,
}

#[async_trait]
impl ClassifyCallProvider for CliClassifyCallProvider {
    async fn single_shot(
        &self,
        agent: &AgentProfile,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, ClassifyError> {
        let mut profile = agent.clone();
        profile.system_prompt = Some(system_prompt.to_string());

        let argv = CliAgentRunner::build_argv(&profile, user_prompt, None, None);

        let ProviderConfig::Cli(ref cli_config) = profile.provider;

        let stdin_data = if cli_config.input_mode == InputMode::Stdin {
            if cli_config.system_prompt_arg.is_none() {
                Some(format!(
                    "[System Instructions]\n{system_prompt}\n[End System Instructions]\n\n{user_prompt}"
                ))
            } else {
                Some(user_prompt.to_string())
            }
        } else {
            None
        };

        let cwd = profile.working_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        });

        let tools_in_flight = Arc::new(AtomicUsize::new(0));

        let spawn_input = SpawnInput {
            run_id: None,
            backend_id: profile.id.clone(),
            scope_key: None,
            argv,
            cwd: Some(cwd),
            env: Some(profile.env.clone()),
            stdin_data,
            timeout_ms: Some(profile.timeout_seconds * 1000),
            no_output_timeout_ms: Some(cli_config.no_output_timeout_ms),
            tools_in_flight: Some(tools_in_flight.clone()),
            form_suspended: None,
        };

        let managed = self
            .process_supervisor
            .spawn(spawn_input)
            .await
            .map_err(|e| ClassifyError::Retryable(format!("process spawn error: {e}")))?;

        let ao_process::supervisor::ManagedRun {
            mut stdout_rx,
            mut stderr_rx,
            wait_handle,
            ..
        } = managed;

        let mut normalizer = self
            .normalizer_registry
            .create(&cli_config.command, cli_config);
        normalizer.set_tools_in_flight_counter(tools_in_flight);

        let stderr_handle = tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(line) = stderr_rx.recv().await {
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let mut delta_buf = String::new();
        let mut complete_text: Option<String> = None;

        while let Some(chunk) = stdout_rx.recv().await {
            for payload in normalizer.process_chunk(&chunk) {
                match payload {
                    AgentEventPayload::TextDelta { text } => delta_buf.push_str(&text),
                    AgentEventPayload::TextComplete { text } => complete_text = Some(text),
                    _ => {}
                }
            }
        }

        let exit = wait_handle.await.map_err(|e| {
            ClassifyError::Retryable(format!("process wait error: {e}"))
        })?;

        let stderr_text = stderr_handle.await.unwrap_or_default();

        for payload in normalizer.finalize(exit.exit_code, &stderr_text) {
            match payload {
                AgentEventPayload::TextDelta { text } => delta_buf.push_str(&text),
                AgentEventPayload::TextComplete { text } => complete_text = Some(text),
                _ => {}
            }
        }

        Ok(complete_text.unwrap_or(delta_buf))
    }
}

