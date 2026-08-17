/// Controls whether a tool is included in every LLM request's tools array
/// (AlwaysLoad) or advertised only by name and loaded on demand (Deferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPolicy {
    /// Include this tool's full schema in every LLM request.
    AlwaysLoad,
    /// Advertise this tool by name only; schema is loaded via ToolSearch.
    Deferred,
}

/// Per-user override that can promote or demote a tool from its default policy.
/// Stored in settings.json and applied at session startup when computing the
/// resolved loaded set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPolicyOverride {
    /// Promote a Deferred tool to always-loaded for this user/project.
    ForceAlwaysLoad,
    /// Demote an AlwaysLoad tool to deferred for this user/project.
    ForceDeferred,
}
