use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub phases: Vec<PhaseDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseType {
    Prompt,
    Folder,
    Input,
    Pause,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub intent: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_type: Option<PhaseType>,
    #[serde(default = "default_auto_advance")]
    pub auto_advance: bool,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub inputs: Vec<PhaseInput>,
    #[serde(default)]
    pub outputs: Vec<PhaseOutput>,
    #[serde(default)]
    pub fields: Vec<InputField>,
}

/// A form field for input-type phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputField {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseInput {
    pub id: String,
    #[serde(default)]
    pub from_phase: Option<String>,
    #[serde(default)]
    pub from_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseOutput {
    pub id: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Where a workflow definition originated. Mirrors the provenance taxonomy
/// used elsewhere (skills/rules `AddedBy`) but named for workflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowSource {
    Project,
    User,
    Plugin,
}

impl Default for WorkflowSource {
    fn default() -> Self {
        WorkflowSource::User
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub phase_count: usize,
    #[serde(default)]
    pub source: WorkflowSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_on: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseStatus {
    Completed,
    Skipped,
    Running,
    Failed,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseState {
    pub status: PhaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Archived,
    Stopped,
}

fn default_auto_advance() -> bool {
    true
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    #[serde(default)]
    pub status: TaskStatus,
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_version: Option<String>,
    pub created: DateTime<Utc>,
    pub project_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub context: HashMap<String, String>,
    #[serde(default)]
    pub phases: HashMap<String, PhaseState>,
}
