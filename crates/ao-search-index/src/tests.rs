use crate::{ArtifactKind, IndexRecord, IndexScope, SearchFilter, SearchIndex};

fn record(id: &str, scope: IndexScope, artifact: ArtifactKind, text: &str) -> IndexRecord {
    IndexRecord {
        id: id.to_string(),
        scope,
        artifact,
        text: text.to_string(),
    }
}

#[tokio::test]
async fn upsert_then_query_finds_the_entry() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record(
            "m1",
            IndexScope::Agent("agent-a".into()),
            ArtifactKind::Memory,
            "the build uses cargo workspaces",
        ))
        .await
        .unwrap();

    let hits = index
        .query("cargo workspaces".into(), SearchFilter::new(), 10)
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "m1");
}

#[tokio::test]
async fn query_with_no_indexable_tokens_returns_empty() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("m1", IndexScope::Global, ArtifactKind::Memory, "hello world"))
        .await
        .unwrap();

    let hits = index.query("   ---   ".into(), SearchFilter::new(), 10).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn upsert_replaces_prior_content_for_same_id() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("m1", IndexScope::Global, ArtifactKind::Memory, "original phrasing"))
        .await
        .unwrap();
    index
        .upsert(record("m1", IndexScope::Global, ArtifactKind::Memory, "revised phrasing"))
        .await
        .unwrap();

    let old_hits = index.query("original".into(), SearchFilter::new(), 10).await.unwrap();
    assert!(old_hits.is_empty(), "stale text must not still be searchable");

    let new_hits = index.query("revised".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(new_hits.len(), 1);
    assert_eq!(new_hits[0].id, "m1");
}

#[tokio::test]
async fn delete_removes_entry_from_results() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("m1", IndexScope::Global, ArtifactKind::Memory, "ephemeral note"))
        .await
        .unwrap();
    index.delete("m1".to_string()).await.unwrap();

    let hits = index.query("ephemeral".into(), SearchFilter::new(), 10).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn delete_of_unknown_id_is_not_an_error() {
    let index = SearchIndex::open_in_memory().unwrap();
    index.delete("does-not-exist".to_string()).await.unwrap();
}

#[tokio::test]
async fn scope_filter_isolates_matching_scope() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record(
            "m1",
            IndexScope::Project("hash-a".into()),
            ArtifactKind::Memory,
            "shared build quirk",
        ))
        .await
        .unwrap();
    index
        .upsert(record(
            "m2",
            IndexScope::Project("hash-b".into()),
            ArtifactKind::Memory,
            "shared build quirk",
        ))
        .await
        .unwrap();

    let hits = index
        .query(
            "build quirk".into(),
            SearchFilter::new().with_scope(IndexScope::Project("hash-a".into())),
            10,
        )
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "m1");
}

#[tokio::test]
async fn global_scope_filter_matches_only_keyless_rows() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("global-1", IndexScope::Global, ArtifactKind::Memory, "global note"))
        .await
        .unwrap();
    index
        .upsert(record(
            "agent-1",
            IndexScope::Agent("some-agent".into()),
            ArtifactKind::Memory,
            "global note too",
        ))
        .await
        .unwrap();

    let hits = index
        .query("global note".into(), SearchFilter::new().with_scope(IndexScope::Global), 10)
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "global-1");
}

#[tokio::test]
async fn artifact_filter_separates_memory_and_skill_rows() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("mem-1", IndexScope::Global, ArtifactKind::Memory, "deploy runbook"))
        .await
        .unwrap();
    index
        .upsert(record("skill-1", IndexScope::Global, ArtifactKind::Skill, "deploy runbook"))
        .await
        .unwrap();

    let memory_hits = index
        .query("deploy runbook".into(), SearchFilter::new().with_artifact(ArtifactKind::Memory), 10)
        .await
        .unwrap();
    assert_eq!(memory_hits.len(), 1);
    assert_eq!(memory_hits[0].id, "mem-1");

    let skill_hits = index
        .query("deploy runbook".into(), SearchFilter::new().with_artifact(ArtifactKind::Skill), 10)
        .await
        .unwrap();
    assert_eq!(skill_hits.len(), 1);
    assert_eq!(skill_hits[0].id, "skill-1");

    let unfiltered_hits = index.query("deploy runbook".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(unfiltered_hits.len(), 2);
}

#[tokio::test]
async fn ranking_favors_entries_matching_more_query_terms() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record(
            "both",
            IndexScope::Global,
            ArtifactKind::Memory,
            "async runtime and blocking io both matter",
        ))
        .await
        .unwrap();
    index
        .upsert(record(
            "one",
            IndexScope::Global,
            ArtifactKind::Memory,
            "async runtime only",
        ))
        .await
        .unwrap();

    let hits = index
        .query("async blocking".into(), SearchFilter::new(), 10)
        .await
        .unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].id, "both", "entry matching both terms should rank first");
    assert!(hits[0].score >= hits[1].score, "results must be sorted best-first");
}

#[tokio::test]
async fn rebuild_replaces_the_entire_index() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("stale", IndexScope::Global, ArtifactKind::Memory, "stale entry"))
        .await
        .unwrap();
    index
        .upsert(record("stale-skill", IndexScope::Global, ArtifactKind::Skill, "stale skill"))
        .await
        .unwrap();

    index
        .rebuild(vec![record(
            "fresh",
            IndexScope::Global,
            ArtifactKind::Memory,
            "fresh entry",
        )])
        .await
        .unwrap();

    assert!(index.query("stale".into(), SearchFilter::new(), 10).await.unwrap().is_empty());
    assert!(index
        .query("stale skill".into(), SearchFilter::new(), 10)
        .await
        .unwrap()
        .is_empty());
    let hits = index.query("fresh".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "fresh");
}

#[tokio::test]
async fn rebuild_kind_only_touches_matching_artifact_rows() {
    let index = SearchIndex::open_in_memory().unwrap();
    index
        .upsert(record("mem-1", IndexScope::Global, ArtifactKind::Memory, "memory entry"))
        .await
        .unwrap();
    index
        .upsert(record("skill-1", IndexScope::Global, ArtifactKind::Skill, "old skill"))
        .await
        .unwrap();

    index
        .rebuild_kind(
            ArtifactKind::Skill,
            vec![record("skill-2", IndexScope::Global, ArtifactKind::Skill, "new skill")],
        )
        .await
        .unwrap();

    // Memory rows survive a skill-only rebuild untouched.
    let memory_hits = index.query("memory entry".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(memory_hits.len(), 1);
    assert_eq!(memory_hits[0].id, "mem-1");

    // Old skill row is gone; new skill row is present. Query terms are OR'd
    // together (see `build_match_expression`), so "old" alone is the term
    // that must disappear — "new skill" still contains "skill".
    assert!(index.query("old".into(), SearchFilter::new(), 10).await.unwrap().is_empty());
    let skill_hits = index.query("new skill".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(skill_hits.len(), 1);
    assert_eq!(skill_hits[0].id, "skill-2");
}

#[tokio::test]
async fn rebuild_kind_skips_records_of_a_different_kind() {
    let index = SearchIndex::open_in_memory().unwrap();

    // A caller-side bug passing a memory record into a skill-kind rebuild
    // must not silently index it as a skill row.
    index
        .rebuild_kind(
            ArtifactKind::Skill,
            vec![record("mem-1", IndexScope::Global, ArtifactKind::Memory, "mismatched kind")],
        )
        .await
        .unwrap();

    let hits = index
        .query(
            "mismatched kind".into(),
            SearchFilter::new().with_artifact(ArtifactKind::Skill),
            10,
        )
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn index_persists_across_reopen_at_the_same_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("index.sqlite3");

    {
        let index = SearchIndex::open(&path).unwrap();
        index
            .upsert(record("m1", IndexScope::Global, ArtifactKind::Memory, "durable entry"))
            .await
            .unwrap();
    }

    let reopened = SearchIndex::open(&path).unwrap();
    let hits = reopened.query("durable".into(), SearchFilter::new(), 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "m1");
}

#[tokio::test]
async fn is_artifact_empty_reflects_row_presence_per_kind() {
    let index = SearchIndex::open_in_memory().unwrap();
    assert!(index.is_artifact_empty(ArtifactKind::Memory).await.unwrap());
    assert!(index.is_artifact_empty(ArtifactKind::Skill).await.unwrap());

    index
        .upsert(record("mem-1", IndexScope::Global, ArtifactKind::Memory, "a memory row"))
        .await
        .unwrap();

    assert!(!index.is_artifact_empty(ArtifactKind::Memory).await.unwrap());
    assert!(
        index.is_artifact_empty(ArtifactKind::Skill).await.unwrap(),
        "a Memory row must not count toward Skill's emptiness"
    );
}

#[tokio::test]
async fn limit_caps_the_number_of_hits() {
    let index = SearchIndex::open_in_memory().unwrap();
    for i in 0..5 {
        index
            .upsert(record(
                &format!("m{i}"),
                IndexScope::Global,
                ArtifactKind::Memory,
                "repeated term",
            ))
            .await
            .unwrap();
    }

    let hits = index.query("repeated".into(), SearchFilter::new(), 2).await.unwrap();
    assert_eq!(hits.len(), 2);
}
