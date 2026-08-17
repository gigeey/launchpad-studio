//! Fetch tools and prompts from a running MCP server, normalising the results
//! into descriptor types ready for adapter construction and skill registration.

use serde_json::Value;
use tracing::warn;

use super::client::{McpClientHandle, McpError};

/// Maximum number of Unicode codepoints allowed in a single tool description.
///
/// Verbose MCP servers sometimes ship multi-kilobyte descriptions that inflate
/// the model's tool list disproportionately. Any description that exceeds this
/// cap is truncated at the boundary and appended with [`DESCRIPTION_TRUNCATION_SUFFIX`].
const MAX_DESCRIPTION_CHARS: usize = 10_000;

/// Suffix appended to a description that was cut at [`MAX_DESCRIPTION_CHARS`].
const DESCRIPTION_TRUNCATION_SUFFIX: &str = " … [description truncated]";

/// Sanitize a string coming from an external MCP server.
///
/// Replaces null bytes and non-whitespace ASCII control characters with a
/// space so that a hostile server cannot inject invisible control sequences
/// into our tool registry entries. Printable characters, standard whitespace
/// (tab, newline, carriage return), and all non-ASCII Unicode are preserved.
fn sanitize_mcp_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            // Keep standard whitespace as-is; replace other C0 control chars and DEL.
            let cp = c as u32;
            if c == '\0' || (cp < 0x20 && c != '\t' && c != '\n' && c != '\r') || c == '\x7f' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Truncate `desc` to at most [`MAX_DESCRIPTION_CHARS`] codepoints.
///
/// When truncation is applied the returned string ends with
/// [`DESCRIPTION_TRUNCATION_SUFFIX`] so the model knows the description
/// was cut short.
fn maybe_truncate_description(desc: String) -> String {
    if desc.chars().count() <= MAX_DESCRIPTION_CHARS {
        desc
    } else {
        let truncated: String = desc.chars().take(MAX_DESCRIPTION_CHARS).collect();
        format!("{truncated}{DESCRIPTION_TRUNCATION_SUFFIX}")
    }
}

/// Behavioural hints attached to an MCP tool entry.
///
/// All fields are optional — an absent field is `None`, not a boolean default.
/// Callers that need a safe default must check `is_some()` themselves.
/// For example, [`McpToolAdapter::is_concurrency_safe`] treats an absent
/// `read_only_hint` as `false` (write-intent assumed), which is the
/// conservative choice.
///
/// [`McpToolAdapter::is_concurrency_safe`]: crate::mcp::adapter::McpToolAdapter
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpToolAnnotations {
    /// When `Some(true)`, the tool promises it never modifies state and
    /// concurrent invocations are safe.  When `None` or `Some(false)`,
    /// callers must assume the tool may write.
    pub read_only_hint: Option<bool>,
    /// When `Some(true)`, the tool may perform irreversible or destructive
    /// operations.  `None` means the server did not declare intent.
    pub destructive_hint: Option<bool>,
    /// When `Some(true)`, the tool can reach state outside the local
    /// environment (external APIs, remote databases, etc.).
    pub open_world_hint: Option<bool>,
    /// A human-readable display name for the tool, if the server provided one.
    /// Distinct from the machine-facing `raw_name`.
    pub title: Option<String>,
}

/// Descriptor for a single tool advertised by an MCP server.
///
/// `raw_name` is the server-supplied name BEFORE the `mcp__<server>__` namespace prefix.
/// `description` has already been sanitized and truncated to at most
/// [`MAX_DESCRIPTION_CHARS`] codepoints.
#[derive(Debug, Clone)]
pub struct McpToolDescriptor {
    pub raw_name: String,
    pub description: String,
    pub input_schema: Value,
    /// The server's declared `outputSchema` for this tool, if it provided one.
    ///
    /// A server that declares this is committing to a stable, machine-readable
    /// shape for the tool's structured result — the tool's output can be
    /// treated as deterministically parseable at bind time rather than
    /// something we must discover empirically by calling the tool and
    /// inspecting what comes back. Absent when the server omits the field,
    /// or when the field is present but not a JSON object.
    pub output_schema: Option<Value>,
    /// Behavioural hints from the server's `annotations` object, if present.
    /// All fields default to `None` when the server omits `annotations`.
    pub annotations: McpToolAnnotations,
    /// Optional search hint from `tool._meta['anthropic/searchHint']`, if provided.
    ///
    /// Intended to improve deferred-tool search relevance — the hint text
    /// describes extra keywords or context that help resolve the tool by topic
    /// rather than by exact name.
    pub search_hint: Option<String>,
    /// When `true`, this tool must always be loaded regardless of the
    /// server-level `McpLoadingPolicy`.
    ///
    /// Derived from `tool._meta['anthropic/alwaysLoad']` in the `tools/list`
    /// response. Takes precedence over the per-server loading policy so that
    /// critical helper tools can guarantee they appear in the model's tool list
    /// even when the server is configured for deferred loading.
    pub always_load: bool,
}

/// Descriptor for a single prompt advertised by an MCP server.
///
/// `body` is the concatenated text content returned by `prompts/get` with no
/// arguments, which becomes the inline skill body.
#[derive(Debug, Clone)]
pub struct McpPromptDescriptor {
    pub raw_name: String,
    pub description: String,
    pub body: String,
}

/// Call `tools/list` on `client` and return well-formed descriptors.
///
/// Follows `nextCursor` pagination until the server returns a page with no
/// cursor, collecting tools from all pages into a single `Vec`.
///
/// Each description is sanitized (non-printable control characters replaced
/// with spaces) and truncated to [`MAX_DESCRIPTION_CHARS`] codepoints before
/// being stored.
///
/// The optional `outputSchema` field is also read (into
/// [`McpToolDescriptor::output_schema`]) but, unlike `inputSchema`, its
/// absence or malformation never causes the tool entry to be skipped — it
/// simply yields `None`.
///
/// Malformed per-tool entries (missing `name`, missing `inputSchema`, or
/// `inputSchema` not a JSON object) are skipped with a `tracing::warn` line
/// tagged `mcp_server` and (when available) `mcp_tool`; other tools from the
/// same server and page are still included.
///
/// A missing or non-array `tools` field on any page emits a single warn and
/// terminates pagination — not an error, because the server is reachable and
/// tools from earlier pages have already been collected.
pub async fn fetch_tools(client: &McpClientHandle) -> Result<Vec<McpToolDescriptor>, McpError> {
    let server_name = client.name().to_string();
    let mut descriptors: Vec<McpToolDescriptor> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let params = match &cursor {
            Some(c) => serde_json::json!({ "cursor": c }),
            None => serde_json::json!({}),
        };

        let result = client.call("tools/list", params).await?;

        let tools_array = match result.get("tools").and_then(|t| t.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                warn!(
                    mcp_server = %server_name,
                    "tools/list response missing 'tools' array"
                );
                break;
            }
        };

        for tool in &tools_array {
            // `name` is required
            let raw_name = match tool.get("name").and_then(|n| n.as_str()) {
                Some(n) => sanitize_mcp_text(n),
                None => {
                    warn!(
                        mcp_server = %server_name,
                        "tool entry missing 'name' field — skipping"
                    );
                    continue;
                }
            };

            // `description` is optional; default to empty string.
            // Sanitize and truncate to prevent runaway context inflation.
            let description = {
                let raw = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                maybe_truncate_description(sanitize_mcp_text(raw))
            };

            // `inputSchema` is required and must be a JSON object
            let input_schema = match tool.get("inputSchema") {
                Some(s) if s.is_object() => s.clone(),
                Some(_) => {
                    warn!(
                        mcp_server = %server_name,
                        mcp_tool = %raw_name,
                        "tool 'inputSchema' is not a JSON object — skipping"
                    );
                    continue;
                }
                None => {
                    warn!(
                        mcp_server = %server_name,
                        mcp_tool = %raw_name,
                        "tool entry missing 'inputSchema' — skipping"
                    );
                    continue;
                }
            };

            let output_schema = parse_output_schema(tool);

            let annotations = parse_annotations(tool.get("annotations"));

            // Read Anthropic-namespace _meta hints from the tools/list entry.
            let tool_meta = tool.get("_meta");
            let search_hint = tool_meta
                .and_then(|m| m.get("anthropic/searchHint"))
                .and_then(|v| v.as_str())
                .map(sanitize_mcp_text);
            let always_load = tool_meta
                .and_then(|m| m.get("anthropic/alwaysLoad"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            descriptors.push(McpToolDescriptor {
                raw_name,
                description,
                input_schema,
                output_schema,
                annotations,
                search_hint,
                always_load,
            });
        }

        // Follow nextCursor for paginated servers; stop when the page has none.
        cursor = result
            .get("nextCursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    Ok(descriptors)
}

/// Extract a tool's declared `outputSchema` from its `tools/list` entry.
///
/// Returns `None` when the field is absent or is present but not a JSON
/// object — a malformed `outputSchema` never causes the surrounding tool
/// entry to be skipped, unlike a malformed `inputSchema`.
fn parse_output_schema(tool: &Value) -> Option<Value> {
    tool.get("outputSchema").filter(|s| s.is_object()).cloned()
}

/// Extract [`McpToolAnnotations`] from an optional JSON `annotations` value.
///
/// Returns [`McpToolAnnotations::default`] when `value` is absent or not a
/// JSON object — a server that omits `annotations` is treated the same as
/// one that sends an empty object.
fn parse_annotations(value: Option<&Value>) -> McpToolAnnotations {
    let obj = match value.and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return McpToolAnnotations::default(),
    };
    McpToolAnnotations {
        read_only_hint: obj.get("readOnlyHint").and_then(|v| v.as_bool()),
        destructive_hint: obj.get("destructiveHint").and_then(|v| v.as_bool()),
        open_world_hint: obj.get("openWorldHint").and_then(|v| v.as_bool()),
        title: obj.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

/// Call `prompts/list` then `prompts/get` (no args) on `client` and return
/// well-formed descriptors with resolved bodies.
///
/// A server that returns method-not-found (-32601) for `prompts/list` yields
/// an empty `Vec` — not an error. This mirrors [`fetch_tools`]'s "reachable
/// but empty" contract so tool-only MCP servers are handled gracefully.
///
/// Malformed per-prompt entries (missing `name`) are skipped with a warn.
/// A missing or non-array `prompts` field returns empty (not an error).
/// If `prompts/get` fails for an individual prompt, that prompt is skipped
/// with a warn; other prompts are still included.
pub async fn fetch_prompts(client: &McpClientHandle) -> Result<Vec<McpPromptDescriptor>, McpError> {
    let server_name = client.name().to_string();

    let list_result = match client.call("prompts/list", serde_json::json!({})).await {
        Ok(r) => r,
        Err(McpError::CallError { code: -32601, .. }) => {
            // Server does not support prompts — return empty, not an error.
            return Ok(vec![]);
        }
        Err(e) => return Err(e),
    };

    let prompts_array = match list_result.get("prompts").and_then(|p| p.as_array()) {
        Some(arr) => arr.clone(),
        None => {
            warn!(
                mcp_server = %server_name,
                "prompts/list response missing 'prompts' array"
            );
            return Ok(vec![]);
        }
    };

    let mut descriptors = Vec::with_capacity(prompts_array.len());
    for prompt in &prompts_array {
        let raw_name = match prompt.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => {
                warn!(
                    mcp_server = %server_name,
                    "prompt entry missing 'name' field — skipping"
                );
                continue;
            }
        };

        let description = prompt
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();

        // Resolve the body by calling prompts/get with no arguments.
        let get_result = match client
            .call("prompts/get", serde_json::json!({ "name": raw_name }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    mcp_server = %server_name,
                    mcp_prompt = %raw_name,
                    "prompts/get failed: {e} — skipping"
                );
                continue;
            }
        };

        let body = extract_prompt_body(&get_result, &server_name, &raw_name);
        descriptors.push(McpPromptDescriptor { raw_name, description, body });
    }

    Ok(descriptors)
}

/// Extract concatenated text from a `prompts/get` result's `messages` array.
///
/// Joins text content blocks with a newline separator. Returns an empty string
/// if no text content is found, emitting a single warn in that case.
fn extract_prompt_body(result: &Value, server_name: &str, prompt_name: &str) -> String {
    let messages = match result.get("messages").and_then(|m| m.as_array()) {
        Some(arr) => arr,
        None => {
            warn!(
                mcp_server = %server_name,
                mcp_prompt = %prompt_name,
                "prompts/get response missing 'messages' array"
            );
            return String::new();
        }
    };

    let mut parts: Vec<&str> = Vec::new();
    for msg in messages {
        let content = msg.get("content");
        // Content may be a string or an object with { type, text }.
        if let Some(text) = content.and_then(|c| c.as_str()) {
            parts.push(text);
        } else if let Some(text) = content
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
        {
            parts.push(text);
        }
    }

    if parts.is_empty() {
        warn!(
            mcp_server = %server_name,
            mcp_prompt = %prompt_name,
            "prompts/get response had no text content"
        );
    }

    parts.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::mcp::test_support::echo_server_bin;

    #[tokio::test]
    async fn fetch_tools_returns_well_formed_descriptors() {
        let bin = echo_server_bin();
        let client =
            crate::mcp::client::McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &HashMap::new())
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");

        assert_eq!(tools.len(), 1, "normal fixture returns 1 tool");
        assert_eq!(tools[0].raw_name, "echo");
        assert!(!tools[0].description.is_empty());
        assert!(tools[0].input_schema.is_object());
        // No _meta hints in the normal fixture
        assert!(tools[0].search_hint.is_none());
        assert!(!tools[0].always_load);

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_tools_skips_malformed_entry_returns_two() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "tools_list_malformed".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");

        // 2 well-formed + 1 malformed (non-object inputSchema) → len == 2
        assert_eq!(tools.len(), 2, "malformed fixture skips broken_tool, returns 2");
        let names: Vec<&str> = tools.iter().map(|t| t.raw_name.as_str()).collect();
        assert!(names.contains(&"echo"), "echo should be present");
        assert!(names.contains(&"ping"), "ping should be present");
        assert!(!names.contains(&"broken_tool"), "broken_tool should be skipped");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_prompts_returns_descriptors_from_everything_server() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "everything".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("everything", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let prompts = fetch_prompts(&client).await.expect("fetch_prompts should succeed");

        assert_eq!(prompts.len(), 2, "everything fixture returns 2 prompts");
        let names: Vec<&str> = prompts.iter().map(|p| p.raw_name.as_str()).collect();
        assert!(names.contains(&"greet"), "greet should be present");
        assert!(names.contains(&"summarize"), "summarize should be present");

        let greet = prompts.iter().find(|p| p.raw_name == "greet").unwrap();
        assert!(!greet.body.is_empty(), "greet should have a non-empty body");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_prompts_returns_empty_for_tool_only_server() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "tool_only".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("tool_only", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        // A server returning method-not-found for prompts/list yields zero MCP skills.
        let prompts = fetch_prompts(&client).await.expect("method-not-found should not be an error");
        assert!(prompts.is_empty(), "tool-only server should return zero prompts");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_prompts_returns_empty_for_normal_server() {
        let bin = echo_server_bin();
        let client =
            crate::mcp::client::McpClientHandle::spawn("normal", bin.to_str().unwrap(), &[], &HashMap::new())
                .await
                .expect("should spawn echo_mcp_server");

        // Normal behavior also returns method-not-found for prompts/list.
        let prompts = fetch_prompts(&client).await.expect("should succeed with empty result");
        assert!(prompts.is_empty(), "normal server returns no prompts");

        client.shutdown().await;
    }

    // ── Annotation parsing tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tools_parses_full_annotations() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_annotations".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("ann", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");
        assert_eq!(tools.len(), 2, "with_annotations fixture returns 2 tools");

        let read_file = tools.iter().find(|t| t.raw_name == "read_file").unwrap();
        assert_eq!(
            read_file.annotations.read_only_hint,
            Some(true),
            "read_file should have readOnlyHint: true"
        );
        assert_eq!(
            read_file.annotations.destructive_hint,
            Some(false),
            "read_file should have destructiveHint: false"
        );
        assert_eq!(
            read_file.annotations.open_world_hint,
            Some(false),
            "read_file should have openWorldHint: false"
        );
        assert_eq!(
            read_file.annotations.title.as_deref(),
            Some("Read File"),
            "read_file should have title 'Read File'"
        );

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_tools_parses_partial_annotations_no_title() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_annotations".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("ann2", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");
        let write_db = tools.iter().find(|t| t.raw_name == "write_db").unwrap();

        assert_eq!(write_db.annotations.read_only_hint, Some(false));
        assert_eq!(write_db.annotations.destructive_hint, Some(true));
        assert_eq!(write_db.annotations.open_world_hint, Some(true));
        assert!(
            write_db.annotations.title.is_none(),
            "write_db has no title annotation"
        );

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_tools_annotations_absent_yields_all_none() {
        let bin = echo_server_bin();
        // Normal behavior returns the echo tool without any annotations object.
        let client =
            crate::mcp::client::McpClientHandle::spawn("plain", bin.to_str().unwrap(), &[], &HashMap::new())
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");
        assert_eq!(tools.len(), 1);
        let echo = &tools[0];
        assert_eq!(
            echo.annotations,
            McpToolAnnotations::default(),
            "tool without annotations should have all-None annotations"
        );

        client.shutdown().await;
    }

    #[test]
    fn parse_annotations_all_fields() {
        let v = serde_json::json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "openWorldHint": true,
            "title": "My Tool"
        });
        let ann = parse_annotations(Some(&v));
        assert_eq!(ann.read_only_hint, Some(true));
        assert_eq!(ann.destructive_hint, Some(false));
        assert_eq!(ann.open_world_hint, Some(true));
        assert_eq!(ann.title.as_deref(), Some("My Tool"));
    }

    #[test]
    fn parse_annotations_absent_gives_default() {
        assert_eq!(parse_annotations(None), McpToolAnnotations::default());
    }

    #[test]
    fn parse_annotations_non_object_gives_default() {
        let v = serde_json::json!("not-an-object");
        assert_eq!(parse_annotations(Some(&v)), McpToolAnnotations::default());
    }

    // ── outputSchema parsing tests ────────────────────────────────────────────

    #[test]
    fn parse_output_schema_present_populates_verbatim() {
        let tool = serde_json::json!({
            "name": "get_weather",
            "inputSchema": { "type": "object" },
            "outputSchema": {
                "type": "object",
                "properties": { "temperature": { "type": "number" } },
                "required": ["temperature"]
            }
        });
        let expected = tool.get("outputSchema").unwrap().clone();
        assert_eq!(parse_output_schema(&tool), Some(expected));
    }

    #[test]
    fn parse_output_schema_absent_yields_none() {
        let tool = serde_json::json!({
            "name": "get_weather",
            "inputSchema": { "type": "object" }
        });
        assert_eq!(parse_output_schema(&tool), None);
    }

    // ── Description truncation tests ──────────────────────────────────────────

    #[test]
    fn description_at_limit_is_not_truncated() {
        let at_limit: String = "a".repeat(MAX_DESCRIPTION_CHARS);
        let out = maybe_truncate_description(at_limit.clone());
        assert_eq!(out, at_limit, "description exactly at cap must not be truncated");
        assert!(!out.contains("truncated"), "no truncation suffix should appear");
    }

    #[test]
    fn description_over_limit_is_truncated_with_suffix() {
        let over: String = "b".repeat(MAX_DESCRIPTION_CHARS + 50);
        let out = maybe_truncate_description(over);
        assert!(
            out.ends_with(DESCRIPTION_TRUNCATION_SUFFIX),
            "truncated description must end with the truncation suffix"
        );
        let body_chars = out.chars().count() - DESCRIPTION_TRUNCATION_SUFFIX.chars().count();
        assert_eq!(body_chars, MAX_DESCRIPTION_CHARS, "truncated body must be exactly MAX_DESCRIPTION_CHARS");
    }

    // ── Unicode sanitization tests ────────────────────────────────────────────

    #[test]
    fn sanitize_replaces_null_byte() {
        let s = "hello\0world";
        let out = sanitize_mcp_text(s);
        assert_eq!(out, "hello world", "null byte should be replaced with space");
    }

    #[test]
    fn sanitize_replaces_control_chars_except_whitespace() {
        // BEL (\x07), form-feed (\x0C) → replaced; tab/newline/CR preserved
        let s = "a\x07b\x0Cc\td\ne\rf";
        let out = sanitize_mcp_text(s);
        assert!(!out.contains('\x07'), "BEL should be replaced");
        assert!(!out.contains('\x0C'), "form-feed should be replaced");
        assert!(out.contains('\t'), "tab should be preserved");
        assert!(out.contains('\n'), "newline should be preserved");
        assert!(out.contains('\r'), "carriage return should be preserved");
    }

    #[test]
    fn sanitize_preserves_emoji_and_unicode() {
        let s = "emoji 🦀 and cjk 日本語";
        let out = sanitize_mcp_text(s);
        assert_eq!(out, s, "valid Unicode including emoji must be preserved unchanged");
    }

    // ── _meta hint tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tools_reads_anthropic_meta_hints() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "tools_with_meta".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("meta", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");

        let smart = tools.iter().find(|t| t.raw_name == "smart_query")
            .expect("smart_query should be present");
        assert_eq!(
            smart.search_hint.as_deref(),
            Some("database query"),
            "anthropic/searchHint should be read into search_hint"
        );
        assert!(smart.always_load, "anthropic/alwaysLoad:true should set always_load=true");

        // Tool without _meta: no hints, always_load defaults to false
        let optional = tools.iter().find(|t| t.raw_name == "optional_tool")
            .expect("optional_tool should be present");
        assert!(optional.search_hint.is_none());
        assert!(!optional.always_load);

        client.shutdown().await;
    }

    // ── Cursor pagination tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tools_follows_next_cursor_across_two_pages() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "tools_paginated".to_string());

        let client =
            crate::mcp::client::McpClientHandle::spawn("paged", bin.to_str().unwrap(), &[], &env)
                .await
                .expect("should spawn echo_mcp_server");

        let tools = fetch_tools(&client).await.expect("fetch_tools should succeed");

        let names: Vec<&str> = tools.iter().map(|t| t.raw_name.as_str()).collect();
        assert_eq!(names.len(), 2, "pagination should collect tools from both pages");
        assert!(names.contains(&"alpha"), "page 1 tool should be present");
        assert!(names.contains(&"beta"), "page 2 tool should be present");

        client.shutdown().await;
    }
}
