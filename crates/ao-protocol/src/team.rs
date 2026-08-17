use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

/// Identifier for a team-owned tasklist.
///
/// Teams themselves were removed, but `TasklistOwner::Team` is retained so
/// tasklists already written to disk still deserialize (the enum is
/// `#[serde(tag = "kind")]`, so dropping the variant would break existing
/// data). This alias is the type that variant carries.
pub type TeamId = String;

/// A routable member: an agent id plus the role it plays for the caller that
/// assembled the list.
///
/// Despite living in this module, this is NOT team-specific — the live
/// producer is `agent_routing`, which builds a `Vec<TeamMember>` from an
/// agent's configured delegates so the task-owner extractor can resolve a
/// name to an agent id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamMember {
    pub agent_id: AgentId,
    pub role_description: String,
    /// Optional working directory override for this member. Takes precedence
    /// over the agent profile's working_dir when delegating.
    #[serde(default)]
    pub working_dir: Option<String>,
}
