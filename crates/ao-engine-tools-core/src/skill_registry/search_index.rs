use ao_protocol::error::AoError;
use ao_search_index::{ArtifactKind, IndexRecord, IndexScope, SearchIndex};

use super::{SkillEntry, SkillRegistry};

/// Build search-index records for every successfully loaded skill in
/// `registry`. Entries that failed to parse ([`SkillEntry::Err`]) are
/// skipped — they already can't be invoked (see [`SkillRegistry::load`]), so
/// there is nothing useful to surface from a search.
///
/// Every record indexes under [`IndexScope::Global`]: unlike memory, skills
/// aren't partitioned per agent or project on disk — the user pool and each
/// plugin pool are single shared directories under the data root, and
/// per-agent visibility is an `AgentProfile.skills` allowlist layered on top
/// of that shared storage, not a separate copy. That visibility check is a
/// retrieval-time concern for the consumer of a search hit, not something
/// this index needs to encode.
pub fn skill_index_records(registry: &SkillRegistry) -> Vec<IndexRecord> {
    registry
        .all_visible()
        .filter_map(|(name, entry)| {
            let SkillEntry::Ok(record) = entry else {
                return None;
            };
            let mut text = format!("{}\n{}", record.name, record.description);
            if let Some(hint) = &record.when_to_use {
                text.push('\n');
                text.push_str(hint);
            }
            Some(IndexRecord {
                id: name.to_string(),
                scope: IndexScope::Global,
                artifact: ArtifactKind::Skill,
                text,
            })
        })
        .collect()
}

/// Resync the search index's skill rows against the live `registry` state.
///
/// Skills have no append-only log to replay incrementally — a
/// [`SkillRegistry`] is always a fresh scan of the on-disk pools (see
/// [`SkillRegistry::load`]) rather than a persisted store with its own
/// write path — so keeping the index consistent is a full resync of the
/// `Skill` artifact rows via [`SearchIndex::rebuild_kind`] rather than a
/// per-write upsert. Safe to call after any registration/edit, and doubles
/// as the cold-start / corruption recovery path for skill entries.
pub async fn reindex_skills(index: &SearchIndex, registry: &SkillRegistry) -> Result<(), AoError> {
    index.rebuild_kind(ArtifactKind::Skill, skill_index_records(registry)).await
}
