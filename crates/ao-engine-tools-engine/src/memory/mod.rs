pub mod contradiction;
pub mod decay;
pub mod delete;
pub mod edit;
pub mod eviction;
pub mod list;
pub mod promotion;
pub mod promotion_budget;
pub mod prompt;
pub mod review;
pub mod staged_ttl;
pub mod store;
pub mod write;

#[cfg(test)]
mod tests;

pub use store::{
    agent_project_key, check_entry_caps, check_scope_caps, resolve_scope_context,
    resolve_working_dir, ScopeContext, AGENT_HARD_CAP, AGENT_SOFT_CAP, ENTRY_CHAR_HARD,
    ENTRY_CHAR_SOFT, GLOBAL_HARD_CAP, GLOBAL_SOFT_CAP, PROJECT_HARD_CAP, PROJECT_SOFT_CAP,
    THREAD_ENTRY_CHAR_HARD, THREAD_ENTRY_CHAR_SOFT, THREAD_HARD_CAP, THREAD_SOFT_CAP,
};
pub use promotion::{apply_promotion_verdict, apply_promotion_verdict_with_budget, PromotionVerdict};
pub use promotion_budget::{
    decisions_from_outcome_history, record_review_decision, PromotionBudgetController,
    PromotionBudgetGate, ReviewDecision,
};
pub use staged_ttl::{
    expired_staged_candidate_ids, sweep_expired_staged_candidates, STAGED_CANDIDATE_TTL_DAYS,
};
pub use write::write_thread_entry;

use ao_engine_tools_core::Registry;
use std::sync::Arc;

/// Register all four Memory tools into the supplied [`Registry`].
pub fn register_memory_tools(registry: &mut Registry) {
    registry.register_io(Arc::new(write::MemoryWrite));
    registry.register_io(Arc::new(edit::MemoryEdit));
    registry.register_io(Arc::new(delete::MemoryDelete));
    registry.register_io(Arc::new(list::MemoryList));
}
