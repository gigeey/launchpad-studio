use crate::scope::{ArtifactKind, IndexScope};

/// A unit of searchable text to upsert into the index.
///
/// `id` is the entry's identity in its owning store (a memory entry's uuid,
/// or a skill's canonical name) — upserting a record replaces any prior row
/// with the same `id`, regardless of what `scope`/`artifact`/`text` it had.
#[derive(Debug, Clone)]
pub struct IndexRecord {
    pub id: String,
    pub scope: IndexScope,
    pub artifact: ArtifactKind,
    pub text: String,
}

/// One ranked query result.
///
/// `score` is oriented so **higher is more relevant** (the inverse of
/// SQLite FTS5's raw `bm25()`, which ranks best matches most negative) —
/// see [`crate::SearchIndex::query`].
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub score: f64,
}

/// Optional narrowing applied to a [`crate::SearchIndex::query`] call.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub scope: Option<IndexScope>,
    pub artifact: Option<ArtifactKind>,
}

impl SearchFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_scope(mut self, scope: IndexScope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn with_artifact(mut self, artifact: ArtifactKind) -> Self {
        self.artifact = Some(artifact);
        self
    }
}
