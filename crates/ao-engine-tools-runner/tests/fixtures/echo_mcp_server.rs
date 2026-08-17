//! Multi-behavior MCP test fixture binary.
//!
//! Used by the `mcp::client`, `mcp::schema_fetch`, `mcp::adapter`, and
//! `mcp::list_resources` / `mcp::read_resource` unit tests.
//! Set `MCP_BEHAVIOR` to control:
//! - (default / "normal"):        handle initialize + `tools/list` (1 tool) + echo back `tools/call` params
//! - "crash":                     exit immediately (simulates spawn/handshake failure)
//! - "bad_protocol":              handle initialize, then for tool calls send a JSON
//!                                object with `id` but neither `result` nor `error`
//! - "error_response":            handle initialize, then return a JSON-RPC error
//!                                (code -32603) for tool calls
//! - "hang_after_init":           handle initialize, then block forever on tool calls
//! - "is_error":                  return a `tools/call` response with `isError: true`
//! - "die_after_first_call":      respond normally to `initialize` + `tools/list`, reply
//!                                to the first `tools/call`, then exit — simulates a
//!                                server that crashes after one successful call
//! - "tools_list_malformed":      `tools/list` returns 2 well-formed tools + 1 with a
//!                                non-object inputSchema (for schema_fetch tests)
//! - "everything":                `tools/list` returns 1 tool; `prompts/list` returns
//!                                2 prompts ("greet" and "summarize"); `prompts/get`
//!                                returns a single-message body for each
//! - "tool_only":                 `tools/list` returns 1 tool; `prompts/list` returns
//!                                method-not-found (-32601) — verifies zero MCP skills
//! - "with_capabilities":         initialize returns capabilities with resources/tools/prompts
//!                                (for capability-capture tests)
//! - "send_progress":             `tools/call` sends 3 `notifications/progress` messages
//!                                (echoing the progressToken from `_meta`) then responds
//! - "with_annotations":          `tools/list` returns two tools; the first (`read_file`)
//!                                has full annotations (`readOnlyHint:true`, `destructiveHint:false`,
//!                                `openWorldHint:false`, `title:"Read File"`); the second
//!                                (`write_db`) has `readOnlyHint:false`, `destructiveHint:true`,
//!                                `openWorldHint:true` and no `title`
//! - "with_resources":            initialize returns capabilities with `resources: {}`;
//!                                `resources/list` returns 2 text resources;
//!                                `resources/read` for `resource://notes.txt` returns text
//!                                content; other URIs return an empty contents array
//! - "resources_paginated":       initialize returns capabilities with `resources: {}`;
//!                                `resources/list` with no cursor returns 1 resource + nextCursor;
//!                                with that cursor returns 1 more resource + no nextCursor
//! - "with_blob_resource":        initialize returns capabilities with `resources: {}`;
//!                                `resources/list` returns 1 binary resource;
//!                                `resources/read` for `resource://data.pdf` returns a
//!                                base64 blob with mimeType `application/pdf`
//! - "tools_paginated":           `tools/list` with no cursor returns 1 tool (`alpha`) +
//!                                nextCursor; with that cursor returns 1 more tool (`beta`)
//!                                and no nextCursor (for cursor-pagination tests)
//! - "tools_with_meta":           `tools/list` returns 2 tools; the first (`smart_query`)
//!                                has `_meta: { "anthropic/alwaysLoad": true,
//!                                "anthropic/searchHint": "database query" }`; the second
//!                                (`optional_tool`) has no `_meta` hints
//! - "echo_meta":                 `tools/call` echoes the `_meta` object from the incoming
//!                                params back as text content (for outbound-meta correlation tests)

use std::io::{self, BufRead, Write};

fn main() {
    let behavior = std::env::var("MCP_BEHAVIOR").unwrap_or_else(|_| "normal".to_string());

    if behavior == "crash" {
        std::process::exit(1);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(_) => break,
        };

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);

        // Skip notifications (no id)
        if id.is_none() {
            continue;
        }

        let response: Option<serde_json::Value> = match method {
            "initialize" => {
                let caps = if matches!(
                    behavior.as_str(),
                    "with_capabilities"
                    | "with_resources"
                    | "resources_paginated"
                    | "with_blob_resource"
                ) {
                    serde_json::json!({ "resources": {}, "tools": {}, "prompts": {} })
                } else {
                    serde_json::json!({})
                };
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": caps,
                        "serverInfo": { "name": "echo-mcp-server", "version": "0.1.0" }
                    }
                }))
            }
            "prompts/list" => match behavior.as_str() {
                "everything" => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "prompts": [
                            { "name": "greet", "description": "A greeting prompt" },
                            { "name": "summarize", "description": "Summarize something" }
                        ]
                    }
                })),
                // All other behaviors (including "tool_only") return method-not-found.
                _ => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })),
            },
            "prompts/get" => match behavior.as_str() {
                "everything" => {
                    let prompt_name = params
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let text = match prompt_name {
                        "greet" => "Say hello warmly.",
                        "summarize" => "Summarize the provided text concisely.",
                        _ => "Prompt body.",
                    };
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "messages": [
                                {
                                    "role": "user",
                                    "content": { "type": "text", "text": text }
                                }
                            ]
                        }
                    }))
                }
                _ => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })),
            },
            "tools/list" => match behavior.as_str() {
                "with_annotations" => {
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "read_file",
                                    "description": "reads a file without modifying it",
                                    "inputSchema": { "type": "object" },
                                    "annotations": {
                                        "readOnlyHint": true,
                                        "destructiveHint": false,
                                        "openWorldHint": false,
                                        "title": "Read File"
                                    }
                                },
                                {
                                    "name": "write_db",
                                    "description": "writes to an external database",
                                    "inputSchema": { "type": "object" },
                                    "annotations": {
                                        "readOnlyHint": false,
                                        "destructiveHint": true,
                                        "openWorldHint": true
                                    }
                                }
                            ]
                        }
                    }))
                }
                "tools_list_malformed" => {
                    // 2 well-formed tools + 1 with a non-object inputSchema
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "echoes back its arguments",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "x": { "type": "integer" } }
                                    }
                                },
                                {
                                    "name": "ping",
                                    "description": "ping tool",
                                    "inputSchema": { "type": "object" }
                                },
                                {
                                    "name": "broken_tool",
                                    "description": "has a non-object inputSchema",
                                    "inputSchema": "not-an-object"
                                }
                            ]
                        }
                    }))
                }
                "tools_paginated" => {
                    let cursor_in = params.get("cursor").and_then(|c| c.as_str());
                    if cursor_in.is_none() {
                        // Page 1: return 1 tool + nextCursor
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "alpha",
                                    "description": "first paginated tool",
                                    "inputSchema": { "type": "object" }
                                }],
                                "nextCursor": "cursor-page2"
                            }
                        }))
                    } else {
                        // Page 2: return 1 more tool, no nextCursor
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "beta",
                                    "description": "second paginated tool",
                                    "inputSchema": { "type": "object" }
                                }]
                            }
                        }))
                    }
                }
                "tools_with_meta" => {
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "smart_query",
                                    "description": "queries an external data source",
                                    "inputSchema": { "type": "object" },
                                    "_meta": {
                                        "anthropic/alwaysLoad": true,
                                        "anthropic/searchHint": "database query"
                                    }
                                },
                                {
                                    "name": "optional_tool",
                                    "description": "a deferred helper tool",
                                    "inputSchema": { "type": "object" }
                                }
                            ]
                        }
                    }))
                }
                _ => {
                    // Normal: return 1 well-formed tool
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "echoes back its arguments",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "x": { "type": "integer" } }
                                    }
                                }
                            ]
                        }
                    }))
                }
            },
            "tools/call" => match behavior.as_str() {
                "bad_protocol" => {
                    // Valid JSON-RPC id but missing both result and error
                    Some(serde_json::json!({ "jsonrpc": "2.0", "id": id }))
                }
                "error_response" => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": "Internal error" }
                })),
                "hang_after_init" => {
                    // Block until the test process sends SIGKILL
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    None
                }
                "die_after_first_call" => {
                    // Respond normally to this call, then exit to simulate a server crash
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": params.to_string() }],
                            "isError": false
                        }
                    });
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = writeln!(out, "{s}");
                        let _ = out.flush();
                    }
                    std::process::exit(0);
                }
                "is_error" => {
                    // Return a successful JSON-RPC response but with isError: true
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": "tool execution failed" }],
                            "isError": true
                        }
                    }))
                }
                "send_progress" => {
                    // Extract the progressToken from _meta, send 3 progress notifications,
                    // then return a normal result.
                    let token = params
                        .get("_meta")
                        .and_then(|m| m.get("progressToken"))
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());

                    if let Some(ref token) = token {
                        for step in 1u32..=3 {
                            let notif = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": token,
                                    "progress": step,
                                    "total": 3,
                                    "message": format!("step {step} of 3")
                                }
                            });
                            if let Ok(s) = serde_json::to_string(&notif) {
                                let _ = writeln!(out, "{s}");
                                let _ = out.flush();
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }

                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": "done" }],
                            "isError": false
                        }
                    }))
                }
                "echo_meta" => {
                    // Echo only the _meta object from the incoming params back as
                    // text content. Lets tests verify what was injected into _meta
                    // without having to parse the full parameter blob.
                    let meta = params.get("_meta").cloned().unwrap_or(serde_json::json!(null));
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": meta.to_string() }],
                            "isError": false
                        }
                    }))
                }
                _ => {
                    // Echo the params back as text content
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": params.to_string() }],
                            "isError": false
                        }
                    }))
                }
            },
            "resources/list" => match behavior.as_str() {
                "with_resources" => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resources": [
                            {
                                "uri": "resource://notes.txt",
                                "name": "Notes",
                                "description": "A plain-text notes file",
                                "mimeType": "text/plain"
                            },
                            {
                                "uri": "resource://config.json",
                                "name": "Config",
                                "description": "Server configuration",
                                "mimeType": "application/json"
                            }
                        ]
                    }
                })),
                "resources_paginated" => {
                    let cursor_in = params.get("cursor").and_then(|c| c.as_str());
                    if cursor_in.is_none() {
                        // Page 1: return 1 resource + nextCursor
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "resources": [
                                    {
                                        "uri": "resource://page1/item1",
                                        "name": "Page 1 Item"
                                    }
                                ],
                                "nextCursor": "cursor-page2"
                            }
                        }))
                    } else {
                        // Page 2: return 1 resource, no nextCursor
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "resources": [
                                    {
                                        "uri": "resource://page2/item1",
                                        "name": "Page 2 Item"
                                    }
                                ]
                            }
                        }))
                    }
                }
                "with_blob_resource" => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resources": [
                            {
                                "uri": "resource://data.pdf",
                                "name": "Data PDF",
                                "description": "A binary PDF document",
                                "mimeType": "application/pdf"
                            }
                        ]
                    }
                })),
                // All other behaviors: method not found
                _ => Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })),
            },
            "resources/read" => {
                let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                match behavior.as_str() {
                    "with_resources" => {
                        let contents = if uri == "resource://notes.txt" {
                            serde_json::json!([{
                                "uri": uri,
                                "mimeType": "text/plain",
                                "text": "This is the content of notes.txt from the MCP server."
                            }])
                        } else if uri == "resource://config.json" {
                            serde_json::json!([{
                                "uri": uri,
                                "mimeType": "application/json",
                                "text": "{\"enabled\": true, \"version\": 1}"
                            }])
                        } else {
                            serde_json::json!([])
                        };
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "contents": contents }
                        }))
                    }
                    "with_blob_resource" => {
                        // "hello" encoded as standard base64: aGVsbG8=
                        let contents = if uri == "resource://data.pdf" {
                            serde_json::json!([{
                                "uri": uri,
                                "mimeType": "application/pdf",
                                "blob": "aGVsbG8="
                            }])
                        } else {
                            serde_json::json!([])
                        };
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "contents": contents }
                        }))
                    }
                    _ => Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "Method not found" }
                    })),
                }
            }
            "shutdown" => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null
                });
                let _ = writeln!(out, "{}", serde_json::to_string(&resp).unwrap());
                let _ = out.flush();
                break;
            }
            _ => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            })),
        };

        if let Some(resp) = response {
            if let Ok(s) = serde_json::to_string(&resp) {
                let _ = writeln!(out, "{s}");
                let _ = out.flush();
            }
        }
    }
}
