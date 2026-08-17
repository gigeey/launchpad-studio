use async_trait::async_trait;

use ao_protocol::assignment::{Assignment, AssignmentRun};
use ao_protocol::error::AoError;

/// Trait abstraction over `ao_engine::assignment_runner::fire_assignment` that
/// lets `ao-engine-tools-core` (and the tools built on top of it) trigger an
/// immediate assignment run without introducing a circular crate dependency.
///
/// `ao-engine` already depends on `ao-engine-tools-core` for `RunnerContext`,
/// so `ao-engine-tools-core` cannot in turn depend on `ao-engine`. This trait
/// is defined here; `ao-engine` implements it on a concrete handle that closes
/// over the persistence layer, queue dispatcher, and event bus the shared
/// `fire_assignment` helper needs. The `AssignmentTrigger` tool calls through
/// this surface, so a manual fire-now goes through the exact same seam as the
/// cron tick and the inbound webhook route — no logic is duplicated.
#[async_trait]
pub trait AssignmentFireHandle: Send + Sync {
    /// Fire the given assignment immediately, as if triggered by hand rather
    /// than by its configured trigger. Resolves the run's destination thread
    /// per the assignment's `thread_policy`, records a new `AssignmentRun`
    /// row, and enqueues the dispatch. `timezone` is the caller's IANA
    /// timezone string (best-effort; only relevant for cron bookkeeping that
    /// a manual fire doesn't touch).
    async fn fire_now(
        &self,
        assignment: &Assignment,
        timezone: Option<&str>,
    ) -> Result<AssignmentRun, AoError>;
}
