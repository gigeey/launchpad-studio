//! Acceptance harness — Anthropic ≥80% input-token reduction with 100 deferred tools.
//!
//! # Environment variable
//!
//! The tests in this module are gated behind `ANTHROPIC_API_KEY`. When the
//! variable is absent or empty each test prints a tracing `info` skip line and
//! returns early. CI ensures at least one provider's harness ran in every
//! qualifying build.
//!
//! # What this measures
//!
//! Two calls are made to `POST /v1/messages/count_tokens`:
//!
//! 1. **pin_inline** — 100 synthetic `mcp__benchmark__tool_NNN` tools plus
//!    `ToolSearch`, all emitted inline (no `defer_loading` flag). Records the
//!    input-token count as the baseline.
//!
//! 2. **pin_deferred** — the same 100 tools with `defer_loading: true` (the
//!    Anthropic API strips their input_schemas before counting) plus `ToolSearch`
//!    inline. Computes and asserts the deferred-tool acceptance criterion:
//!
//!    ```text
//!    (inline − deferred) / inline ≥ 0.80
//!    ```

use serde_json::{json, Value};

const API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MODEL: &str = "claude-opus-4-7";

/// System prompt used in both count_tokens calls.
const SYSTEM_PROMPT: &str =
    "You are an expert software engineer with access to a comprehensive suite of \
     development tools. You help users understand, navigate, and modify codebases \
     accurately and efficiently. Always use the most appropriate tool for the task.";

/// User message used in both count_tokens calls.
const USER_MESSAGE: &str =
    "Please analyze the project structure and provide a comprehensive summary of \
     the codebase, including key modules, their responsibilities, and how they \
     interact with each other.";

/// Build 100 synthetic tools with realistic-shape schemas (3–5 properties each,
/// mix of types, ~50-word descriptions per schema).
fn synthetic_tools() -> Vec<Value> {
    (1u32..=100)
        .map(|i| {
            json!({
                "name": format!("mcp__benchmark__tool_{i:03}"),
                "description": format!(
                    "Benchmark tool {i:03} for the token-counting acceptance harness. \
                     This synthetic MCP tool exercises the deferred-loading pipeline and validates \
                     that per-tool schema deferral reduces input-token consumption by the expected \
                     margin across realistic workloads. Tool variant {i:03} of 100."
                ),
                "input_schema": {
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
            })
        })
        .collect()
}

/// The ToolSearch tool — always included inline in both requests.
fn tool_search_spec() -> Value {
    json!({
        "name": "ToolSearch",
        "description": "Search the tool registry by name to load a deferred tool's full schema into the current session, enabling the model to invoke it directly.",
        "input_schema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact qualified name of the deferred tool to resolve (e.g. mcp__server__tool_name)."
                }
            },
            "required": ["name"]
        }
    })
}

/// Call `POST /v1/messages/count_tokens` and return the `input_tokens` count.
async fn count_tokens(api_key: &str, tools: Vec<Value>) -> u64 {
    let client = reqwest::Client::new();

    let body = json!({
        "model": MODEL,
        "system": [{ "type": "text", "text": SYSTEM_PROMPT }],
        "messages": [{
            "role": "user",
            "content": USER_MESSAGE
        }],
        "tools": tools,
    });

    let response = client
        .post(format!("{API_BASE}/v1/messages/count_tokens"))
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("count_tokens HTTP request failed");

    let status = response.status();
    let response_body: Value = response
        .json()
        .await
        .expect("count_tokens response should be valid JSON");

    assert!(
        status.is_success(),
        "count_tokens returned HTTP {status}: {response_body}"
    );

    response_body["input_tokens"]
        .as_u64()
        .expect("count_tokens response missing numeric input_tokens field")
}

/// Baseline measurement: 100 tools fully inline (no defer_loading flag).
///
/// Skipped when `ANTHROPIC_API_KEY` is not set.
#[tokio::test]
async fn pin_inline() {
    let _ = tracing_subscriber::fmt::try_init();

    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::info!("ANTHROPIC_API_KEY not set — skipping pin_inline");
            return;
        }
    };

    let mut tools = synthetic_tools();
    tools.push(tool_search_spec());

    let inline_tokens = count_tokens(&api_key, tools).await;

    tracing::info!(inline_tokens, "pin_inline baseline token count");
    assert!(inline_tokens > 0, "inline token count should be positive");
}

/// Reduction check: 100 tools with defer_loading: true vs fully inline.
///
/// Makes both API calls within a single test so the comparison is self-contained.
/// Asserts `(inline − deferred) / inline ≥ 0.80` for deferred tool loading.
///
/// Skipped when `ANTHROPIC_API_KEY` is not set.
#[tokio::test]
async fn pin_deferred() {
    let _ = tracing_subscriber::fmt::try_init();

    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::info!("ANTHROPIC_API_KEY not set — skipping pin_deferred");
            return;
        }
    };

    // --- inline: all 100 tools without defer_loading ---
    let mut inline_tools = synthetic_tools();
    inline_tools.push(tool_search_spec());
    let inline_tokens = count_tokens(&api_key, inline_tools).await;

    // --- deferred: 100 tools with defer_loading: true + ToolSearch inline ---
    let deferred_tools: Vec<Value> = synthetic_tools()
        .into_iter()
        .map(|mut t| {
            t.as_object_mut()
                .unwrap()
                .insert("defer_loading".into(), json!(true));
            t
        })
        .chain(std::iter::once(tool_search_spec()))
        .collect();
    let deferred_tokens = count_tokens(&api_key, deferred_tools).await;

    let reduction = (inline_tokens as f64 - deferred_tokens as f64) / inline_tokens as f64;

    tracing::info!(
        inline_tokens,
        deferred_tokens,
        reduction_pct = format!("{:.1}%", reduction * 100.0),
        "token reduction measured"
    );

    assert!(
        reduction >= 0.80,
        "expected ≥80% input-token reduction; got {:.1}% \
         (inline={inline_tokens} deferred={deferred_tokens})",
        reduction * 100.0,
    );
}
