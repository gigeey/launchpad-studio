use thiserror::Error;

#[derive(Debug, Error)]
pub enum AoError {
    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Thread not found: {0}")]
    ThreadNotFound(String),

    #[error("Agent already exists: {0}")]
    AgentAlreadyExists(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(String),

    #[error("JSON error: {0}")]
    Json(String),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Delegation error: {0}")]
    DelegationError(String),

    #[error("Attachment not found: {0}")]
    AttachmentNotFound(String),

    #[error("Validation error: {0}")]
    ValidationError(String),

    #[error("Workflow not found: {0}")]
    WorkflowNotFound(String),

    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Skill not found: {0}")]
    SkillNotFound(String),

    #[error("Rule not found: {0}")]
    RuleNotFound(String),

    #[error("Instruction not found: {0}")]
    InstructionNotFound(String),

    #[error("Tasklist not found: {0}")]
    TasklistNotFound(String),

    #[error("Team {team_id} already has an active tasklist: {tasklist_id}")]
    TasklistAlreadyActive {
        team_id: String,
        tasklist_id: String,
    },

    #[error("Invalid tasklist transition: {0}")]
    InvalidTasklistTransition(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Project already exists: {0}")]
    ProjectAlreadyExists(String),

    #[error("Assignment not found: {0}")]
    AssignmentNotFound(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Search index error: {0}")]
    SearchIndex(String),

    #[error("Memory entry not found: {0}")]
    MemoryNotFound(String),

    #[error("Artifact not found: {0}")]
    ArtifactNotFound(String),

    #[error("Artifact group not found: {0}")]
    ArtifactGroupNotFound(String),

    #[error("Delegation not found: {0}")]
    DelegationNotFound(String),

    #[error("Workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// The on-disk workspace registry (`workspaces.json`) exists but failed
    /// to parse. Deliberately distinct from a missing file, which is a
    /// normal first-run state handled by returning a default registry — see
    /// `ao_protocol::workspaces::load_registry`. Every mutation route that
    /// reads the registry via `load_registry` propagates this instead of
    /// falling back to a synthetic default, because doing the latter and
    /// then saving would silently overwrite whatever real data is still in
    /// the file with an empty registry.
    #[error("Workspace registry corrupt: {0}")]
    WorkspaceRegistryCorrupt(String),

    /// A workspace-registry mutation route (`create_workspace`,
    /// `rename_workspace`, `delete_workspace`, `activate_workspace`,
    /// `duplicate_workspace` in `ao-server`) refused because this process's
    /// active data root is pinned via `LAUNCHPAD_STUDIO_DATA_DIR`
    /// (`RootProvenance::EnvOverride`). The registry file lives at a fixed
    /// path outside any data root (see `workspaces::registry_path`), so a
    /// pinned track and an unpinned track share the exact same file — this
    /// is the server-side enforcement of the rule the frontend already
    /// applies client-side by disabling the workspace switcher whenever
    /// `provenance === "env_override"`.
    ///
    /// `env_var` and `value` are carried as structured fields, separate
    /// from the fixed user-facing sentence the HTTP layer reports for this
    /// variant (see `ao-server/src/error.rs`), so a client can show the raw
    /// diagnostic without it being folded into that sentence.
    #[error("Workspace mutation blocked: data root is pinned via {env_var}={value}")]
    WorkspaceMutationBlockedByPinnedDataRoot { env_var: String, value: String },

    /// `POST /workspaces/{id}/activate`'s pre-flight probe (see
    /// `ao_server::routes::workspaces::probe_target_data_root`) failed to
    /// open the target workspace's data root — the same initialization
    /// `ao_persistence::PersistenceLayer::init_with_root` runs at process
    /// startup, run early enough that a failure here refuses the registry
    /// mutation instead of persisting an `active` pointer that would crash
    /// the app on the restart activation triggers, and identically on every
    /// subsequent launch since the pointer was already saved. `path` and
    /// `cause` are carried as structured fields, separate from the fixed
    /// sentence in this variant's `Display` impl, so a client can report
    /// exactly which target failed and why.
    #[error("Workspace data root could not be opened: {path} ({cause})")]
    WorkspaceActivationTargetUnopenable { path: String, cause: String },

    /// A configured provider rejected the stored API key (HTTP 401/403) when
    /// queried for `GET /providers/{name}/models`. Kept distinct from the
    /// generic [`Self::Provider`] (which covers agent-turn failures) so the
    /// HTTP layer can map it to a structured, frontend-distinguishable
    /// response — see `crates/ao-server/src/error.rs`.
    #[error("Provider auth failure: {0}")]
    ProviderAuthFailure(String),

    /// A transport-level or non-auth non-2xx failure talking to a
    /// configured provider's API during `GET /providers/{name}/models`.
    #[error("Provider network failure: {0}")]
    ProviderNetworkFailure(String),

    /// A configured provider returned a 2xx response that didn't parse as
    /// the expected shape during `GET /providers/{name}/models`.
    #[error("Provider malformed response: {0}")]
    ProviderMalformedResponse(String),
}
