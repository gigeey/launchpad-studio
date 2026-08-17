use std::path::PathBuf;

use async_trait::async_trait;

/// Evidence gathered from one completed tasklist that was tagged with the
/// project being verified.
#[derive(Debug, Clone)]
pub struct TasklistEvidence {
    pub title: String,
    /// Human-readable completion summary (per-task results assembled by the
    /// task feeder at completion time).
    pub summary: String,
}

/// A previous verification verdict fed back into subsequent calls so the verifier
/// can track settled vs. open gaps without reading the full chat transcript.
#[derive(Debug, Clone)]
pub struct PriorVerdict {
    pub round: u32,
    pub verdict: String,
    pub gaps: Vec<String>,
}

/// All inputs assembled by `ProjectVerify` before calling the engine.
/// The engine receives no chat transcript — only the goal, spec, and
/// objective work evidence.
#[derive(Debug, Clone)]
pub struct VerificationInput {
    pub project_id: String,
    pub goal: String,
    pub spec: Option<String>,
    pub tasklist_evidence: Vec<TasklistEvidence>,
    pub prior_verdicts: Vec<PriorVerdict>,
    /// Optional pointer(s) to additional evidence supplied by the calling agent
    /// (e.g. file paths, test output snippets). Passed verbatim to the engine.
    pub extra_evidence: Option<String>,
    /// Filesystem path to the project's working directory. Used by the full
    /// inspection engine to set the child process's working directory so it
    /// can read files, run git diff, and discover the test suite. When `None`
    /// the inspection engine falls back to the quick engine and notes the
    /// limitation in the returned verdict rationale.
    pub working_dir: Option<PathBuf>,
}

/// Structured verdict from the verifier. Used both as the live tool result and
/// persisted as a `VerificationRecord`.
#[derive(Debug, Clone)]
pub struct VerificationVerdict {
    /// `"pass"` or `"fail"`.
    pub verdict: String,
    pub gaps: Vec<String>,
    /// `"high"`, `"medium"`, or `"low"`.
    pub confidence: String,
    pub rationale: String,
}

/// Pluggable verification back-end.
///
/// Defined in this crate so `RunnerContext` can hold an optional engine without
/// creating a circular crate dependency.
///
/// Two production implementations exist in `ao-engine-tools-runner`:
/// - `ProviderVerificationEngine` (mode=`"quick"`) — a single uncached model
///   call that judges the goal against tasklist summaries.
/// - `InspectionVerifier` (mode=`"full"`) — spawns an isolated read-only
///   child agent that opens the working directory, reads diffs, and runs
///   the test suite before issuing its verdict.
///
/// Tests inject a scripted mock so no live provider is needed.
#[async_trait]
pub trait VerificationEngine: Send + Sync {
    async fn verify(&self, input: VerificationInput) -> Result<VerificationVerdict, String>;
}
