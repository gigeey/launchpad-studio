use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

pub type ProjectId = String;

/// Maximum number of verification rounds before escalation is required.
pub const MAX_VERIFICATION_ROUNDS: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Interviewing,
    Active,
    Completed,
    Archived,
    /// Verification repeatedly failed and the project requires human review
    /// before it can be completed. Set automatically when `MAX_VERIFICATION_ROUNDS`
    /// is reached without a passing verdict.
    NeedsReview,
}

/// One round of automated goal verification. Persisted alongside the project
/// record so follow-up calls can reference earlier verdicts and avoid
/// relitigating already-resolved gaps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationRecord {
    pub round: u32,
    pub timestamp: DateTime<Utc>,
    /// `"pass"` or `"fail"`.
    pub verdict: String,
    pub gaps: Vec<String>,
    /// `"high"`, `"medium"`, or `"low"`.
    pub confidence: String,
    pub rationale: String,
    /// Which engine produced this verdict. `"quick"` for the single-model-call
    /// verifier; `"full"` is reserved for the inspection-subagent engine.
    #[serde(default = "default_verification_engine")]
    pub engine: String,
}

fn default_verification_engine() -> String {
    "quick".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    #[serde(default)]
    pub emoji: Option<String>,
    pub goal: String,
    #[serde(default)]
    pub spec: Option<String>,
    pub agent_id: AgentId,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    pub status: ProjectStatus,
    /// Final summary recorded when the project is marked Completed.
    /// Set by the `ProjectComplete` tool; absent on in-progress projects.
    #[serde(default)]
    pub summary: Option<String>,
    /// Ordered log of automated verification rounds. Stored in the project YAML
    /// (not a sidecar) so a single read gives both status and verification history.
    /// Serde default keeps old project files loading cleanly.
    #[serde(default)]
    pub verifications: Vec<VerificationRecord>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
