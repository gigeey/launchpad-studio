use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use ao_engine_tools_core::{
    AskQuestionError, EngineTool, FormBridge, FormRequest, FormResponse, FormAnswer, Registry,
    RunnerContext, ToolOutput,
};
use async_trait::async_trait;
use serde_json::json;

use super::AskUserQuestionWithForm;

struct FixedFormBridge {
    result: Mutex<Option<Result<FormResponse, AskQuestionError>>>,
}

impl FixedFormBridge {
    fn returning(result: Result<FormResponse, AskQuestionError>) -> Self {
        Self {
            result: Mutex::new(Some(result)),
        }
    }
}

#[async_trait]
impl FormBridge for FixedFormBridge {
    async fn ask_form(&self, request: FormRequest) -> Result<FormResponse, AskQuestionError> {
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Err(AskQuestionError::Cancelled))
            .map(|mut r| {
                // Reflect back the id the bridge would have minted.
                if r.form_id.is_empty() {
                    r.form_id = if request.id.is_empty() {
                        "test-form-id".to_string()
                    } else {
                        request.id.clone()
                    };
                }
                r
            })
    }
}

fn text_response(form_id: &str, field_id: &str, value: &str) -> FormResponse {
    let mut answers = HashMap::new();
    answers.insert(field_id.to_string(), FormAnswer::Text(value.to_string()));
    FormResponse {
        form_id: form_id.to_string(),
        answers,
        ..Default::default()
    }
}

fn ctx_with_bridge(
    bridge: std::sync::Arc<dyn FormBridge + Send + Sync>,
) -> RunnerContext {
    let mut r = Registry::new();
    r.register_engine(std::sync::Arc::new(AskUserQuestionWithForm));
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_registry(std::sync::Arc::new(r))
        .with_form_bridge(bridge)
}

fn default_ctx() -> RunnerContext {
    ctx_with_bridge(std::sync::Arc::new(FixedFormBridge::returning(Ok(
        text_response("test-form-id", "name", "Alice"),
    ))))
}

/// `ask_form` that never resolves — the bridge-side stand-in for a genuinely
/// abandoned operator. Used to prove the deadline branch, not the bridge,
/// is what ends the wait.
struct PendingForeverFormBridge;

#[async_trait]
impl FormBridge for PendingForeverFormBridge {
    async fn ask_form(&self, _request: FormRequest) -> Result<FormResponse, AskQuestionError> {
        std::future::pending().await
    }
}

fn sample_request() -> FormRequest {
    FormRequest {
        id: String::new(),
        agent_id: "agent".to_string(),
        session_id: "sess".to_string(),
        title: "Q".to_string(),
        intro: None,
        fields: vec![],
    }
}

// ── Schema validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_title_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({ "questions": [{"id":"q1","type":"text","label":"Name?"}] }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn empty_title_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({ "title": "  ", "questions": [{"id":"q1","type":"text","label":"Name?"}] }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn missing_questions_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(json!({ "title": "My Form" }), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn empty_questions_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(json!({ "title": "My Form", "questions": [] }), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn nine_questions_is_rejected() {
    let ctx = default_ctx();
    let questions: Vec<serde_json::Value> = (0..9)
        .map(|i| json!({"id": format!("q{i}"), "type": "text", "label": format!("Q{i}")}))
        .collect();
    let out = AskUserQuestionWithForm
        .invoke(json!({ "title": "T", "questions": questions }), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn checkbox_without_options_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Pick one",
                "questions": [{"id": "c", "type": "checkbox", "label": "Choose"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn radio_with_one_option_is_accepted() {
    // A single-option radio is the confirm-style prompt the retired
    // single-question tool used to cover; it must remain expressible.
    let mut answers = HashMap::new();
    answers.insert(
        "r".to_string(),
        FormAnswer::Selections(vec!["a".to_string()]),
    );
    let ctx = ctx_with_bridge(std::sync::Arc::new(FixedFormBridge::returning(Ok(
        FormResponse {
            form_id: "test-form-id".to_string(),
            answers,
            ..Default::default()
        },
    ))));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Confirm",
                "questions": [{
                    "id": "r",
                    "type": "radio",
                    "label": "Proceed?",
                    "options": [{"id": "a", "label": "OK"}]
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["r"]["kind"], "selections");
            assert_eq!(v["answers"]["r"]["values"][0], "a");
        }
        other => panic!("expected structured output, got {other:?}"),
    }
}

#[tokio::test]
async fn radio_with_empty_options_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Pick",
                "questions": [{
                    "id": "r",
                    "type": "radio",
                    "label": "Which?",
                    "options": []
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn unknown_field_type_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "T",
                "questions": [{"id": "x", "type": "color", "label": "Pick a color"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

// ── Happy paths ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn text_field_returns_structured_answer() {
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid",
        "name",
        "Alice",
    ))));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Tell me about yourself",
                "questions": [{"id": "name", "type": "text", "label": "Your name?"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["name"]["kind"], "text");
            assert_eq!(v["answers"]["name"]["value"], "Alice");
        }
        _ => panic!("expected Structured, got {out:?}"),
    }
}

#[tokio::test]
async fn checkbox_field_returns_selections() {
    let mut answers = HashMap::new();
    answers.insert(
        "langs".to_string(),
        FormAnswer::Selections(vec!["rust".to_string(), "go".to_string()]),
    );
    let response = FormResponse {
        form_id: "f1".to_string(),
        answers,
        ..Default::default()
    };
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(response)));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Languages",
                "questions": [{
                    "id": "langs",
                    "type": "checkbox",
                    "label": "Which languages?",
                    "options": [
                        {"id": "rust", "label": "Rust"},
                        {"id": "go", "label": "Go"},
                        {"id": "py", "label": "Python"}
                    ]
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            let vals = v["answers"]["langs"]["values"].as_array().unwrap();
            assert!(vals.iter().any(|v| v == "rust"));
            assert!(vals.iter().any(|v| v == "go"));
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn file_field_returns_attachment_ids() {
    let mut answers = HashMap::new();
    answers.insert(
        "doc".to_string(),
        FormAnswer::Files(vec!["att-abc123".to_string()]),
    );
    let response = FormResponse {
        form_id: "f2".to_string(),
        answers,
        ..Default::default()
    };
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(response)));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Upload",
                "questions": [{"id": "doc", "type": "file", "label": "Attach document"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            let ids = v["answers"]["doc"]["attachment_ids"].as_array().unwrap();
            assert_eq!(ids[0], "att-abc123");
        }
        _ => panic!("expected Structured"),
    }
}

// ── Action buttons (Cancel / Regenerate / Something else) ─────────────────────

#[tokio::test]
async fn cancel_action_is_surfaced_without_answers() {
    let response = FormResponse {
        form_id: "f3".to_string(),
        action: Some(ao_engine_tools_core::FormAction::Cancel),
        ..Default::default()
    };
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(response)));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["action"], "cancel");
            assert!(v.get("answers").is_none(), "action response must not carry an answers map");
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn regenerate_action_carries_note() {
    let response = FormResponse {
        form_id: "f4".to_string(),
        action: Some(ao_engine_tools_core::FormAction::Regenerate),
        note: Some("wrong questions".to_string()),
        ..Default::default()
    };
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(response)));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["action"], "regenerate");
            assert_eq!(v["note"], "wrong questions");
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn other_action_is_surfaced() {
    let response = FormResponse {
        form_id: "f5".to_string(),
        action: Some(ao_engine_tools_core::FormAction::Other),
        ..Default::default()
    };
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(response)));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => assert_eq!(v["action"], "other"),
        _ => panic!("expected Structured"),
    }
}

// ── Error paths ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_operator_returns_recoverable_error() {
    // NoopFormBridge (built into RunnerContext default) always returns NoOperator.
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn cancelled_returns_non_recoverable_error() {
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Err(
        AskQuestionError::Cancelled,
    )));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert_eq!(message, "cancelled");
            assert!(!recoverable);
        }
        _ => panic!("expected Error"),
    }
}

// ── Async mode ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn async_mode_returns_posted_without_blocking() {
    // Uses NoopFormBridge (always NoOperator) — async must NOT touch the bridge.
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Tell us about yourself",
                "mode": "async",
                "questions": [{"id": "bio", "type": "textarea", "label": "Short bio"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["posted"], true);
            assert!(v["form_id"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(v["spec"]["form_id"].as_str().is_some_and(|s| !s.is_empty()));
            assert_eq!(v["spec"]["title"], "Tell us about yourself");
        }
        _ => panic!("expected Structured, got {out:?}"),
    }
}

#[tokio::test]
async fn async_spec_fields_use_flat_wire_shape() {
    // Regression guard: the async `spec` must serialize fields in the same flat
    // shape as the sync FormRequest path — a string `kind` discriminant with
    // `options`/`placeholder` hoisted to the top level. Emitting the raw
    // tagged-enum (`kind: { type, options }`) makes the renderer drop every input
    // control, leaving only labels.
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Pick one",
                "mode": "async",
                "questions": [{
                    "id": "choice",
                    "type": "radio",
                    "label": "Choose",
                    "required": true,
                    "options": [
                        {"id": "a", "label": "Option A"},
                        {"id": "b", "label": "Option B"}
                    ]
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            let field = &v["spec"]["fields"][0];
            // `kind` is the string discriminant, NOT a nested object.
            assert_eq!(field["kind"], "radio");
            assert!(
                !field["kind"].is_object(),
                "kind must be a string discriminant, not a tagged-enum object"
            );
            // Options are hoisted to the field's top level, not nested under `kind`.
            assert_eq!(field["options"][0]["id"], "a");
            assert_eq!(field["options"][1]["label"], "Option B");
            assert_eq!(field["required"], true);
            assert!(field["label"].as_str().is_some());
        }
        _ => panic!("expected Structured, got {out:?}"),
    }
}

#[tokio::test]
async fn async_mode_form_id_is_stable_within_outcome() {
    let ctx = RunnerContext::new_with_cwd("s", "a", PathBuf::from("/tmp"));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "mode": "async",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["form_id"], v["spec"]["form_id"]);
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn default_mode_is_sync() {
    // Without "mode", should go through the sync bridge path.
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid",
        "name",
        "Alice",
    ))));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Name",
                "questions": [{"id": "name", "type": "text", "label": "Your name?"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    // Sync bridge path returns answers, not a posted:true payload.
    match out {
        ToolOutput::Structured(v) => {
            assert!(v.get("posted").is_none(), "should not have 'posted' key in sync output");
            assert_eq!(v["answers"]["name"]["value"], "Alice");
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn explicit_sync_mode_uses_bridge() {
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid",
        "q",
        "answer",
    ))));
    let ctx = ctx_with_bridge(bridge);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "mode": "sync",
                "questions": [{"id": "q", "type": "text", "label": "Q?"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["q"]["value"], "answer");
        }
        _ => panic!("expected Structured"),
    }
}

#[tokio::test]
async fn invalid_mode_is_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "mode": "turbo",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

// ── max_files clamp ──────────────────────────────────────────────────────────

#[test]
fn max_files_zero_is_clamped_to_one() {
    let field = super::parse_field(&json!({
        "id": "f", "type": "file", "label": "L", "max_files": 0
    }))
    .unwrap();
    match field.kind {
        ao_engine_tools_core::FormFieldKind::File { max_files, .. } => {
            assert_eq!(max_files, 1, "max_files: 0 should clamp to 1");
        }
        _ => panic!("expected File kind"),
    }
}

#[test]
fn max_files_upper_bound_is_ten() {
    let field = super::parse_field(&json!({
        "id": "f", "type": "file", "label": "L", "max_files": 11
    }))
    .unwrap();
    match field.kind {
        ao_engine_tools_core::FormFieldKind::File { max_files, .. } => {
            assert_eq!(max_files, 10, "max_files: 11 should clamp to 10");
        }
        _ => panic!("expected File kind"),
    }
}

#[test]
fn max_files_ten_is_accepted() {
    let field = super::parse_field(&json!({
        "id": "f", "type": "file", "label": "L", "max_files": 10
    }))
    .unwrap();
    match field.kind {
        ao_engine_tools_core::FormFieldKind::File { max_files, .. } => {
            assert_eq!(max_files, 10);
        }
        _ => panic!("expected File kind"),
    }
}

// ── Trait surface ────────────────────────────────────────────────────────────

#[test]
fn is_not_concurrency_safe() {
    assert!(!AskUserQuestionWithForm.is_concurrency_safe());
}

#[test]
fn does_not_mutate_filesystem() {
    assert!(!AskUserQuestionWithForm.mutates_filesystem());
}

#[test]
fn tool_name_is_correct() {
    assert_eq!(AskUserQuestionWithForm.name(), "AskUserQuestionWithForm");
}

// ── Invoke-time validation (SF-1) ────────────────────────────────────────────

#[tokio::test]
async fn duplicate_field_ids_are_rejected() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Test",
                "questions": [
                    {"id": "name", "type": "text", "label": "Name?"},
                    {"id": "name", "type": "text", "label": "Name again?"},
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("duplicate field id"), "expected 'duplicate field id' in: {message}");
        }
        _ => panic!("expected Error, got {out:?}"),
    }
}

#[test]
fn duplicate_option_ids_are_rejected() {
    let result = super::parse_field(&json!({
        "id": "choice",
        "type": "radio",
        "label": "Pick",
        "options": [
            {"id": "a", "label": "A"},
            {"id": "a", "label": "A again"},
            {"id": "b", "label": "B"},
        ]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("duplicate option id"), "expected 'duplicate option id' in: {msg}");
}

#[test]
fn max_files_on_text_field_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "text", "label": "Q?", "max_files": 3
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("max_files"), "expected 'max_files' in: {msg}");
}

#[test]
fn max_files_on_textarea_field_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "textarea", "label": "Q?", "max_files": 1
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("max_files"), "expected 'max_files' in: {msg}");
}

#[test]
fn max_files_on_checkbox_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "checkbox", "label": "Q?", "max_files": 3,
        "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("max_files"), "expected 'max_files' in: {msg}");
}

#[test]
fn accept_on_radio_field_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "radio", "label": "Q?", "accept": "image/*",
        "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("accept"), "expected 'accept' in: {msg}");
}

#[test]
fn accept_on_textarea_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "textarea", "label": "Q?", "accept": "image/*"
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("accept"), "expected 'accept' in: {msg}");
}

#[test]
fn options_on_text_field_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "text", "label": "Q?",
        "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("options"), "expected 'options' in: {msg}");
}

#[test]
fn options_on_file_field_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "file", "label": "Upload",
        "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("options"), "expected 'options' in: {msg}");
}

#[test]
fn field_id_with_invalid_chars_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "my field!", "type": "text", "label": "L"
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(
        msg.contains("letters, digits") || msg.contains("alphanumeric"),
        "expected pattern error in: {msg}"
    );
}

#[test]
fn field_id_empty_string_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "", "type": "text", "label": "L"
    }));
    assert!(result.is_err());
}

#[test]
fn option_id_with_invalid_chars_is_rejected() {
    let result = super::parse_field(&json!({
        "id": "q", "type": "radio", "label": "Pick",
        "options": [
            {"id": "opt one", "label": "Opt 1"},
            {"id": "opt-two", "label": "Opt 2"},
        ]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(
        msg.contains("letters, digits") || msg.contains("alphanumeric"),
        "expected pattern error in: {msg}"
    );
}

#[tokio::test]
async fn title_over_200_chars_is_rejected() {
    let ctx = default_ctx();
    let long_title = "a".repeat(201);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": long_title,
                "questions": [{"id": "q", "type": "text", "label": "Q?"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("200"), "expected '200' in: {message}");
        }
        _ => panic!("expected Error, got {out:?}"),
    }
}

#[tokio::test]
async fn intro_over_1000_chars_is_rejected() {
    let ctx = default_ctx();
    let long_intro = "a".repeat(1001);
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "T",
                "intro": long_intro,
                "questions": [{"id": "q", "type": "text", "label": "Q?"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("1000"), "expected '1000' in: {message}");
        }
        _ => panic!("expected Error, got {out:?}"),
    }
}

#[test]
fn field_id_over_64_chars_is_rejected() {
    let long_id = "a".repeat(65);
    let result = super::parse_field(&json!({
        "id": long_id, "type": "text", "label": "L"
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("64"), "expected '64' in: {msg}");
}

#[test]
fn field_label_over_300_chars_is_rejected() {
    let long_label = "a".repeat(301);
    let result = super::parse_field(&json!({
        "id": "q", "type": "text", "label": long_label
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("300"), "expected '300' in: {msg}");
}

#[test]
fn option_id_over_64_chars_is_rejected() {
    let long_id = "a".repeat(65);
    let result = super::parse_field(&json!({
        "id": "q", "type": "radio", "label": "Pick",
        "options": [
            {"id": long_id, "label": "A"},
            {"id": "b", "label": "B"},
        ]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("64"), "expected '64' in: {msg}");
}

#[test]
fn option_label_over_200_chars_is_rejected() {
    let long_label = "a".repeat(201);
    let result = super::parse_field(&json!({
        "id": "q", "type": "checkbox", "label": "Pick",
        "options": [
            {"id": "a", "label": long_label},
            {"id": "b", "label": "B"},
        ]
    }));
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("200"), "expected '200' in: {msg}");
}

// ── Sync form deadline (T2b) ────────────────────────────────────────────────

/// The deadline firing must produce a clearly-typed, distinguishable outcome
/// — not a generic error and not an empty answer. Uses a bridge whose
/// `ask_form` never resolves, so only the timeout branch can win.
#[tokio::test]
async fn sync_form_times_out_and_produces_distinguishable_outcome() {
    let ctx = ctx_with_bridge(std::sync::Arc::new(PendingForeverFormBridge));
    let out = super::resolve_sync_form(
        sample_request(),
        &ctx,
        std::time::Duration::from_millis(20),
    )
    .await
    .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["outcome"], "form_timed_out");
            assert_eq!(v["timeout_secs"], 0, "20ms rounds down to 0 whole seconds");
            assert!(v.get("answers").is_none(), "timeout must not look like an answered form");
            assert!(v.get("action").is_none(), "timeout must not look like an action click");
        }
        other => panic!("expected Structured timeout outcome, got {other:?}"),
    }
}

/// Regression guard: an answer that arrives before the deadline must still
/// resolve through the direct `ask_form` oneshot round-trip, exactly as it
/// did before this feature existed — NOT get rerouted into the async
/// `{"posted":true,...}` shape (which re-enters as a brand-new turn via
/// `submit_message`, arriving after the decision it was meant to inform).
/// Sync and async are separate control-flow primitives; adding a deadline to
/// sync must never blur that line. The 1-hour timeout here is long enough
/// that if this test ever went through the timeout branch instead of the
/// bridge, the test run itself would hang rather than silently pass.
#[tokio::test]
async fn answer_before_deadline_delivers_via_oneshot_not_async_repost() {
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid", "name", "Alice",
    ))));
    let ctx = ctx_with_bridge(bridge);
    let out = super::resolve_sync_form(sample_request(), &ctx, std::time::Duration::from_secs(3600))
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["name"]["value"], "Alice");
            assert!(
                v.get("posted").is_none() && v.get("spec").is_none(),
                "an in-deadline answer must never carry the async posted/spec envelope"
            );
            assert!(v.get("outcome").is_none(), "an answered form must not carry the timeout discriminant");
        }
        other => panic!("expected Structured answer, got {other:?}"),
    }
}

/// Timeout, cancellation, and a normal answer must be THREE observably
/// distinct outcomes — never collapsed into one another (T2c). Exercises
/// `resolve_sync_form` directly with three separate ctx/bridge
/// combinations, one per outcome; the cancellation case drives
/// `ctx.cancel.cancel()` itself — the exact select arm the MCP route's
/// per-session token relies on — rather than a bridge returning
/// `Err(Cancelled)` (that path is covered separately by
/// `cancelled_returns_non_recoverable_error` above). The MCP-route-level
/// proof that a live request actually reaches this arm lives in
/// `ao-server`'s `mcp_route_session_cancel_resolves_suspended_sync_form_as_cancelled`.
#[tokio::test]
async fn timeout_cancellation_and_answer_are_three_distinct_outcomes() {
    // Timeout: the bridge never resolves, so the deadline is the only way out.
    let timeout_ctx = ctx_with_bridge(std::sync::Arc::new(PendingForeverFormBridge));
    let timeout_out = super::resolve_sync_form(
        sample_request(),
        &timeout_ctx,
        std::time::Duration::from_millis(20),
    )
    .await
    .unwrap();

    // Cancellation: ctx.cancel fires before the (never-resolving) bridge can.
    let cancel_ctx = ctx_with_bridge(std::sync::Arc::new(PendingForeverFormBridge));
    cancel_ctx.cancel.cancel();
    let cancel_out = super::resolve_sync_form(
        sample_request(),
        &cancel_ctx,
        std::time::Duration::from_secs(3600),
    )
    .await
    .unwrap();

    // Answer: the bridge resolves normally, well within the deadline.
    let answer_ctx = ctx_with_bridge(std::sync::Arc::new(FixedFormBridge::returning(Ok(
        text_response("fid", "x", "hello"),
    ))));
    let answer_out = super::resolve_sync_form(
        sample_request(),
        &answer_ctx,
        std::time::Duration::from_secs(3600),
    )
    .await
    .unwrap();

    match &timeout_out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["outcome"], "form_timed_out");
            assert!(v.get("answers").is_none());
            assert!(v.get("action").is_none());
        }
        other => panic!("expected Structured timeout outcome, got {other:?}"),
    }

    match &cancel_out {
        ToolOutput::Error { message, recoverable } => {
            assert_eq!(message, "cancelled");
            assert!(!recoverable);
        }
        other => panic!("expected Error cancellation outcome, got {other:?}"),
    }

    match &answer_out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["x"]["value"], "hello");
            assert!(v.get("outcome").is_none(), "an answer must not carry the timeout discriminant");
            assert!(v.get("action").is_none());
        }
        other => panic!("expected Structured answer outcome, got {other:?}"),
    }

    // Belt-and-suspenders: pairwise distinct as serialized JSON, so a future
    // refactor can't accidentally make two of these three collapse into the
    // same shape without a test noticing.
    let a = serde_json::to_value(&timeout_out).unwrap();
    let b = serde_json::to_value(&cancel_out).unwrap();
    let c = serde_json::to_value(&answer_out).unwrap();
    assert_ne!(a, b, "timeout and cancellation must not serialize identically");
    assert_ne!(b, c, "cancellation and answer must not serialize identically");
    assert_ne!(a, c, "timeout and answer must not serialize identically");
}

// ── Model-facing prompt text (T2f) ──────────────────────────────────────────

/// The cancelled outcome must be documented for the model, distinguishably
/// from the timed-out outcome — a model that only knows about
/// `form_timed_out` has no instruction for a bare `"cancelled"` error and
/// may guess an answer instead of aborting.
#[test]
fn description_documents_cancelled_outcome_distinctly_from_timeout() {
    let desc = super::prompt::DESCRIPTION;
    assert!(
        desc.contains("\"cancelled\""),
        "DESCRIPTION must name the literal 'cancelled' error message the tool emits"
    );
    assert!(
        desc.contains("form_timed_out"),
        "sanity check: the timeout outcome is still documented"
    );
    // The cancelled-outcome text must carry the same "don't guess, abort"
    // instruction as the timeout text, not just an isolated mention.
    let cancelled_idx = desc.find("\"cancelled\"").unwrap();
    let cancelled_section = &desc[cancelled_idx..];
    assert!(
        cancelled_section.to_ascii_lowercase().contains("abort")
            || cancelled_section.to_ascii_lowercase().contains("do not guess"),
        "cancelled-outcome text must instruct the model to abort rather than guess"
    );
}

// ── Occupied-slot guard (one live form per (agent, thread)) ────────────────

/// Fresh on-disk [`ao_persistence::snapshot::SnapshotStore`] backing an
/// occupied-slot test — the guard reads through `ctx.snapshot_store`, so
/// these tests exercise the real `invoke()` path against a real store rather
/// than a stub, per the "green tests over dark code" concern: a mock that
/// always answers "occupied" would prove nothing about whether `invoke()`
/// actually reads the store the same way the write paths populate it.
async fn snapshot_store(dir: &tempfile::TempDir) -> ao_persistence::snapshot::SnapshotStore {
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    root.ensure_directories().await.unwrap();
    ao_persistence::snapshot::SnapshotStore::load(root).await.unwrap()
}

/// `ctx_with_bridge` plus a wired snapshot store and (optionally) a
/// non-default thread — the two extra pieces of context the slot-occupancy
/// tests below need to set up an existing pending form to post over.
fn ctx_with_snapshot(
    store: std::sync::Arc<ao_persistence::snapshot::SnapshotStore>,
    thread_id: Option<&str>,
) -> RunnerContext {
    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid", "x", "answer",
    ))));
    let ctx = ctx_with_bridge(bridge).with_snapshot_store(store);
    match thread_id {
        Some(id) => ctx.with_thread(id.to_string()),
        None => ctx,
    }
}

fn sample_form_input() -> serde_json::Value {
    json!({
        "title": "New question",
        "questions": [{"id": "x", "type": "text", "label": "Anything else?"}]
    })
}

/// A second SYNC form posted onto an occupied slot must now succeed through
/// the real `invoke()` path instead of being rejected — the tool-boundary
/// reject that used to gate this call is gone (see the replacement comment
/// at the top of `invoke()`). The actual slot handover + visible
/// `form_withdrawn` trace happens deeper in the sync write path
/// (`LiveFormBridge::persist_pending`, which needs a real `LiveFormBridge`,
/// not this file's `FixedFormBridge` stub — covered by
/// `ao-engine-tools-runner`'s own `prompt_bridge` test suite instead). This
/// test only proves the tool-boundary reject is gone.
#[tokio::test]
async fn second_form_on_occupied_live_slot_now_succeeds_instead_of_being_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = std::sync::Arc::new(snapshot_store(&dir).await);
    store
        .set_pending_form("agent", None, "existing-form".to_string(), json!({}))
        .await
        .unwrap();

    let ctx = ctx_with_snapshot(store, None);
    let out = AskUserQuestionWithForm.invoke(sample_form_input(), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v["answers"]["x"]["value"], "answer"),
        other => panic!("expected Structured (no longer rejected), got {other:?}"),
    }
}

/// Same, for the async branch: `invoke()` must return `posted: true`, not an
/// error, when the slot is already occupied — proving the removed
/// tool-boundary reject used to block this branch too (it ran before `mode`
/// was even parsed).
#[tokio::test]
async fn second_async_form_on_occupied_live_slot_now_succeeds_instead_of_being_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = std::sync::Arc::new(snapshot_store(&dir).await);
    store
        .set_pending_form("agent", None, "existing-form".to_string(), json!({}))
        .await
        .unwrap();

    let ctx = ctx_with_snapshot(store, None);
    let mut input = sample_form_input();
    input["mode"] = json!("async");
    let out = AskUserQuestionWithForm.invoke(input, &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["posted"], json!(true), "must post, not reject: {v:?}");
            assert!(v["form_id"].as_str().is_some());
        }
        other => panic!("expected Structured posted:true (no longer rejected), got {other:?}"),
    }
}

/// End-to-end regression for the newest-wins slot handover: posting form B
/// while form A is still pending — the "same still-running agent posts a
/// second form before the first is answered" scenario the invariant exists
/// for — must (a) let `invoke()` succeed for B instead of rejecting it, and
/// (b) leave a visible `form_withdrawn` trace for A once the runner's
/// post-dispatch hook (`wire_posted_async_form`) processes B's `posted: true`
/// result. This is the test that actually proves the old occupied-slot
/// reject is unreachable on the real form-post pipeline, not just that
/// `invoke()` alone doesn't reject.
///
/// Also covers the oneshot/rehydrate requirement: re-reading the transcript
/// afterward (as a page reload would) must not duplicate the withdrawn
/// trace — the write happens exactly once, at supersede time, never on read.
#[tokio::test]
async fn posting_a_second_async_form_through_the_real_pipeline_supersedes_the_first_and_survives_rehydrate(
) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let transcripts = std::sync::Arc::new(ao_persistence::transcript::TranscriptStore::new(root));
    let store = std::sync::Arc::new(snapshot_store(&dir).await);

    let bridge = std::sync::Arc::new(FixedFormBridge::returning(Ok(text_response(
        "fid", "x", "answer",
    ))));
    let ctx = ctx_with_bridge(bridge)
        .with_snapshot_store(store.clone())
        .with_transcript_store(transcripts.clone());

    // Post form A (async) through the real invoke() + post-dispatch pipeline.
    let mut input_a = sample_form_input();
    input_a["title"] = json!("Question A");
    input_a["mode"] = json!("async");
    let out_a = AskUserQuestionWithForm.invoke(input_a, &ctx).await.unwrap();
    ao_engine_tools_core::form_events::wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out_a)
        .await;

    let snap = store.get().await;
    assert_eq!(snap.agents.get("agent").unwrap().pending_forms.len(), 1, "A must be pending");

    // Post form B (async) while A is still pending — must NOT be rejected.
    let mut input_b = sample_form_input();
    input_b["title"] = json!("Question B");
    input_b["mode"] = json!("async");
    let out_b = AskUserQuestionWithForm.invoke(input_b, &ctx).await.unwrap();
    match &out_b {
        ToolOutput::Structured(v) => assert_eq!(v["posted"], json!(true), "must post, not reject: {v:?}"),
        other => panic!("expected Structured posted:true, got {other:?}"),
    }
    ao_engine_tools_core::form_events::wire_posted_async_form(&ctx, "AskUserQuestionWithForm", &out_b)
        .await;

    // A's slot is now held by B — newest wins, exactly one form pending.
    let snap = store.get().await;
    let pending = &snap.agents.get("agent").unwrap().pending_forms;
    assert_eq!(pending.len(), 1, "exactly one form pending — newest wins");

    // ...and a visible form_withdrawn trace was left for A.
    let entries = transcripts.read_all("agent").await.unwrap();
    let withdrawn: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == ao_engine_tools_core::FORM_WITHDRAWN)
        .collect();
    assert_eq!(withdrawn.len(), 1, "exactly one withdrawn trace for the displaced form");
    assert!(
        withdrawn[0].content.contains("Question A"),
        "must name the displaced question: {}",
        withdrawn[0].content
    );
    assert!(!withdrawn[0].hidden_from_user, "the withdrawn trace must be visible in the transcript");

    // Oneshot guarantee: re-reading the transcript (a page reload = another
    // plain read, no side effects) must not re-emit or duplicate the trace.
    let entries_reload = transcripts.read_all("agent").await.unwrap();
    let withdrawn_reload = entries_reload
        .iter()
        .filter(|e| e.event_type == ao_engine_tools_core::FORM_WITHDRAWN)
        .count();
    assert_eq!(
        withdrawn_reload, 1,
        "rehydrating (re-reading the transcript, as a page reload would) must not duplicate the withdrawn trace"
    );
}

/// A pending form on a DIFFERENT thread must never block this one — the
/// slot is keyed on `(agent_id, thread_id)`, and each thread gets its own.
/// Over-rejecting here (treating any pending form anywhere on the agent as
/// occupying every thread) would be its own new bug.
#[tokio::test]
async fn form_on_different_thread_is_allowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = std::sync::Arc::new(snapshot_store(&dir).await);
    store
        .set_pending_form("agent", Some("thread-a".to_string()), "existing-form".to_string(), json!({}))
        .await
        .unwrap();

    let ctx = ctx_with_snapshot(store, Some("thread-b"));
    let out = AskUserQuestionWithForm.invoke(sample_form_input(), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["x"]["value"], "answer");
        }
        other => panic!("expected Structured (not rejected), got {other:?}"),
    }
}

/// A slot whose only occupant is marked `orphaned` must be treated as empty
/// — the anti-lockout case. `orphaned` means the reaper already determined
/// nothing will ever answer that record (its owning process didn't survive
/// a restart); refusing a fresh post because of it would strand the thread
/// forever with no way to ask another question.
#[tokio::test]
async fn slot_with_only_an_orphaned_record_is_allowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = std::sync::Arc::new(snapshot_store(&dir).await);
    store
        .set_pending_form("agent", None, "stale-form".to_string(), json!({}))
        .await
        .unwrap();
    store.mark_pending_form_orphaned("agent", "stale-form").await.unwrap();

    let ctx = ctx_with_snapshot(store, None);
    let out = AskUserQuestionWithForm.invoke(sample_form_input(), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["answers"]["x"]["value"], "answer");
        }
        other => panic!("expected Structured (not rejected), got {other:?}"),
    }
}

/// No snapshot store wired on `ctx` at all (the default in every other test
/// in this file) must not panic and must not block — the guard is
/// best-effort, matching every other snapshot touch on this tool's write
/// paths.
#[tokio::test]
async fn no_snapshot_store_wired_does_not_block() {
    let ctx = default_ctx();
    let out = AskUserQuestionWithForm.invoke(sample_form_input(), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(_) => {}
        other => panic!("expected Structured (not rejected), got {other:?}"),
    }
}

// ── Sync-form-timeout vs. session-TTL misconfiguration (T2f) ────────────────

#[test]
fn misconfiguration_detector_warns_when_timeout_meets_or_exceeds_ttl() {
    // Equal is still unsafe: `ctx.cancel` and the form's own deadline race in
    // the same `tokio::select!`, so a tie is not guaranteed to resolve via
    // the timeout branch.
    let equal = super::check_sync_form_timeout_vs_session_ttl(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(3600),
    );
    assert!(equal.is_some(), "timeout == ttl must be flagged as unsafe");
    let msg = equal.unwrap();
    assert!(msg.contains("AO_SYNC_FORM_TIMEOUT_SECS"), "warning must name the form-timeout env var: {msg}");
    assert!(msg.contains("LAUNCHPAD_MCP_SESSION_TTL_SECS"), "warning must name the session-TTL env var: {msg}");
    assert!(msg.contains("3600"), "warning must include the resolved values: {msg}");

    let over = super::check_sync_form_timeout_vs_session_ttl(
        std::time::Duration::from_secs(4000),
        std::time::Duration::from_secs(3600),
    );
    assert!(over.is_some(), "timeout > ttl must be flagged as unsafe");
    let msg = over.unwrap();
    assert!(msg.contains("4000") && msg.contains("3600"), "warning must include both resolved values: {msg}");
}

#[test]
fn misconfiguration_detector_stays_quiet_for_default_config() {
    // The shipped defaults: AO_SYNC_FORM_TIMEOUT_SECS=1800,
    // LAUNCHPAD_MCP_SESSION_TTL_SECS=3600.
    let default = super::check_sync_form_timeout_vs_session_ttl(
        std::time::Duration::from_secs(1800),
        std::time::Duration::from_secs(3600),
    );
    assert!(default.is_none(), "default config (1800 < 3600) must not warn");
}

/// `invoke()` itself must reach the deadline machinery through the sync
/// branch (not just the lower-level `resolve_sync_form` helper exercised
/// above) — full end-to-end proof that a real tool call, given a bridge that
/// never answers and the tool's own default multi-minute deadline, does not
/// hang the test suite. Uses a short per-test override via the env var the
/// tool reads, scoped to this one test to avoid cross-test interference.
#[tokio::test]
async fn invoke_sync_mode_surfaces_timeout_end_to_end() {
    // This env var is read only by `sync_form_timeout()`, and no other test
    // in this crate sets or reads it, so a same-process write here cannot
    // race another test's use of the same key.
    std::env::set_var("AO_SYNC_FORM_TIMEOUT_SECS", "0");
    let ctx = ctx_with_bridge(std::sync::Arc::new(PendingForeverFormBridge));
    let out = AskUserQuestionWithForm
        .invoke(
            json!({
                "title": "Q",
                "questions": [{"id": "x", "type": "text", "label": "L"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    std::env::remove_var("AO_SYNC_FORM_TIMEOUT_SECS");
    match out {
        ToolOutput::Structured(v) => assert_eq!(v["outcome"], "form_timed_out"),
        other => panic!("expected Structured timeout outcome, got {other:?}"),
    }
}
