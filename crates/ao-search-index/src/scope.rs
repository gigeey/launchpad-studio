/// Which artifact store a search-index row came from.
///
/// A single index serves both the memory store and the skill registry;
/// every row is tagged so a query can be restricted to one artifact type
/// (or left unrestricted to search across both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Memory,
    Skill,
}

impl ArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Memory => "memory",
            ArtifactKind::Skill => "skill",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "memory" => Some(ArtifactKind::Memory),
            "skill" => Some(ArtifactKind::Skill),
            _ => None,
        }
    }
}

/// The WHO×WHERE scope a search-index row is visible under.
///
/// Mirrors the memory store's scope matrix (agent / project / global, plus
/// the reserved `agent×project` cell) without depending on
/// `ao_protocol::memory::MemoryScope` directly, so this crate stays usable
/// by artifact stores that never touch the memory domain (e.g. the skill
/// registry, which indexes everything under [`IndexScope::Global`] today).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexScope {
    Agent(String),
    Project(String),
    Global,
    AgentProject(String),
}

impl IndexScope {
    pub(crate) fn kind_str(&self) -> &'static str {
        match self {
            IndexScope::Agent(_) => "agent",
            IndexScope::Project(_) => "project",
            IndexScope::Global => "global",
            IndexScope::AgentProject(_) => "agent_project",
        }
    }

    /// Storage key for the scope's identity. `Global` has no key, so it
    /// stores an empty string rather than `NULL` — every real agent id,
    /// project hash, and agent×project key is non-empty, so `""` can never
    /// collide with a live key while staying a plain, always-bindable `TEXT`
    /// value for equality filtering.
    pub(crate) fn key_str(&self) -> &str {
        match self {
            IndexScope::Agent(key) | IndexScope::Project(key) | IndexScope::AgentProject(key) => {
                key
            }
            IndexScope::Global => "",
        }
    }
}
