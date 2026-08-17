//! Request body builder — converts a [`CompletionRequest`] into the OpenAI
//! Chat Completions API JSON wire shape.
//!
//! ## OpenAI-specific dialect decisions
//!
//! - `stream_options.include_usage` is always set so the final SSE chunk
//!   carries token counts without a second round-trip.
//! - `parallel_tool_calls: true` is emitted **only when `tools` is non-empty**;
//!   sending this flag with an empty tools array is rejected by the OpenAI API.
//! - The system prompt is rendered as a leading `{"role":"system","content":...}`
//!   message inside the `messages` array — NOT a top-level `system` field
//!   (OpenAI's convention, unlike Anthropic's).

use ao_engine_tools_provider_config::OpenAIConfig;
use ao_engine_tools_runner::{
    message::{Message, MessageNormalizer},
    provider::{CompletionRequest, ProviderError},
};
use serde_json::{json, Value};

/// Build the OpenAI Chat Completions POST body from a [`CompletionRequest`].
///
/// The returned `Value` is ready to be serialised as the HTTP request body.
/// `model`, `stream`, `stream_options`, and (when non-empty) `tools` and
/// `parallel_tool_calls` are all set here. `max_completion_tokens` and
/// `reasoning_effort` are set only when `config` carries a resolved value —
/// unlike Anthropic's `max_tokens`, neither is required by this API, so
/// omitting them leaves the model's own default in effect rather than
/// forcing a hardcoded fallback onto every request.
///
/// System prompt is injected as the first element of the `messages` array
/// (`{"role":"system","content":<prompt>}`). If `request.system_prompt` is
/// `None`, no system message is emitted.
///
/// `Message::System` entries in `request.messages` are filtered out before
/// the normalizer is called — system content is carried via `system_prompt`,
/// not inline messages. The remaining history is then trimmed to
/// `config.max_context_tokens` (see [`ao_engine_tools_runner::message::truncate_to_context_budget`])
/// before normalization runs — same client-side budget enforcement as the
/// Anthropic request builder, since neither wire API exposes a
/// "cap total context tokens" parameter of its own.
pub fn build(
    config: &OpenAIConfig,
    request: &CompletionRequest,
    normalizer: &dyn MessageNormalizer,
) -> Result<Value, ProviderError> {
    // System messages are handled via system_prompt; filter any out of the array.
    let non_system: Vec<Message> = request
        .messages
        .iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .cloned()
        .collect();
    let non_system =
        ao_engine_tools_runner::message::truncate_to_context_budget(&non_system, config.max_context_tokens);

    // Translate canonical messages to OpenAI wire format.
    let mut messages_arr = normalizer
        .to_provider(&non_system)
        .map_err(|e| ProviderError::Transport(format!("message normalizer error: {e}")))?;

    // Prepend system message when system_prompt is Some.
    if let Some(system) = &request.system_prompt {
        let system_msg = json!({ "role": "system", "content": system });
        if let Some(arr) = messages_arr.as_array_mut() {
            arr.insert(0, system_msg);
        }
    }

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("messages".into(), messages_arr);

    if let Some(max_output_tokens) = config.max_output_tokens {
        body.insert("max_completion_tokens".into(), json!(max_output_tokens));
    }
    if let Some(effort) = config.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(effort.as_str()));
    }

    // Tools use OpenAI's function-calling envelope.
    // OpenAI has no native defer_loading equivalent — deferred tools that have
    // not yet been resolved by ToolSearch are omitted entirely (runtime-expansion
    // fallback). Once a tool appears in loaded_deferred_tools it is included.
    // parallel_tool_calls is gated on the visible (filtered) tools list being
    // non-empty — sending it with an empty array is an API error.
    let visible_tools: Vec<Value> = request
        .tools
        .iter()
        .filter(|t| {
            !request.deferred_tools.contains(&t.name)
                || request.loaded_deferred_tools.contains(&t.name)
        })
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();
    if !visible_tools.is_empty() {
        body.insert("tools".into(), Value::Array(visible_tools));
        body.insert("parallel_tool_calls".into(), json!(true));
    }

    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));

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

    // -------------------------------------------------------------------------
    // Minimal normalizer for request builder tests.
    //
    // Converts only the message shapes exercised in these tests. The full
    // production OpenAINormalizer lives in messages.rs and handles
    // all edge cases. This struct is test-local; callers must not construct it
    // outside this module.
    // -------------------------------------------------------------------------
    struct SimpleOpenAINorm;

    impl MessageNormalizer for SimpleOpenAINorm {
        fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
            let mut wire = Vec::new();
            for msg in messages {
                match msg {
                    Message::System { .. } => {
                        return Err(NormalizerError::Unrepresentable(
                            "System messages must be handled by request.rs".into(),
                        ));
                    }
                    Message::User { content } => {
                        let text: String = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        wire.push(json!({ "role": "user", "content": text }));
                    }
                    Message::Assistant { content } => {
                        let text: String = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        let tool_calls: Vec<Value> = content
                            .iter()
                            .filter_map(|b| {
                                if let ContentBlock::ToolUse { id, name, input } = b {
                                    Some(json!({
                                        "id": id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": serde_json::to_string(input)
                                                .unwrap_or_default(),
                                        }
                                    }))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if tool_calls.is_empty() {
                            wire.push(json!({ "role": "assistant", "content": text }));
                        } else {
                            let content_val =
                                if text.is_empty() { Value::Null } else { json!(text) };
                            wire.push(json!({
                                "role": "assistant",
                                "content": content_val,
                                "tool_calls": tool_calls
                            }));
                        }
                    }
                    Message::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let text: String = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        let output =
                            if *is_error { format!("Error: {text}") } else { text };
                        wire.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": output
                        }));
                    }
                }
            }
            Ok(Value::Array(wire))
        }

        fn from_provider(&self, _value: Value) -> Result<Vec<Message>, NormalizerError> {
            Err(NormalizerError::Unrepresentable(
                "from_provider not exercised in request builder tests".into(),
            ))
        }
    }

    fn fixture_config() -> OpenAIConfig {
        OpenAIConfig {
            api_key: "sk-openai-FIXTURE".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            organization: None,
            project: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
        }
    }

    fn fixture_request_3msg_2tool() -> CompletionRequest {
        CompletionRequest {
            messages: vec![
                Message::User {
                    content: vec![ContentBlock::Text {
                        text: "What files are in the current directory?".into(),
                    }],
                },
                Message::Assistant {
                    content: vec![
                        ContentBlock::Text {
                            text: "I'll check that for you.".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_01".into(),
                            name: "Bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                },
                Message::ToolResult {
                    tool_use_id: "call_01".into(),
                    content: vec![ContentBlock::Text {
                        text: "file1.rs\nfile2.rs".into(),
                    }],
                    is_error: false,
                },
            ],
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
        }
    }

    #[test]
    fn request_build_3msg_2tool_matches_fixture() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();

        let built = build(&config, &req, &normalizer).expect("build should succeed");

        let fixture_path = format!(
            "{}/tests/fixtures/request_3msg_2tool.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let fixture_str =
            std::fs::read_to_string(&fixture_path).expect("fixture file should exist");
        let expected: Value =
            serde_json::from_str(&fixture_str).expect("fixture should be valid JSON");

        assert_eq!(built, expected);
    }

    #[test]
    fn request_build_system_prompt_some_produces_leading_system_message() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "Hi".into() }],
            }],
            system_prompt: Some("You are helpful.".into()),
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let messages = built["messages"].as_array().expect("messages must be array");

        assert_eq!(messages[0]["role"], "system", "first message should be system");
        assert_eq!(messages[0]["content"], "You are helpful.");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn request_build_system_prompt_none_first_message_is_user() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "Hi".into() }],
            }],
            system_prompt: None,
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let messages = built["messages"].as_array().expect("messages must be array");

        assert_eq!(
            messages[0]["role"], "user",
            "first message should be user when system_prompt is None"
        );
    }

    #[test]
    fn request_build_tool_call_round_trip_preserves_ids() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![
                Message::Assistant {
                    content: vec![ContentBlock::ToolUse {
                        id: "call_abc".into(),
                        name: "Read".into(),
                        input: json!({"file_path": "/etc/hosts"}),
                    }],
                },
                Message::ToolResult {
                    tool_use_id: "call_abc".into(),
                    content: vec![ContentBlock::Text {
                        text: "127.0.0.1 localhost".into(),
                    }],
                    is_error: false,
                },
            ],
            system_prompt: None,
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let messages = built["messages"].as_array().unwrap();

        // Assistant message carries the tool_call id.
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_abc");
        // Tool result message echoes the same id as tool_call_id.
        assert_eq!(messages[1]["tool_call_id"], "call_abc");
    }

    #[test]
    fn request_build_empty_tools_elides_tools_and_parallel_tool_calls() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "Hi".into() }],
            }],
            system_prompt: None,
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");

        assert!(
            built.get("tools").is_none(),
            "tools key must be absent when tools vec is empty"
        );
        assert!(
            built.get("parallel_tool_calls").is_none(),
            "parallel_tool_calls must be absent when tools vec is empty"
        );
    }

    fn tool_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("Description of {name}"),
            input_schema: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        }
    }

    #[test]
    fn build_deferred_tools_omitted_from_wire_format() {
        // 5 tools (3 AlwaysLoad, 2 Deferred), loaded_deferred_tools = {} → tools[] has 3
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            system_prompt: None,
            tools: vec![
                tool_spec("Bash"),
                tool_spec("Read"),
                tool_spec("Write"),
                tool_spec("weather"),
                tool_spec("stocks"),
            ],
            mode: Default::default(),
            deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let tools = built["tools"].as_array().expect("tools must be array");
        assert_eq!(tools.len(), 3, "only 3 always-load tools should appear");

        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Bash"));
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
        assert!(!names.contains(&"weather"), "weather should be omitted");
        assert!(!names.contains(&"stocks"), "stocks should be omitted");
    }

    #[test]
    fn build_one_loaded_deferred_tool_appears_in_tools_array() {
        // loaded_deferred_tools = {"weather"} → tools[] has 4 (3 always + weather)
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            system_prompt: None,
            tools: vec![
                tool_spec("Bash"),
                tool_spec("Read"),
                tool_spec("Write"),
                tool_spec("weather"),
                tool_spec("stocks"),
            ],
            mode: Default::default(),
            deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            loaded_deferred_tools: ["weather".to_string()].into_iter().collect(),
            thinking: None,
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let tools = built["tools"].as_array().expect("tools must be array");
        assert_eq!(tools.len(), 4, "3 always-load + weather = 4");

        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"weather"), "weather should be present after loading");
        assert!(!names.contains(&"stocks"), "stocks still deferred — should be omitted");
    }

    #[test]
    fn build_all_loaded_deferred_tools_produces_full_tools_array() {
        // loaded_deferred_tools = {"weather", "stocks"} → tools[] has all 5
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            system_prompt: None,
            tools: vec![
                tool_spec("Bash"),
                tool_spec("Read"),
                tool_spec("Write"),
                tool_spec("weather"),
                tool_spec("stocks"),
            ],
            mode: Default::default(),
            deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            loaded_deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            thinking: None,
        };

        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let tools = built["tools"].as_array().expect("tools must be array");
        assert_eq!(tools.len(), 5, "all 5 tools present when both deferred are loaded");
    }

    #[test]
    fn build_two_turn_deferred_resolution_round_trip() {
        // Simulates the cross-turn scenario:
        // Turn 1: ToolSearch in tools[], weather deferred (omitted), stocks deferred (omitted).
        // Runner intercepts ToolSearch result and injects "weather" into loaded_deferred_tools.
        // Turn 2: weather is now visible in tools[]; stocks remains omitted.
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;

        let turn1 = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: "What is the weather?".into() }],
            }],
            system_prompt: None,
            tools: vec![
                tool_spec("ToolSearch"),
                tool_spec("weather"),
                tool_spec("stocks"),
            ],
            mode: Default::default(),
            deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let built1 = build(&config, &turn1, &normalizer).expect("turn1 build ok");
        let tools1 = built1["tools"].as_array().expect("turn1 tools");
        assert_eq!(tools1.len(), 1, "turn 1: only ToolSearch present");
        assert_eq!(tools1[0]["function"]["name"], "ToolSearch");

        // Runner processes ToolSearch(name="weather") and injects into loaded_deferred_tools.
        let turn2 = CompletionRequest {
            messages: turn1.messages.clone(),
            system_prompt: None,
            tools: vec![
                tool_spec("ToolSearch"),
                tool_spec("weather"),
                tool_spec("stocks"),
            ],
            mode: Default::default(),
            deferred_tools: ["weather".to_string(), "stocks".to_string()]
                .into_iter()
                .collect(),
            loaded_deferred_tools: ["weather".to_string()].into_iter().collect(),
            thinking: None,
        };

        let built2 = build(&config, &turn2, &normalizer).expect("turn2 build ok");
        let tools2 = built2["tools"].as_array().expect("turn2 tools");
        assert_eq!(tools2.len(), 2, "turn 2: ToolSearch + weather present");

        let names2: Vec<&str> = tools2
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names2.contains(&"ToolSearch"));
        assert!(names2.contains(&"weather"));
        assert!(!names2.contains(&"stocks"), "stocks still deferred");
    }

    // ─── tuning knobs: max_output_tokens / max_context_tokens / reasoning_effort ─

    #[test]
    fn build_omits_max_completion_tokens_when_config_unset() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        assert!(built.get("max_completion_tokens").is_none());
    }

    #[test]
    fn build_sets_max_completion_tokens_from_config() {
        let config = OpenAIConfig { max_output_tokens: Some(4096), ..fixture_config() };
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        assert_eq!(built["max_completion_tokens"], 4096);
    }

    #[test]
    fn build_omits_reasoning_effort_when_config_unset() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        assert!(built.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_sets_reasoning_effort_from_config() {
        let config = OpenAIConfig {
            reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::High),
            ..fixture_config()
        };
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        assert_eq!(built["reasoning_effort"], "high");
    }

    #[test]
    fn build_max_context_tokens_none_leaves_all_messages_intact() {
        let config = fixture_config();
        let normalizer = SimpleOpenAINorm;
        let req = fixture_request_3msg_2tool();
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let messages = built["messages"].as_array().expect("messages should be an array");
        assert_eq!(messages.len(), 3, "no cap configured — all 3 messages must reach the wire");
    }

    #[test]
    fn build_max_context_tokens_tight_budget_drops_oldest_messages() {
        let config = OpenAIConfig { max_context_tokens: Some(20), ..fixture_config() };
        let normalizer = SimpleOpenAINorm;
        let long_text = "x".repeat(400);
        let req = CompletionRequest {
            messages: vec![
                Message::User { content: vec![ContentBlock::Text { text: long_text.clone() }] },
                Message::Assistant { content: vec![ContentBlock::Text { text: long_text.clone() }] },
                Message::User { content: vec![ContentBlock::Text { text: "most recent turn".into() }] },
            ],
            system_prompt: None,
            tools: vec![],
            mode: Default::default(),
            ..Default::default()
        };
        let built = build(&config, &req, &normalizer).expect("build should succeed");
        let messages = built["messages"].as_array().expect("messages should be an array");
        assert!(
            messages.len() < 3,
            "a tight max_context_tokens budget should drop at least one older message, got {} messages",
            messages.len()
        );
        let last_content = messages.last().unwrap()["content"].as_str().unwrap_or_default();
        assert!(last_content.contains("most recent turn"));
    }
}
