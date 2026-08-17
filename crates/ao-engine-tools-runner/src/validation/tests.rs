//! Unit tests for the validation pipeline. Declared from `mod.rs` as
//! `#[cfg(test)] mod tests;` so private items remain in scope.

use serde_json::{json, Value};

use super::{validate_invocation, ErasedTool, ValidationOutcome};

/// Test fixture: a tool with a configurable schema and an optional
/// `validate` body. Lets each case shape the gate behavior without
/// pulling in real IO tools.
struct FakeTool {
    name: &'static str,
    schema: Value,
    validate_err: Option<String>,
}

impl FakeTool {
    fn new(name: &'static str, schema: Value) -> Self {
        Self {
            name,
            schema,
            validate_err: None,
        }
    }

    fn with_validate_err(mut self, msg: &str) -> Self {
        self.validate_err = Some(msg.to_string());
        self
    }
}

impl ErasedTool for FakeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    fn validate(&self, _input: &Value) -> Result<(), String> {
        match &self.validate_err {
            Some(m) => Err(m.clone()),
            None => Ok(()),
        }
    }
}

fn person_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "age": {"type": "integer", "minimum": 0}
        },
        "required": ["name"]
    })
}

#[tokio::test]
async fn matching_input_passes_through_to_ok() {
    let tool = FakeTool::new("Person", person_schema());
    let input = json!({"name": "Ada", "age": 36});

    let out = validate_invocation(&tool, &input).await;
    match out {
        ValidationOutcome::Ok(v) => assert_eq!(v, input),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_required_field_returns_schema_error_naming_field() {
    let tool = FakeTool::new("Person", person_schema());
    let input = json!({"age": 36});

    let out = validate_invocation(&tool, &input).await;
    match out {
        ValidationOutcome::SchemaError(msg) => {
            assert!(
                msg.starts_with("InputValidationError:"),
                "wrong prefix: {msg}"
            );
            assert!(
                msg.contains("name"),
                "expected message to name the missing `name` field, got: {msg}"
            );
            assert!(
                msg.contains("Person"),
                "expected message to mention the tool name, got: {msg}"
            );
        }
        other => panic!("expected SchemaError, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_type_returns_schema_error() {
    let tool = FakeTool::new("Person", person_schema());
    let input = json!({"name": 12});

    let out = validate_invocation(&tool, &input).await;
    match out {
        ValidationOutcome::SchemaError(msg) => {
            assert!(msg.starts_with("InputValidationError:"), "wrong prefix: {msg}");
        }
        other => panic!("expected SchemaError, got {other:?}"),
    }
}

#[tokio::test]
async fn passes_schema_then_tool_validate_fails_returns_tool_error() {
    let tool = FakeTool::new("Person", person_schema())
        .with_validate_err("age must be even on this fixture");
    let input = json!({"name": "Ada", "age": 36});

    let out = validate_invocation(&tool, &input).await;
    match out {
        ValidationOutcome::ToolError(msg) => {
            assert!(
                msg.starts_with("ToolValidationError:"),
                "wrong prefix: {msg}"
            );
            assert!(
                msg.contains("age must be even"),
                "tool message not surfaced: {msg}"
            );
        }
        other => panic!("expected ToolError, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_schema_surfaces_clear_error_without_panic() {
    // A bare integer is not a valid JSON Schema document — `Validator::new`
    // returns an Err. We assert we caught it and produced a clean
    // `SchemaError`, never a panic.
    let tool = FakeTool::new("Broken", json!(42));
    let input = json!({});

    let out = validate_invocation(&tool, &input).await;
    match out {
        ValidationOutcome::SchemaError(msg) => {
            assert!(msg.starts_with("InputValidationError:"), "wrong prefix: {msg}");
            assert!(
                msg.contains("Broken"),
                "expected tool name in compile-failure message, got: {msg}"
            );
        }
        other => panic!("expected SchemaError on bad schema, got {other:?}"),
    }
}

#[tokio::test]
async fn default_validate_is_noop_when_schema_passes() {
    // No `with_validate_err` — default impl returns Ok.
    let tool = FakeTool::new("Person", person_schema());
    let input = json!({"name": "Ada"});

    let out = validate_invocation(&tool, &input).await;
    assert!(matches!(out, ValidationOutcome::Ok(_)));
}
