use async_trait::async_trait;

/// Invalidates any cached composed-context state for an agent profile after
/// a mutation, so the agent's next turn recomputes its system prompt instead
/// of serving a stale cached render.
///
/// Defined in this crate (rather than alongside the concrete cache) so a
/// tool like `AgentAuthor` can hold an optional invalidator without pulling
/// in `ao-engine`, which already depends on this crate — a direct dependency
/// the other way would be circular. The production implementation wraps the
/// engine's context cache; the no-op stub below backs tool registration
/// before runtime wiring occurs and any test that doesn't care about cache
/// freshness.
#[async_trait]
pub trait AgentProfileCacheInvalidator: Send + Sync {
    /// Drop cached context keyed to `agent_id` so it is rebuilt from disk on
    /// the next lookup.
    async fn invalidate(&self, agent_id: &str);
}

/// Invalidator that does nothing. Used for the pre-wiring stub and for tests
/// that don't assert on cache invalidation.
pub struct NoopAgentProfileCacheInvalidator;

#[async_trait]
impl AgentProfileCacheInvalidator for NoopAgentProfileCacheInvalidator {
    async fn invalidate(&self, _agent_id: &str) {}
}
