//! Canonical `CompletionRequest` → Gemini `generateContent` body translation.
//!
//! ## Gemini-specific dialect decisions
//!
//! - `systemInstruction` carries the system prompt as a top-level field outside
//!   `contents[]` — the most common mis-placement is inlining it as a system role
//!   inside `contents[]`.
//! - `tools` uses Gemini's `functionDeclarations` envelope:
//!   `[{ "functionDeclarations": [...] }]`
//! - `generationConfig` is omitted entirely when the request carries no generation
//!   parameters (v1 `CompletionRequest` has no temperature / max_tokens /
//!   stop_sequences fields).
//! - `safetySettings` is omitted when not present — Gemini defaults apply.

use ao_engine_tools_runner::{
    message::{Message, MessageNormalizer},
    provider::{CompletionRequest, ProviderError},
};
use serde_json::{json, Value};

/// Build the Gemini `generateContent` POST body from a [`CompletionRequest`].
///
/// The returned `Value` is ready to be serialised as the HTTP request body.
/// System prompt is placed in `systemInstruction`, not `contents[]`.
/// Tools are wrapped in the `functionDeclarations` envelope.
pub fn build_request_body(
    request: &CompletionRequest,
    normalizer: &dyn MessageNormalizer,
) -> Result<Value, ProviderError> {
    // System messages are carried via systemInstruction; filter them out of the array.
    let non_system: Vec<Message> = request
        .messages
        .iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .cloned()
        .collect();

    // Translate canonical messages → Gemini contents[].
    let contents = normalizer
        .to_provider(&non_system)
        .map_err(|e| ProviderError::Transport(format!("message normalizer error: {e}")))?;

    let mut body = serde_json::Map::new();
    body.insert("contents".into(), contents);

    // systemInstruction is a top-level field outside contents[].
    if let Some(system) = &request.system_prompt {
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": system }] }),
        );
    }

    // Tools use Gemini's functionDeclarations envelope.
    // Deferred tools not yet resolved by ToolSearch are omitted entirely
    // (runtime-expansion fallback — Gemini has no native defer_loading equivalent).
    let visible_tools: Vec<Value> = request
        .tools
        .iter()
        .filter(|t| {
            !request.deferred_tools.contains(&t.name)
                || request.loaded_deferred_tools.contains(&t.name)
        })
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect();

    if !visible_tools.is_empty() {
        body.insert(
            "tools".into(),
            json!([{ "functionDeclarations": visible_tools }]),
        );
    }

    // generationConfig: omitted in v1 — CompletionRequest carries no generation params.
    // safetySettings: omitted in v1 — rely on Gemini defaults.

    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_runner::{
        message::{ContentBlock, Message, MessageNormalizer, NormalizerError},
        provider::{CompletionRequest, ToolSpec},
    };
    use serde_json::{json, Value};

    // Minimal Gemini-shaped normalizer for request builder tests.
    // Converts User/Assistant/ToolResult messages to Gemini contents[].
    // Full production encoding lives in messages.rs.
    struct SimpleGeminiNorm;

    impl MessageNormalizer for SimpleGeminiNorm {
        fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
            let mut contents = Vec::new();
            for msg in messages {
                match msg {
                    Message::System { .. } => {
                        return Err(NormalizerError::Unrepresentable(
                            "System messages must be handled by request.rs".into(),
                        ));
                    }
                    Message::User { content } => {
                        let parts: Vec<Value> = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(json!({ "text": text })),
                                _ => None,
                            })
                            .collect();
                        contents.push(json!({ "role": "user", "parts": parts }));
                    }
                    Message::Assistant { content } => {
                        let parts: Vec<Value> = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(json!({ "text": text })),
                                ContentBlock::ToolUse { name, input, .. } => Some(
                                    json!({ "functionCall": { "name": name, "args": input } }),
                                ),
                                _ => None,
                            })
                            .collect();
                        contents.push(json!({ "role": "model", "parts": parts }));
                    }
                    Message::ToolResult { content, .. } => {
                        // Stub shape — see messages.rs for the full functionResponse encoding.
                        let text: String = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        contents.push(json!({
                            "role": "user",
                            "parts": [{ "functionResponse": { "name": "stub", "response": { "output": text } } }]
                        }));
                    }
                }
            }
            Ok(Value::Array(contents))
        }

        fn from_provider(&self, _value: Value) -> Result<Vec<Message>, NormalizerError> {
            Err(NormalizerError::Unrepresentable(
                "from_provider not exercised in request builder tests".into(),
            ))
        }
    }

    fn fixture_text_only_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text {
                    text: "Hello, world!".into(),
                }],
            }],
            system_prompt: None,
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        }
    }

    #[test]
    fn build_text_only_request_has_contents_no_optional_fields() {
        let req = fixture_text_only_request();
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        let contents = body["contents"].as_array().expect("contents must be array");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert!(body.get("systemInstruction").is_none(), "systemInstruction must be absent");
        assert!(body.get("tools").is_none(), "tools must be absent");
        assert!(body.get("generationConfig").is_none(), "generationConfig must be absent");
    }

    #[test]
    fn build_request_with_system_prompt_emits_system_instruction_outside_contents() {
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "Hi".into() }],
            }],
            system_prompt: Some("You are helpful.".into()),
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        let si = &body["systemInstruction"];
        assert!(si.is_object(), "systemInstruction must be an object");
        let parts = si["parts"].as_array().expect("systemInstruction.parts must be array");
        assert_eq!(parts[0]["text"], "You are helpful.");

        // System content must NOT appear as a role inside contents[].
        let contents = body["contents"].as_array().unwrap();
        for item in contents {
            assert_ne!(item["role"], "system", "system role must not appear in contents[]");
        }
    }

    #[test]
    fn build_request_with_tools_emits_function_declarations_envelope() {
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "List files".into() }],
            }],
            system_prompt: None,
            tools: vec![
                ToolSpec {
                    name: "Bash".into(),
                    description: "Run a bash command".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"]
                    }),
                },
                ToolSpec {
                    name: "Read".into(),
                    description: "Read a file".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {"file_path": {"type": "string"}},
                        "required": ["file_path"]
                    }),
                },
            ],
            mode: Default::default(),
            ..Default::default()
        };
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        let tools = body["tools"].as_array().expect("tools must be array");
        assert_eq!(tools.len(), 1, "tools must be a single functionDeclarations envelope");
        let decls = tools[0]["functionDeclarations"]
            .as_array()
            .expect("functionDeclarations must be array");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0]["name"], "Bash");
        assert_eq!(decls[0]["description"], "Run a bash command");
        assert_eq!(decls[1]["name"], "Read");
    }

    #[test]
    fn build_request_without_optional_fields_omits_all_optional_keys() {
        let req = fixture_text_only_request();
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        assert!(body.get("systemInstruction").is_none(), "systemInstruction must be absent");
        assert!(body.get("tools").is_none(), "tools must be absent");
        assert!(body.get("generationConfig").is_none(), "generationConfig must be absent");
        assert!(body.get("safetySettings").is_none(), "safetySettings must be absent");
    }

    // -----------------------------------------------------------------------
    // Deferred-tool filtering in functionDeclarations
    // -----------------------------------------------------------------------

    fn five_tool_fixture() -> (Vec<ToolSpec>, Vec<ToolSpec>) {
        // Returns (3 always-load tools, 2 deferred tools)
        let always = vec![
            ToolSpec {
                name: "Bash".into(),
                description: "Run bash".into(),
                input_schema: json!({ "type": "object", "properties": { "command": { "type": "string" } } }),
            },
            ToolSpec {
                name: "Read".into(),
                description: "Read file".into(),
                input_schema: json!({ "type": "object", "properties": { "file_path": { "type": "string" } } }),
            },
            ToolSpec {
                name: "ToolSearch".into(),
                description: "Search tools".into(),
                input_schema: json!({ "type": "object", "properties": { "name": { "type": "string" } } }),
            },
        ];
        let deferred = vec![
            ToolSpec {
                name: "weather".into(),
                description: "Get weather".into(),
                input_schema: json!({ "type": "object", "properties": { "city": { "type": "string" } } }),
            },
            ToolSpec {
                name: "stocks".into(),
                description: "Get stock prices".into(),
                input_schema: json!({ "type": "object", "properties": { "ticker": { "type": "string" } } }),
            },
        ];
        (always, deferred)
    }

    fn user_message() -> Vec<Message> {
        vec![Message::User {
            content: vec![ContentBlock::Text { text: "Hello".into() }],
        }]
    }

    #[test]
    fn deferred_tools_omitted_from_function_declarations() {
        // 5 tools (3 AlwaysLoad, 2 Deferred), loaded_deferred_tools = {} → functionDeclarations has 3
        let (always, deferred) = five_tool_fixture();
        let all_tools: Vec<ToolSpec> = always.iter().chain(deferred.iter()).cloned().collect();
        let deferred_names: std::collections::HashSet<String> =
            deferred.iter().map(|t| t.name.clone()).collect();

        let req = CompletionRequest {
            messages: user_message(),
            tools: all_tools,
            deferred_tools: deferred_names,
            loaded_deferred_tools: Default::default(),
            ..Default::default()
        };
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        let decls = body["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("functionDeclarations must be present");
        assert_eq!(decls.len(), 3, "only 3 always-load tools should appear");
        let names: Vec<&str> = decls.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"weather"), "weather must be omitted");
        assert!(!names.contains(&"stocks"), "stocks must be omitted");
    }

    #[test]
    fn one_loaded_deferred_tool_appears_in_function_declarations() {
        // loaded_deferred_tools = {"weather"} → functionDeclarations has 4 (3 always + weather)
        let (always, deferred) = five_tool_fixture();
        let all_tools: Vec<ToolSpec> = always.iter().chain(deferred.iter()).cloned().collect();
        let deferred_names: std::collections::HashSet<String> =
            deferred.iter().map(|t| t.name.clone()).collect();

        let req = CompletionRequest {
            messages: user_message(),
            tools: all_tools,
            deferred_tools: deferred_names,
            loaded_deferred_tools: ["weather".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let body = build_request_body(&req, &SimpleGeminiNorm).expect("build should succeed");

        let decls = body["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("functionDeclarations must be present");
        assert_eq!(decls.len(), 4, "3 always-load + weather");
        let names: Vec<&str> = decls.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"weather"), "weather must be included after loading");
        assert!(!names.contains(&"stocks"), "stocks must still be omitted");
    }

    #[test]
    fn two_turn_round_trip_deferred_tool_appears_after_tool_search() {
        // Turn 1: weather deferred (absent from functionDeclarations)
        // Turn 2: ToolSearch resolved weather → it appears in functionDeclarations
        let (always, deferred) = five_tool_fixture();
        let all_tools: Vec<ToolSpec> = always.iter().chain(deferred.iter()).cloned().collect();
        let deferred_names: std::collections::HashSet<String> =
            deferred.iter().map(|t| t.name.clone()).collect();

        // Turn 1: no loaded deferred tools
        let turn1 = CompletionRequest {
            messages: user_message(),
            tools: all_tools.clone(),
            deferred_tools: deferred_names.clone(),
            loaded_deferred_tools: Default::default(),
            ..Default::default()
        };
        let body1 = build_request_body(&turn1, &SimpleGeminiNorm).expect("turn1 build should succeed");
        let decls1 = body1["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls1.len(), 3, "turn1: only 3 always-load tools");
        let names1: Vec<&str> = decls1.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(!names1.contains(&"weather"), "turn1: weather must be absent");

        // Runner processes ToolSearch(name="weather") → injects into loaded_deferred_tools
        // Turn 2: weather now in loaded_deferred_tools
        let turn2 = CompletionRequest {
            messages: user_message(),
            tools: all_tools,
            deferred_tools: deferred_names,
            loaded_deferred_tools: ["weather".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let body2 = build_request_body(&turn2, &SimpleGeminiNorm).expect("turn2 build should succeed");
        let decls2 = body2["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls2.len(), 4, "turn2: 3 always-load + weather");
        let names2: Vec<&str> = decls2.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names2.contains(&"weather"), "turn2: weather must be present after ToolSearch");
    }
}
