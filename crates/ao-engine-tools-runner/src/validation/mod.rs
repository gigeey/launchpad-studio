//! Validation pipeline.
//!
//! Each tool invocation that arrives from the model is run through two
//! gates before any hook, permission check, or tool body executes:
//!
//! 1. **JSON-schema validation** against the schema returned by
//!    `ErasedTool::input_schema()`. If the schema fails to compile or
//!    the input fails to match, the pipeline yields
//!    [`ValidationOutcome::SchemaError`] and the runner skips the rest
//!    of the pipeline for this invocation.
//! 2. **Tool-specific validation** via
//!    [`ErasedTool::validate`]. Tools that need additional sanity checks
//!    (cross-field invariants, runtime path resolution, etc.) override
//!    the default no-op; if it returns `Err`, the pipeline yields
//!    [`ValidationOutcome::ToolError`].
//!
//! Both error variants carry a single message string shaped for the
//! eventual `tool_result` block: schema errors are prefixed with
//! `InputValidationError:` and tool errors with `ToolValidationError:`.
//!
//! [`ErasedTool`] is intentionally local to the runner crate. The
//! foundation traits `IoTool` / `EngineTool` do not share a common
//! parent, but both expose `name()` and `input_schema()`; this trait is
//! the small dyn-safe view the validator needs without bolting a new
//! method onto either foundation trait.

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, IoTool, ToolRef};
use jsonschema::Validator;
use serde_json::Value;

/// Object-safe view over a tool used by the validation pipeline.
///
/// The default `validate` is a no-op so most tools opt into schema
/// validation only. Tools (or callers wrapping a tool) override
/// `validate` when they need a second-stage check that cannot be
/// expressed in the JSON schema itself.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &str;
    fn input_schema(&self) -> Value;

    fn validate(&self, _input: &Value) -> Result<(), String> {
        Ok(())
    }
}

impl ErasedTool for Arc<dyn IoTool> {
    fn name(&self) -> &str {
        IoTool::name(self.as_ref())
    }
    fn input_schema(&self) -> Value {
        IoTool::input_schema(self.as_ref())
    }
}

impl ErasedTool for Arc<dyn EngineTool> {
    fn name(&self) -> &str {
        EngineTool::name(self.as_ref())
    }
    fn input_schema(&self) -> Value {
        EngineTool::input_schema(self.as_ref())
    }
}

impl ErasedTool for ToolRef {
    fn name(&self) -> &str {
        ToolRef::name(self)
    }
    fn input_schema(&self) -> Value {
        match self {
            ToolRef::Io(t) => t.input_schema(),
            ToolRef::Engine(t) => t.input_schema(),
        }
    }
}

/// Outcome of a single invocation passing (or failing) the validation
/// pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationOutcome {
    /// Both stages passed. Carries the parsed input forward unchanged
    /// so downstream stages see exactly what the model emitted.
    Ok(Value),
    /// Either the schema failed to compile, or the input did not match
    /// the schema. Message is human-readable and prefixed with
    /// `InputValidationError:`.
    SchemaError(String),
    /// Tool-specific `validate` returned an error. Message is prefixed
    /// with `ToolValidationError:`.
    ToolError(String),
}

/// How many individual schema errors to surface in a single
/// `SchemaError` message before truncating. Five keeps the message
/// scannable without dropping the field that broke first.
const MAX_REPORTED_SCHEMA_ERRORS: usize = 5;

/// Run the schema gate, then the tool's `validate` gate. The function
/// is async to fit the runner's pipeline shape — the body itself is
/// currently synchronous.
pub async fn validate_invocation(
    tool: &dyn ErasedTool,
    raw_input: &Value,
) -> ValidationOutcome {
    let schema = tool.input_schema();

    let validator = match Validator::new(&schema) {
        Ok(v) => v,
        Err(err) => {
            return ValidationOutcome::SchemaError(format!(
                "InputValidationError: failed to compile input schema for `{}`: {}",
                tool.name(),
                err
            ));
        }
    };

    let mut messages: Vec<String> = Vec::new();
    for err in validator.iter_errors(raw_input) {
        let location = err.instance_path.to_string();
        let path = if location.is_empty() { "/".to_string() } else { location };
        messages.push(format!("at `{path}`: {err}"));
        if messages.len() >= MAX_REPORTED_SCHEMA_ERRORS {
            break;
        }
    }
    if !messages.is_empty() {
        return ValidationOutcome::SchemaError(format!(
            "InputValidationError: input for `{}` failed schema validation: {}",
            tool.name(),
            messages.join("; ")
        ));
    }

    if let Err(msg) = tool.validate(raw_input) {
        return ValidationOutcome::ToolError(format!(
            "ToolValidationError: {msg}"
        ));
    }

    ValidationOutcome::Ok(raw_input.clone())
}

#[cfg(test)]
mod tests;
