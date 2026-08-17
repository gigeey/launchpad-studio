//! Acceptance harness — Gemini `countTokens` no-regression smoke.
//!
//! # Environment variable
//!
//! The tests in this module are gated behind `GOOGLE_API_KEY`. When the
//! variable is absent or empty each test prints a tracing `info` skip line and
//! returns early. CI ensures at least one provider's harness ran in every
//! qualifying build.
//!
//! # What this measures
//!
//! Two calls are made to `POST /v1beta/models/{model}:countTokens`:
//!
//! 1. **pin_inline_gemini** — 100 synthetic `mcp__benchmark__tool_NNN` tools
//!    plus `ToolSearch`, all emitted as inline `functionDeclarations`. Records
//!    `totalTokens` as the baseline.
//!
//! 2. **pin_deferred_gemini** — the same request with the 100 deferred tools
//!    omitted entirely (Gemini runtime-expansion fallback); only `ToolSearch`
//!    appears in `functionDeclarations`. Computes and asserts the delta.
//!
//! # Decision note
//!
//! Gemini has no native `defer_loading` equivalent. The fallback is purely
//! structural (omit, then inject on resolution). This smoke confirms that the
//! omit-and-inject strategy actually saves tokens on the Gemini wire format.
//!
//! # Assertion
//!
//! ```text
//! deferred < inline  AND  (inline - deferred) > 1_000
//! ```
//!
//! This guards against the request becoming smaller for trivial reasons while
//! confirming the deferral strategy saves a meaningful number of tokens.

// TODO(loop-e-followup): tune to a percentage threshold once we have ≥3
// baseline runs across model versions.

use serde_json::{json, Value};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const MODEL: &str = "gemini-1.5-pro";

/// System prompt used in both `countTokens` calls.
const SYSTEM_PROMPT: &str =
    "You are an expert software engineer with access to a comprehensive suite of \
     development tools. You help users understand, navigate, and modify codebases \
     accurately and efficiently. Always use the most appropriate tool for the task.";

/// User message used in both `countTokens` calls.
const USER_MESSAGE: &str =
    "Please analyze the project structure and provide a comprehensive summary of \
     the codebase, including key modules, their responsibilities, and how they \
     interact with each other.";

/// Build 100 synthetic tools in Gemini `functionDeclarations` wire shape
/// (3–5 properties each, mix of types, ~50-word descriptions per schema).
fn synthetic_tools_gemini() -> Vec<Value> {
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
                "parameters": {
                    "type": "object",
                    "properties": {
                        "target_path": {
                            "type": "string",
                            "description": "Absolute filesystem path to the resource this tool operates on."
                        },
                        "operation_mode": {
                            "type": "string",
                            "description": "The operation mode to apply. One of: read, write, append, truncate. Defaults to read when omitted."
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

/// The ToolSearch tool in Gemini `functionDeclarations` wire shape.
fn tool_search_gemini() -> Value {
    json!({
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
    })
}

/// Build a Gemini `generateContent`-shaped request body for `countTokens`.
///
/// `function_decls` are wrapped in the `tools[].functionDeclarations` envelope.
/// When empty, the `tools` field is omitted entirely.
fn build_request_body(function_decls: Vec<Value>) -> Value {
    let mut body = json!({
        "systemInstruction": {
            "parts": [{ "text": SYSTEM_PROMPT }]
        },
        "contents": [{
            "role": "user",
            "parts": [{ "text": USER_MESSAGE }]
        }]
    });

    if !function_decls.is_empty() {
        body.as_object_mut().unwrap().insert(
            "tools".into(),
            json!([{ "functionDeclarations": function_decls }]),
        );
    }

    body
}

/// Call `POST /v1beta/models/{MODEL}:countTokens` and return `totalTokens`.
async fn count_tokens_gemini(api_key: &str, body: &Value) -> u64 {
    let client = reqwest::Client::new();
    let url = format!("{API_BASE}/models/{MODEL}:countTokens");

    let response = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .expect("countTokens HTTP request failed");

    let status = response.status();
    let response_body: Value = response
        .json()
        .await
        .expect("countTokens response should be valid JSON");

    assert!(
        status.is_success(),
        "countTokens returned HTTP {status}: {response_body}"
    );

    response_body["totalTokens"]
        .as_u64()
        .expect("countTokens response missing numeric totalTokens field")
}

/// Baseline: 100 tools fully inline as `functionDeclarations`.
///
/// Skipped when `GOOGLE_API_KEY` is not set.
#[tokio::test]
async fn pin_inline_gemini() {
    let _ = tracing_subscriber::fmt::try_init();

    let api_key = match std::env::var("GOOGLE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::info!("GOOGLE_API_KEY not set — skipping pin_inline_gemini");
            return;
        }
    };

    let mut decls = synthetic_tools_gemini();
    decls.push(tool_search_gemini());

    let body = build_request_body(decls);
    let total_tokens = count_tokens_gemini(&api_key, &body).await;

    tracing::info!(total_tokens, "pin_inline_gemini baseline token count");
    assert!(total_tokens > 0, "inline token count should be positive");
}

/// Reduction check: 100 tools deferred (omitted) vs 100 tools inline.
///
/// Makes both `countTokens` calls within a single test so the comparison is
/// self-contained. Asserts `deferred < inline` and `(inline − deferred) > 1_000`.
///
/// Skipped when `GOOGLE_API_KEY` is not set.
#[tokio::test]
async fn pin_deferred_gemini() {
    let _ = tracing_subscriber::fmt::try_init();

    let api_key = match std::env::var("GOOGLE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::info!("GOOGLE_API_KEY not set — skipping pin_deferred_gemini");
            return;
        }
    };

    // --- inline: all 100 tools as functionDeclarations ---
    let mut inline_decls = synthetic_tools_gemini();
    inline_decls.push(tool_search_gemini());
    let inline_body = build_request_body(inline_decls);
    let inline_tokens = count_tokens_gemini(&api_key, &inline_body).await;

    // --- deferred: only ToolSearch in functionDeclarations (100 deferred tools omitted) ---
    let deferred_body = build_request_body(vec![tool_search_gemini()]);
    let deferred_tokens = count_tokens_gemini(&api_key, &deferred_body).await;

    let reduction = (inline_tokens as f64 - deferred_tokens as f64) / inline_tokens as f64;

    tracing::info!(
        inline_tokens,
        deferred_tokens,
        reduction_pct = format!("{:.1}%", reduction * 100.0),
        "Gemini runtime-expansion token reduction measured"
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
