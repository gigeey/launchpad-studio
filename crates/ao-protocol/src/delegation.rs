use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

pub type DelegationId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelegationRequest {
    pub delegation_id: DelegationId,
    pub target_agent_id: AgentId,
    pub task: String,
    #[serde(default)]
    pub prior_context: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelegationResult {
    pub delegation_id: DelegationId,
    pub source_agent_id: AgentId,
    pub status: DelegationStatus,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DelegationStatus {
    Completed,
    Failed,
    TimedOut,
    Blocked,
}
