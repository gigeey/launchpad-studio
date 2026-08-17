pub mod recall;
mod recall_prompt;
#[cfg(test)]
mod recall_tests;

pub use recall::RecallHistory;

use ao_engine_tools_core::Registry;
use std::sync::Arc;

pub fn register_recall_history_tool(registry: &mut Registry) {
    registry.register_io(Arc::new(RecallHistory));
}
