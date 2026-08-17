//! Minimal MCP server fixture for ao-engine integration tests.
//!
//! Behaviors (controlled by `MCP_BEHAVIOR` env var):
//!   "crash"  — exits immediately (simulates handshake failure)
//!   default  — serves the MCP stdio protocol and exposes one "echo" tool

use std::io::{BufRead, Write};

fn main() {
    if std::env::var("MCP_BEHAVIOR").as_deref() == Ok("crash") {
        std::process::exit(1);
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line_result in stdin.lock().lines() {
        let line = match line_result {
            Ok(l) if !l.trim().is_empty() => l,
            Ok(_) => continue,
            Err(_) => break,
        };

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = req["method"].as_str().unwrap_or("").to_owned();
        let id = match req.get("id") {
            Some(v) => v.clone(),
            None => continue, // notification — no response needed
        };

        let result = match method.as_str() {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "fixture_mcp_server", "version": "0.1.0"}
            }),
            "tools/list" => serde_json::json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echoes the input back.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "description": "Text to echo"}
                        }
                    }
                }]
            }),
            "tools/call" => {
                let args = req["params"]["arguments"].clone();
                serde_json::json!({"content": [{"type": "text", "text": args.to_string()}]})
            }
            _ => serde_json::json!({}),
        };

        let response = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .unwrap();

        stdout.write_all(response.as_bytes()).unwrap();
        stdout.write_all(b"\n").unwrap();
        stdout.flush().unwrap();

        if method == "shutdown" {
            break;
        }
    }
}
