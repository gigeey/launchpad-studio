//! Local, offline SQLite FTS5 full-text index shared by the memory store and
//! the skill registry.
//!
//! There is deliberately no vector store and no embedding model here —
//! retrieval over these stores is local keyword search over a small,
//! curated, per-scope-capped corpus, not similarity search over an
//! unbounded one. See [`SearchIndex`] for the read/write API.

mod index;
mod query;
mod record;
mod scope;

#[cfg(test)]
mod tests;

pub use index::{SearchIndex, SEARCH_INDEX_FILENAME};
pub use record::{IndexRecord, SearchFilter, SearchHit};
pub use scope::{ArtifactKind, IndexScope};
