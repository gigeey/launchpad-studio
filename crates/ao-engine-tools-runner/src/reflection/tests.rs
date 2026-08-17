use std::sync::Arc;

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

// --- parse_proposals robustness -----------------------------------------

#[test]
fn parses_plain_json_array() {
    let proposals = parse_proposals(
        r#"[{"kind":"memory","content":"User prefers tabs."},{"kind":"skill","content":"Run tests via cargo nextest."}]"#,
    );
    assert_eq!(proposals.len(), 2);
    assert_eq!(proposals[0].kind, ArtifactKind::Memory);
    assert_eq!(proposals[0].content, "User prefers tabs.");
    assert_eq!(proposals[1].kind, ArtifactKind::Skill);
}

#[test]
fn parses_empty_array_as_no_candidates() {
    assert!(parse_proposals("[]").is_empty());
}

#[test]
fn parses_fenced_json_array() {
    let text = concat!(
        "Here you go:\n```json\n",
        r#"[{"kind":"memory","content":"Deploys use blue/green."}]"#,
        "\n```\nLet me know if you need more."
    );
    let proposals = parse_proposals(text);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].content, "Deploys use blue/green.");
}

#[test]
fn parses_array_wrapped_in_prose() {
    let text = concat!(
        "Based on the transcript, here is what stood out:\n\n",
        r#"[{"kind":"memory","content":"Prefers dark mode."}]"#,
        "\n\nHappy to elaborate."
    );
    let proposals = parse_proposals(text);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].content, "Prefers dark mode.");
}

#[test]
fn unparseable_text_yields_no_candidates_not_an_error() {
    assert!(parse_proposals("I could not find anything durable here.").is_empty());
}

#[test]
fn unknown_kind_is_skipped_but_valid_siblings_survive() {
    let proposals = parse_proposals(
        r#"[{"kind":"workflow","content":"not a real kind yet"},{"kind":"memory","content":"kept"}]"#,
    );
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].content, "kept");
}

#[test]
fn empty_content_is_skipped() {
    let proposals = parse_proposals(r#"[{"kind":"memory","content":"   "}]"#);
    assert!(proposals.is_empty());
}

// --- confidence parsing --------------------------------------------------

#[test]
fn parses_explicit_confidence() {
    let proposals = parse_proposals(
        r#"[{"kind":"memory","content":"Specific to this thread.","confidence":0.2}]"#,
    );
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].confidence, 0.2);
}

#[test]
fn missing_confidence_defaults_to_the_documented_default() {
    let proposals = parse_proposals(r#"[{"kind":"memory","content":"No confidence given."}]"#);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].confidence, DEFAULT_PROPOSAL_CONFIDENCE);
}

#[test]
fn out_of_range_confidence_is_clamped_into_0_to_1() {
    let proposals = parse_proposals(
        r#"[{"kind":"memory","content":"Too high.","confidence":5.0},{"kind":"memory","content":"Too low.","confidence":-2.0}]"#,
    );
    assert_eq!(proposals.len(), 2);
    assert_eq!(proposals[0].confidence, 1.0);
    assert_eq!(proposals[1].confidence, 0.0);
}

#[test]
fn non_numeric_confidence_falls_back_to_the_default() {
    let proposals = parse_proposals(
        r#"[{"kind":"memory","content":"Confidence is a string.","confidence":"high"}]"#,
    );
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].confidence, DEFAULT_PROPOSAL_CONFIDENCE);
}

#[test]
fn candidate_count_is_capped() {
    let items: Vec<String> = (0..(MAX_CANDIDATES_PER_PASS + 5))
        .map(|i| format!(r#"{{"kind":"memory","content":"item {i}"}}"#))
        .collect();
    let text = format!("[{}]", items.join(","));
    let proposals = parse_proposals(&text);
    assert_eq!(proposals.len(), MAX_CANDIDATES_PER_PASS);
}

// --- ProviderReflectionProposer -----------------------------------------

#[tokio::test]
async fn proposer_drives_the_injected_provider_and_parses_its_reply() {
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"User is on the free tier."}]"#,
    )]));
    let proposer = ProviderReflectionProposer::new(provider.clone());

    let proposals = proposer.propose("user: I'm on the free plan").await.unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].content, "User is on the free tier.");
    // Exactly one call was made against the injected provider.
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn proposer_returns_empty_vec_for_a_reply_with_no_findings() {
    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let proposer = ProviderReflectionProposer::new(provider);

    let proposals = proposer.propose("nothing notable happened").await.unwrap();
    assert!(proposals.is_empty());
}

#[tokio::test]
async fn proposer_propagates_a_hard_provider_error() {
    // Empty script -> ScriptExhausted on the first complete() call.
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let proposer = ProviderReflectionProposer::new(provider);

    let err = proposer.propose("delta text").await.unwrap_err();
    assert!(err.contains("reflection"));
}

#[tokio::test]
async fn proposer_engine_trait_object_is_usable_via_dyn_dispatch() {
    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let engine: Arc<dyn ReflectionProposalEngine> =
        Arc::new(ProviderReflectionProposer::new(provider));
    let proposals = engine.propose("delta text").await.unwrap();
    assert!(proposals.is_empty());
}

// --- parse_generalized_skill robustness ----------------------------------

#[test]
fn parses_plain_json_object() {
    let skill = parse_generalized_skill(
        r#"{"name":"build-verify-fix","description":"Build, test, fix.","body":"Run cargo build, then cargo test, fix any failures, repeat."}"#,
    )
    .unwrap();
    assert_eq!(skill.name, "build-verify-fix");
    assert_eq!(skill.description, "Build, test, fix.");
    assert!(skill.body.contains("cargo build"));
}

#[test]
fn parses_fenced_json_object() {
    let text = concat!(
        "Here is the template:\n```json\n",
        r#"{"name":"deploy-flow","description":"Deploy procedure.","body":"Do the deploy steps."}"#,
        "\n```\n"
    );
    let skill = parse_generalized_skill(text).unwrap();
    assert_eq!(skill.name, "deploy-flow");
}

#[test]
fn parses_object_wrapped_in_prose() {
    let text = concat!(
        "Sure, here's a generalized version:\n\n",
        r#"{"name":"review-loop","description":"Review procedure.","body":"Read, comment, iterate."}"#,
        "\n\nLet me know if that helps."
    );
    let skill = parse_generalized_skill(text).unwrap();
    assert_eq!(skill.name, "review-loop");
}

#[test]
fn missing_field_yields_none() {
    assert!(parse_generalized_skill(r#"{"name":"x","description":"y"}"#).is_none());
}

#[test]
fn empty_field_yields_none() {
    assert!(parse_generalized_skill(r#"{"name":"","description":"y","body":"z"}"#).is_none());
}

#[test]
fn unparseable_text_yields_none_not_a_panic() {
    assert!(parse_generalized_skill("I could not find a pattern here.").is_none());
}

// --- ProviderSkillGeneralizer ---------------------------------------------

#[tokio::test]
async fn generalizer_drives_the_injected_provider_and_parses_its_reply() {
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"name":"build-verify-fix","description":"Build, test, fix loop.","body":"Run the build, run tests, fix failures, repeat until green."}"#,
    )]));
    let generalizer = ProviderSkillGeneralizer::new(provider.clone());

    let observations = vec![
        "Ran cargo build, then cargo test, fixed a lifetime error, reran tests.".to_string(),
        "Ran cargo build, then cargo test, fixed a missing import, reran tests.".to_string(),
    ];
    let skill = generalizer.generalize(&observations).await.unwrap();
    assert_eq!(skill.name, "build-verify-fix");
    // Exactly one call was made against the injected provider.
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn generalizer_rejects_zero_observations_without_calling_the_provider() {
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let generalizer = ProviderSkillGeneralizer::new(provider.clone());

    let err = generalizer.generalize(&[]).await.unwrap_err();
    assert!(err.contains("zero observations"));
    assert_eq!(provider.remaining_turns(), 0, "an empty script must be left untouched");
}

#[tokio::test]
async fn generalizer_propagates_a_hard_provider_error() {
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let generalizer = ProviderSkillGeneralizer::new(provider);

    let err = generalizer
        .generalize(&["some observation".to_string()])
        .await
        .unwrap_err();
    assert!(err.contains("generalization"));
}

#[tokio::test]
async fn generalizer_engine_trait_object_is_usable_via_dyn_dispatch() {
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"name":"n","description":"d","body":"b"}"#,
    )]));
    let engine: Arc<dyn SkillGeneralizationEngine> =
        Arc::new(ProviderSkillGeneralizer::new(provider));
    let skill = engine.generalize(&["one observation".to_string()]).await.unwrap();
    assert_eq!(skill.name, "n");
}
