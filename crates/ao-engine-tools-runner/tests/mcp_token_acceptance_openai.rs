//! Acceptance harness — OpenAI tiktoken-rs no-regression token-count smoke.
//!
//! # Environment variable
//!
//! The tests in this module are gated behind `OPENAI_API_KEY`. The key is not
//! used for any network call — token counting is performed locally via
//! `tiktoken-rs`. The gate exists so the harness can be wired to a real
//! OpenAI count surface in the future without an env-contract change.
//!
//! # What this measures
//!
//! Two request bodies are serialised to JSON and encoded with tiktoken-rs's
//! `cl100k_base` encoder:
//!
//! 1. **pin_inline_openai** — 100 synthetic `mcp__benchmark__tool_NNN` tools
//!    plus `ToolSearch`, all emitted inline in `tools[]`. Records the raw token
//!    count as the baseline.
//!
//! 2. **pin_deferred_openai** — the same request with the 100 deferred tools
//!    omitted entirely (OpenAI runtime-expansion fallback); only `ToolSearch`
//!    appears in `tools[]`. Records the reduced token count.
//!
//! # Decision note
//!
//! `tiktoken-rs` counts the raw JSON payload, NOT the OpenAI server's actual
//! prompt-token billing — there is some envelope overhead the local encoder
//! does not model. This is acceptable for relative-comparison smokes; the
//! assertion cares about delta, not absolute values.
//!
//! # Assertion
//!
//! ```text
//! deferred < inline  AND  (inline - deferred) > 1_000
//! ```
//!
//! This guards against the request becoming smaller for trivial reasons (e.g.
//! JSON-escape changes) while confirming the deferral strategy actually saves a
//! meaningful number of tokens.

// TODO(loop-e-followup): tune to a percentage threshold once we have ≥3
// baseline runs across model versions.

use serde_json::{json, Value};

/// System prompt used in both token-counting calls.
const SYSTEM_PROMPT: &str =
    "You are an expert software engineer with access to a comprehensive suite of \
     development tools. You help users understand, navigate, and modify codebases \
     accurately and efficiently. Always use the most appropriate tool for the task.";

/// User message used in both token-counting calls.
const USER_MESSAGE: &str =
    "Please analyze the project structure and provide a comprehensive summary of \
     the codebase, including key modules, their responsibilities, and how they \
     interact with each other.";

/// Model name embedded in the request body (affects no tokenisation here).
const MODEL: &str = "gpt-4o";

/// Build 100 synthetic tools in OpenAI function-calling wire shape
/// (3–5 properties each, mix of types, ~50-word descriptions per schema).
fn synthetic_tools_openai() -> Vec<Value> {
    (1u32..=100)
        .map(|i| {
            json!({
                "type": "function",
                "function": {
                    "name": format!("mcp__benchmark__tool_{i:03}"),
                    "description": format!(
                        "Benchmark tool {i:03} for the token-counting acceptance harness. \
                         This synthetic MCP tool exercises the deferred-loading pipeline and validates \
                         that per-tool schema deferral reduces input-token consumption by the expected \
                         margin across realistic workloads. Tool variant {i:03} of 100."
                    ),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "target_path": {
                                "type": "string",
                                "description": "Absolute filesystem path to the resource this tool operates on."
                            },
                            "operation_mode": {
                                "type": "string",
                                "enum": ["read", "write", "append", "truncate"],
                                "description": "The operation mode to apply. Defaults to read when omitted."
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Maximum wall-clock time in milliseconds before aborting. Zero means no timeout."
                            },
                            "metadata": {
                                "type": "object",
                                "description": "Arbitrary key-value metadata attached to the operation for audit logging."
                            },
                            "dry_run": {
                                "type": "boolean",
                                "description": "When true the tool reports what it would do without executing any side-effects."
                            }
                        },
                        "required": ["target_path"]
                    }
                }
            })
        })
        .collect()
}

/// The ToolSearch tool in OpenAI function-calling wire shape.
fn tool_search_openai() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "ToolSearch",
            "description": "Search the tool registry by name to load a deferred tool's full schema \
                            into the current session, enabling the model to invoke it directly.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Exact qualified name of the deferred tool to resolve \
                                        (e.g. mcp__server__tool_name)."
                    }
                },
                "required": ["name"]
            }
        }
    })
}

/// Build an OpenAI Chat Completions request body with the supplied tools list.
fn build_request_body(tools: Vec<Value>) -> Value {
    let mut body = json!({
        "model": MODEL,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user",   "content": USER_MESSAGE  }
        ],
        "stream": true,
        "stream_options": { "include_usage": true }
    });

    if !tools.is_empty() {
        let obj = body.as_object_mut().unwrap();
        obj.insert("tools".into(), Value::Array(tools));
        obj.insert("parallel_tool_calls".into(), json!(true));
    }

    body
}

/// Count tokens in a JSON value by serialising to a compact string and encoding
/// with tiktoken-rs's `cl100k_base` encoder.
fn count_tokens_local(body: &Value) -> usize {
    let json_str = serde_json::to_string(body).expect("serialisation should not fail");
    let bpe = tiktoken_rs::cl100k_base().expect("cl100k_base encoder should be available");
    bpe.encode_with_special_tokens(&json_str).len()
}

/// Baseline: 100 tools fully inline, token-counted via tiktoken-rs.
///
/// Skipped when `OPENAI_API_KEY` is not set.
#[test]
fn pin_inline_openai() {
    let _ = tracing_subscriber::fmt::try_init();

    match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => {}
        _ => {
            tracing::info!("OPENAI_API_KEY not set — skipping pin_inline_openai");
            return;
        }
    }

    let mut tools = synthetic_tools_openai();
    tools.push(tool_search_openai());

    let body = build_request_body(tools);
    let token_count = count_tokens_local(&body);

    tracing::info!(token_count, "pin_inline_openai baseline token count");
    assert!(token_count > 0, "inline token count should be positive");
}

/// Reduction check: 100 tools deferred (omitted) vs 100 tools inline.
///
/// Asserts `deferred < inline` and `(inline − deferred) > 1_000`.
///
/// Skipped when `OPENAI_API_KEY` is not set.
#[test]
fn pin_deferred_openai() {
    let _ = tracing_subscriber::fmt::try_init();

    match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => {}
        _ => {
            tracing::info!("OPENAI_API_KEY not set — skipping pin_deferred_openai");
            return;
        }
    }

    // --- inline: all 100 tools in tools[] ---
    let mut inline_tools = synthetic_tools_openai();
    inline_tools.push(tool_search_openai());
    let inline_body = build_request_body(inline_tools);
    let inline_tokens = count_tokens_local(&inline_body);

    // --- deferred: only ToolSearch in tools[] (100 deferred tools omitted) ---
    let deferred_body = build_request_body(vec![tool_search_openai()]);
    let deferred_tokens = count_tokens_local(&deferred_body);

    let reduction = (inline_tokens as f64 - deferred_tokens as f64) / inline_tokens as f64;

    tracing::info!(
        inline_tokens,
        deferred_tokens,
        reduction_pct = format!("{:.1}%", reduction * 100.0),
        "OpenAI runtime-expansion token reduction measured"
    );

    assert!(
        deferred_tokens < inline_tokens,
        "deferred token count ({deferred_tokens}) should be less than inline ({inline_tokens})"
    );
    assert!(
        inline_tokens - deferred_tokens > 1_000,
        "expected >1000 token reduction; got {} \
         (inline={inline_tokens} deferred={deferred_tokens})",
        inline_tokens - deferred_tokens,
    );
}
