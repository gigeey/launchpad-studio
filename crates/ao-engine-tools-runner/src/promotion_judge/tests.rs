use super::*;
use crate::provider::{MockProviderClient, StopReason};

fn turn(text: &str) -> Vec<CompletionEvent> {
    vec![
        CompletionEvent::AssistantText(text.to_string()),
        CompletionEvent::TurnComplete {
            stop_reason: StopReason::Natural,
        },
    ]
}

// --- parse_verdict robustness --------------------------------------------

#[test]
fn parses_a_promote_verdict_with_generalized_content() {
    let v = parse_verdict(
        r#"{"verdict":"promote","generalized_content":"Prefer tabs over spaces.","rationale":"a stated durable preference"}"#,
    )
    .unwrap();
    match v {
        PromotionVerdict::Promote { generalized_content, rationale } => {
            assert_eq!(generalized_content, "Prefer tabs over spaces.");
            assert_eq!(rationale, "a stated durable preference");
        }
        PromotionVerdict::Reject { .. } => panic!("expected Promote"),
    }
}

#[test]
fn parses_a_reject_verdict() {
    let v = parse_verdict(
        r#"{"verdict":"reject","rationale":"specific to this thread's one-off file"}"#,
    )
    .unwrap();
    match v {
        PromotionVerdict::Reject { rationale } => {
            assert_eq!(rationale, "specific to this thread's one-off file");
        }
        PromotionVerdict::Promote { .. } => panic!("expected Reject"),
    }
}

#[test]
fn parses_fenced_json() {
    let text = concat!(
        "Here is my verdict:\n```json\n",
        r#"{"verdict":"promote","generalized_content":"General rule.","rationale":"generalizes"}"#,
        "\n```\n"
    );
    let v = parse_verdict(text).unwrap();
    assert!(matches!(v, PromotionVerdict::Promote { .. }));
}

#[test]
fn parses_json_with_prose_preamble_and_trailer() {
    let text = concat!(
        "Looking at this note...\n\n",
        r#"{"verdict":"reject","rationale":"one-off detail"}"#,
        "\n\nLet me know if you need more."
    );
    let v = parse_verdict(text).unwrap();
    assert!(matches!(v, PromotionVerdict::Reject { .. }));
}

#[test]
fn promote_verdict_missing_generalized_content_fails_to_parse() {
    let err = parse_verdict(r#"{"verdict":"promote","rationale":"no content given"}"#).unwrap_err();
    assert!(err.contains("generalized_content"));
}

#[test]
fn promote_verdict_with_blank_generalized_content_fails_to_parse() {
    let err = parse_verdict(
        r#"{"verdict":"promote","generalized_content":"   ","rationale":"blank"}"#,
    )
    .unwrap_err();
    assert!(err.contains("generalized_content"));
}

#[test]
fn rejects_text_with_no_verdict_object() {
    assert!(parse_verdict("I could not reach a conclusion.").is_err());
}

#[test]
fn rejects_invalid_verdict_value() {
    let err = parse_verdict(r#"{"verdict":"maybe","rationale":"unsure"}"#).unwrap_err();
    assert!(err.contains("verdict"));
}

// --- ProviderPromotionJudge resilience -----------------------------------

#[tokio::test]
async fn judge_promotes_a_generalizable_note_via_the_injected_provider_only() {
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Always write tests before merging.","rationale":"a recurring, generally useful convention"}"#,
    )]));
    let judge = ProviderPromotionJudge::new(provider.clone());

    let verdict = judge
        .judge("In this thread we agreed: always write tests before merging.")
        .await
        .unwrap();

    match verdict {
        PromotionVerdict::Promote { generalized_content, .. } => {
            assert_eq!(generalized_content, "Always write tests before merging.");
        }
        PromotionVerdict::Reject { rationale } => panic!("expected Promote, got Reject: {rationale}"),
    }
    // Exactly one call was made — proving the judge is driven only through
    // the injected provider, with no retry and no second client anywhere.
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn judge_rejects_a_thread_specific_note() {
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"reject","rationale":"only meaningful in light of this thread's specific file"}"#,
    )]));
    let judge = ProviderPromotionJudge::new(provider.clone());

    let verdict = judge
        .judge("Remember that frontend/src/components/Foo.tsx:42 has the bug.")
        .await
        .unwrap();

    match verdict {
        PromotionVerdict::Reject { rationale } => {
            assert!(rationale.contains("this thread"));
        }
        PromotionVerdict::Promote { .. } => panic!("expected Reject"),
    }
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn unparseable_first_reply_retries_once_then_succeeds() {
    let good = r#"{"verdict":"promote","generalized_content":"General convention.","rationale":"clearly recurring"}"#;
    let provider = Arc::new(MockProviderClient::new(vec![
        turn("I'm not sure how to answer that."),
        turn(good),
    ]));
    let judge = ProviderPromotionJudge::new(provider.clone());

    let verdict = judge.judge("some note").await.unwrap();
    assert!(matches!(verdict, PromotionVerdict::Promote { .. }));
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn both_attempts_unparseable_degrades_to_a_safe_reject() {
    let provider = Arc::new(MockProviderClient::new(vec![
        turn("no json here"),
        turn("still no json"),
    ]));
    let judge = ProviderPromotionJudge::new(provider);

    let verdict = judge.judge("some note").await.unwrap();
    match verdict {
        PromotionVerdict::Reject { rationale } => {
            assert!(rationale.contains("defaulting to reject"));
        }
        PromotionVerdict::Promote { .. } => panic!("an unparseable reply must never promote"),
    }
}

#[tokio::test]
async fn provider_error_degrades_to_a_safe_reject_instead_of_propagating() {
    // Empty script -> ScriptExhausted on the first complete() call.
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let judge = ProviderPromotionJudge::new(provider);

    let verdict = judge.judge("some note").await.unwrap();
    match verdict {
        PromotionVerdict::Reject { rationale } => {
            assert!(rationale.contains("defaulting to reject"));
        }
        PromotionVerdict::Promote { .. } => panic!("a provider failure must never promote"),
    }
}

// --- end-to-end: judge verdict -> ReflectionStagingStore -----------------
//
// Proves the full contract in one pass: the judge is driven only through
// an injected `Arc<dyn ProviderClient>` (no bespoke HTTP/model client is
// constructed anywhere in this crate's new code), and its verdict lands in
// the SAME `ReflectionStagingStore` every other reflected candidate uses —
// never a parallel store — tagged as a `memory` artifact.

fn staging_store(tmp: &tempfile::TempDir) -> ao_persistence::reflection_staging::ReflectionStagingStore {
    ao_persistence::reflection_staging::ReflectionStagingStore::new(
        ao_persistence::paths::DataRoot::new(tmp.path()),
    )
}

#[tokio::test]
async fn a_generalizable_thread_entry_is_promoted_all_the_way_into_the_staging_store() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Prefer tabs over spaces for indentation.","rationale":"stated as a durable, cross-project preference"}"#,
    )]));
    let judge = ProviderPromotionJudge::new(provider.clone());

    let verdict = judge
        .judge("In this thread the user said they like tabs, not spaces, for this file.")
        .await
        .unwrap();

    let staged = ao_engine_tools_engine::memory::promotion::apply_promotion_verdict(
        &staging, "agent-1", "thread-1", verdict, &[],
    )
    .await
    .unwrap();

    assert!(staged.is_some(), "a Promote verdict must stage a candidate");
    assert_eq!(provider.remaining_turns(), 0);

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content, "Prefer tabs over spaces for indentation.");
    assert_eq!(pending[0].kind, ao_protocol::outcome::ArtifactKind::Memory);
    assert_eq!(pending[0].source_thread_id, "thread-1");
}

#[tokio::test]
async fn a_thread_specific_entry_is_rejected_and_never_reaches_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"reject","rationale":"only relevant to this one conversation's specific ticket"}"#,
    )]));
    let judge = ProviderPromotionJudge::new(provider.clone());

    let verdict = judge
        .judge("Remember ticket ABC-123 needed a manual DB fix in this thread.")
        .await
        .unwrap();

    let staged = ao_engine_tools_engine::memory::promotion::apply_promotion_verdict(
        &staging, "agent-1", "thread-1", verdict, &[],
    )
    .await
    .unwrap();

    assert!(staged.is_none(), "a Reject verdict must not stage a candidate");
    assert_eq!(provider.remaining_turns(), 0);

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert!(pending.is_empty(), "a rejected entry must never reach the review queue");
}
