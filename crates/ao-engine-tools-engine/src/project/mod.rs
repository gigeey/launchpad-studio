//! Project agent tools — give the main project agent first-class control over
//! its own project record.
//!
//! These tools are only meaningful inside a project-scoped channel run
//! (i.e. when `RunnerContext::project_id` and `RunnerContext::project_store`
//! are both set). Every tool in this module returns a recoverable error if
//! either field is absent.
//!
//! # Lifecycle covered
//!
//! ```text
//! [user creates project] → status = Interviewing
//!       ↓  agent interviews user, calls ProjectUpdate(activate=true)
//!       ↓  status = Active
//!       ↓  agent drives goal via TodoCreate tasklists / Delegate calls
//!       ↓  agent calls ProjectComplete(summary=…)
//!       ↓  status = Completed
//! ```

pub mod complete;
pub mod get;
pub mod update;
pub mod verify;

pub use complete::ProjectComplete;
pub use get::ProjectGet;
pub use update::ProjectUpdate;
pub use verify::ProjectVerify;

use std::sync::Arc;

use ao_engine_tools_core::Registry;

/// Register all project agent tools into the supplied registry.
///
/// Call this alongside [`crate::register_all`] when assembling a registry for
/// a project-channel session. The tools are registered with `LoadPolicy::Deferred`
/// so they do not appear in the initial tool list; the model activates them via
/// `ToolSearch` when it needs them. Each tool also guards its own `invoke` against
/// missing project scope so a mistakenly broad `register_all` call cannot expose
/// them in non-project contexts.
pub fn register_project_tools(registry: &mut Registry) {
    registry.register_engine(Arc::new(ProjectGet));
    registry.register_engine(Arc::new(ProjectUpdate));
    registry.register_engine(Arc::new(ProjectVerify));
    registry.register_engine(Arc::new(ProjectComplete));
}

#[cfg(test)]
pub(crate) mod tests {
    use ao_persistence::{paths::DataRoot, projects::ProjectStore};
    use std::sync::Arc;

    /// Build a fresh ProjectStore backed by a temporary directory.
    /// Caller must keep the returned `TempDir` alive for the duration of the test.
    pub async fn temp_project_store() -> (tempfile::TempDir, Arc<ProjectStore>) {
        let dir = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(dir.path());
        // Create the projects subdirectory so `ProjectStore::create` can write files.
        data_root.ensure_directories().await.unwrap();
        let store = Arc::new(ProjectStore::new(data_root));
        (dir, store)
    }

    /// Build a minimal `Project` for use in tests.
    pub fn fake_project(id: &str, status: ao_protocol::project::ProjectStatus) -> ao_protocol::project::Project {
        use chrono::Utc;
        ao_protocol::project::Project {
            id: id.to_string(),
            name: "Test Project".to_string(),
            emoji: None,
            goal: "Finish the thing".to_string(),
            spec: None,
            agent_id: "agent-1".to_string(),
            working_dir: None,
            attachments: vec![],
            status,
            summary: None,
            verifications: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}
