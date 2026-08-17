use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::registry::RunRecord;

/// Input parameters for spawning a new managed process.
#[derive(Debug, Clone)]
pub struct SpawnInput {
    pub run_id: Option<String>,
    pub backend_id: String,
    pub scope_key: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub stdin_data: Option<String>,
    pub timeout_ms: Option<u64>,
    pub no_output_timeout_ms: Option<u64>,
    /// Shared counter of in-flight tool calls. When `> 0`, the idle-output
    /// watchdog pauses — a subagent or long Bash can keep the parent CLI's
    /// stdout silent for minutes without being hung. The caller owns this
    /// counter and hands it to both the supervisor (reader) and the output
    /// normalizer (writer: increments on `tool_use` block start, decrements
    /// on `tool_result` block start).
    pub tools_in_flight: Option<Arc<AtomicUsize>>,
    /// Shared counter of this run's tool calls currently suspended awaiting a
    /// synchronous human answer (`AskUserQuestionWithForm`, `mode: "sync"`).
    /// When `> 0`, the OVERALL wall-clock deadline (`timeout_ms` below) stops
    /// accumulating elapsed time — a human sitting on a form is not the agent
    /// burning its budget.
    ///
    /// Deliberately a SEPARATE counter from `tools_in_flight`, not an
    /// overload of it: a long `Bash` call or a `Task` subagent also holds
    /// `tools_in_flight > 0` for the idle-output watchdog above, but must
    /// keep consuming the overall deadline — only a genuine blocked-on-human
    /// suspension pauses it. The caller owns this counter; the only writer is
    /// `LiveFormBridge::ask_form`'s suspension guard (see
    /// `ao_engine_tools_runner::prompt_bridge`), which increments it the
    /// moment the form is registered and decrements it on every exit path
    /// (answered, cancelled, or the awaiting future dropped) so an abandoned
    /// form can never strand it permanently-on.
    pub form_suspended: Option<Arc<AtomicUsize>>,
}

/// A running process managed by the supervisor.
pub struct ManagedRun {
    pub run_id: String,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub stdout_rx: mpsc::UnboundedReceiver<String>,
    pub stderr_rx: mpsc::UnboundedReceiver<String>,
    pub wait_handle: JoinHandle<RunExit>,
    pub cancel_tx: oneshot::Sender<TerminationReason>,
}

/// Result of a completed process run.
#[derive(Debug, Clone)]
pub struct RunExit {
    pub reason: TerminationReason,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub no_output_timed_out: bool,
}

/// Why a process terminated.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminationReason {
    Natural,
    Cancelled,
    Timeout,
    NoOutputTimeout,
    Error,
}

/// Trait for managing process lifecycle: spawn, cancel, inspect.
#[async_trait]
pub trait ProcessSupervisor: Send + Sync {
    async fn spawn(&self, input: SpawnInput) -> Result<ManagedRun, ao_protocol::error::AoError>;
    async fn cancel(&self, run_id: &str) -> Result<(), ao_protocol::error::AoError>;
    fn get_record(&self, run_id: &str) -> Option<RunRecord>;
    fn list_active(&self) -> Vec<RunRecord>;
}
