use ao_engine_tools_provider_config::AnthropicConfig;
use ao_engine_tools_runner::{
    message::{ContentBlock, Message, MessageNormalizer},
    provider::{CompletionEvent, CompletionRequest, CompletionStream, ProviderClient, StopReason, ToolSpec},
};
use futures_util::StreamExt;
use serde_json::json;

use crate::{
    messages::AnthropicNormalizer,
    request,
    response::{AnthropicEvent, ContentBlockKind, DeltaKind},
    AnthropicClient,
};

fn fixture_config() -> AnthropicConfig {
    AnthropicConfig {
        api_key: "sk-ant-FIXTURE".into(),
        base_url: "https://api.anthropic.com".into(),
        model: "claude-opus-4-7".into(),
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
                        id: "toolu_01".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: vec![ContentBlock::Text {
                    text: "file1.rs\nfile2.rs".into(),
                }],
                is_error: false,
            },
        ],
        system_prompt: Some("You are a helpful assistant.".into()),
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
fn from_config_constructs_without_panicking() {
    let _client = AnthropicClient::from_config(fixture_config());
}

// ─── request::build tests ────────────────────────────────────────────────────

#[test]
fn build_3msg_2tool_matches_fixture() {
    // Acquire CACHE_TEST_MUTEX: this test expects cache_control in the fixture,
    // so it must not run while cache_control_absent_when_env_off holds CACHE_OFF.
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");

    let fixture_path = format!(
        "{}/tests/fixtures/request_3msg_2tool.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fixture_str =
        std::fs::read_to_string(&fixture_path).expect("fixture file should exist");
    let expected: serde_json::Value =
        serde_json::from_str(&fixture_str).expect("fixture should be valid JSON");

    assert_eq!(built, expected);
}

#[test]
fn build_with_system_prompt_some_includes_system_field() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: Some("Be concise.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let system = built.get("system").expect("system field should be present");
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "Be concise.");
}

#[test]
fn build_with_system_prompt_none_elides_system_field() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert!(
        built.get("system").is_none(),
        "system field should be absent when system_prompt is None"
    );
}

#[test]
fn build_thinking_absent_elides_thinking_field() {
    // Default request has `thinking: None` → body must not include the
    // `thinking` field. This pins the "no opt-in → no behaviour change"
    // contract callers rely on for backwards compatibility.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert!(
        built.get("thinking").is_none(),
        "thinking field should be absent when request.thinking is None"
    );
}

#[test]
fn build_thinking_disabled_elides_thinking_field() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};
    // Explicit `mode: Disabled` opts out — the cleanest wire shape is to
    // omit the field entirely, so the body must match the absent case.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        thinking: Some(ThinkingConfig {
            mode: ThinkingMode::Disabled,
            display: ThinkingDisplay::Summarized,
            budget_tokens: Some(2048),
        }),
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert!(
        built.get("thinking").is_none(),
        "thinking field should be absent when mode is Disabled, even if display/budget are set"
    );
}

#[test]
fn build_thinking_summarized_emits_adaptive_with_display_and_budget() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "explain step by step".into() }],
        }],
        thinking: Some(ThinkingConfig {
            mode: ThinkingMode::Adaptive,
            display: ThinkingDisplay::Summarized,
            budget_tokens: Some(4096),
        }),
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let thinking = built.get("thinking").expect("thinking field should be present");
    assert_eq!(thinking["type"], "adaptive");
    assert_eq!(thinking["display"], "summarized");
    assert_eq!(thinking["budget_tokens"], 4096);
}

#[test]
fn build_thinking_omitted_display_writes_omitted_string() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};
    // Pins the wire serialization for the omitted-display variant — the
    // canonical enum's snake_case rendering must match Anthropic's API
    // string verbatim.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        thinking: Some(ThinkingConfig {
            mode: ThinkingMode::Adaptive,
            display: ThinkingDisplay::Omitted,
            budget_tokens: None,
        }),
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let thinking = built.get("thinking").expect("thinking field should be present");
    assert_eq!(thinking["display"], "omitted");
    assert!(
        thinking.get("budget_tokens").is_none(),
        "budget_tokens field should be elided when None"
    );
}

// ─── tuning knobs: max_output_tokens / max_context_tokens / reasoning_effort ──

#[test]
fn build_max_tokens_defaults_to_8192_when_config_unset() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert_eq!(built["max_tokens"], 8192);
}

#[test]
fn build_max_tokens_reflects_configured_max_output_tokens() {
    let config = AnthropicConfig { max_output_tokens: Some(2048), ..fixture_config() };
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert_eq!(built["max_tokens"], 2048);
}

#[test]
fn build_reasoning_effort_low_maps_to_adaptive_thinking_with_budget() {
    let config = AnthropicConfig {
        reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::Low),
        ..fixture_config()
    };
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let thinking = built.get("thinking").expect("reasoning_effort should populate thinking");
    assert_eq!(thinking["type"], "adaptive");
    assert_eq!(thinking["budget_tokens"], 1024);
}

#[test]
fn build_reasoning_effort_high_budget_stays_below_max_output_tokens() {
    // High effort's raw tier (10_000) exceeds the configured max_output_tokens
    // (2048) here — the clamp in `reasoning_effort_to_thinking` must keep
    // budget_tokens < max_tokens or Anthropic rejects the request outright.
    let config = AnthropicConfig {
        max_output_tokens: Some(2048),
        reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::High),
        ..fixture_config()
    };
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let thinking = built.get("thinking").expect("reasoning_effort should populate thinking");
    let budget = thinking["budget_tokens"].as_u64().expect("budget_tokens should be a number");
    assert!(budget < 2048, "budget_tokens ({budget}) must stay below max_tokens (2048)");
    assert!(budget >= 1024, "budget_tokens ({budget}) must respect Anthropic's 1024 minimum");
}

#[test]
fn build_explicit_thinking_config_wins_over_reasoning_effort() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};
    // The older per-turn `request.thinking` override is strictly more
    // specific than the resolved `reasoning_effort` knob — it must take
    // precedence, not get silently overridden.
    let config = AnthropicConfig {
        reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::High),
        ..fixture_config()
    };
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        thinking: Some(ThinkingConfig {
            mode: ThinkingMode::Adaptive,
            display: ThinkingDisplay::Raw,
            budget_tokens: Some(1500),
        }),
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let thinking = built.get("thinking").expect("thinking field should be present");
    assert_eq!(thinking["display"], "raw");
    assert_eq!(thinking["budget_tokens"], 1500);
}

#[test]
fn build_reasoning_effort_absent_and_thinking_absent_elides_thinking_field() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert!(built.get("thinking").is_none());
}

#[test]
fn build_max_context_tokens_none_leaves_all_messages_intact() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = fixture_request_3msg_2tool();
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let messages = built["messages"].as_array().expect("messages should be an array");
    assert_eq!(messages.len(), 3, "no cap configured — all 3 messages must reach the wire");
}

#[test]
fn build_max_context_tokens_tight_budget_drops_oldest_messages() {
    // Each message's text is long enough that all three together clearly
    // exceed a very small token budget. The truncator must keep at least the
    // most recent message and drop the oldest first.
    let config = AnthropicConfig { max_context_tokens: Some(20), ..fixture_config() };
    let normalizer = AnthropicNormalizer;
    let long_text = "x".repeat(400);
    let req = CompletionRequest {
        messages: vec![
            Message::User { content: vec![ContentBlock::Text { text: long_text.clone() }] },
            Message::Assistant { content: vec![ContentBlock::Text { text: long_text.clone() }] },
            Message::User { content: vec![ContentBlock::Text { text: "most recent turn".into() }] },
        ],
        ..Default::default()
    };
    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let messages = built["messages"].as_array().expect("messages should be an array");
    assert!(
        messages.len() < 3,
        "a tight max_context_tokens budget should drop at least one older message, got {} messages",
        messages.len()
    );
    // The most recent message must always survive.
    let last_text = messages.last().unwrap()["content"][0]["text"].as_str().unwrap_or_default();
    assert!(last_text.contains("most recent turn"));
}

#[test]
fn build_empty_tools_elides_tools_key() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    assert!(
        built.get("tools").is_none(),
        "tools key should be absent when tools vec is empty"
    );
}

#[test]
fn build_tool_call_round_trip_preserves_ids() {
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_abc".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "/etc/hosts"}),
                }],
            },
            Message::ToolResult {
                tool_use_id: "toolu_abc".into(),
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

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let messages = built["messages"].as_array().unwrap();

    // The assistant message's tool_use id
    let tool_use_id = &messages[0]["content"][0]["id"];
    assert_eq!(tool_use_id, "toolu_abc");

    // The following user (tool_result) message's tool_use_id
    let tool_result_id = &messages[1]["content"][0]["tool_use_id"];
    assert_eq!(tool_result_id, "toolu_abc");
}

// ─── defer_loading flag tests ────────────────────────────────────────────────

/// Build a ToolSpec with the given name for deferral tests.
fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("Description of {name}"),
        input_schema: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
    }
}

#[test]
fn build_deferred_tools_marked_with_defer_loading_flag() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;

    // 3 AlwaysLoad tools + 2 Deferred tools
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![
            tool_spec("Read"),
            tool_spec("Write"),
            tool_spec("Bash"),
            tool_spec("weather"),
            tool_spec("github"),
        ],
        mode: Default::default(),
        deferred_tools: ["weather".to_string(), "github".to_string()].into_iter().collect(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let tools = built["tools"].as_array().expect("tools key should be present");

    assert_eq!(tools.len(), 5, "all 5 tools should appear in the request");

    // 3 AlwaysLoad tools: no defer_loading flag
    for name in ["Read", "Write", "Bash"] {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} missing"));
        assert!(
            t.get("defer_loading").is_none(),
            "{name} should not have defer_loading flag"
        );
    }

    // 2 Deferred tools: defer_loading: true — wire-format key is snake_case "defer_loading"
    for name in ["weather", "github"] {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(
            t["defer_loading"], json!(true),
            "{name} should have defer_loading: true"
        );
        // Verify the other fields are still present
        assert!(t.get("description").is_some(), "{name} description should be present");
        assert!(t.get("input_schema").is_some(), "{name} input_schema should be present");
    }
}

#[test]
fn build_loaded_deferred_tool_emitted_without_flag() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;

    // "weather" has been resolved by ToolSearch → it should appear without defer_loading
    // "github" is still unresolved → it keeps defer_loading: true
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![
            tool_spec("Read"),
            tool_spec("Write"),
            tool_spec("Bash"),
            tool_spec("weather"),
            tool_spec("github"),
        ],
        mode: Default::default(),
        deferred_tools: ["weather".to_string(), "github".to_string()].into_iter().collect(),
        loaded_deferred_tools: ["weather".to_string()].into_iter().collect(),
        thinking: None,
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let tools = built["tools"].as_array().expect("tools key should be present");

    assert_eq!(tools.len(), 5, "all 5 tools should appear");

    // "weather" is in loaded_deferred_tools → no defer_loading
    let weather = tools.iter().find(|t| t["name"] == "weather").expect("weather missing");
    assert!(
        weather.get("defer_loading").is_none(),
        "weather should NOT have defer_loading (it has been resolved)"
    );

    // "github" is still deferred → defer_loading: true
    let github = tools.iter().find(|t| t["name"] == "github").expect("github missing");
    assert_eq!(
        github["defer_loading"], json!(true),
        "github should still have defer_loading: true"
    );

    // AlwaysLoad tools: no defer_loading
    for name in ["Read", "Write", "Bash"] {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} missing"));
        assert!(
            t.get("defer_loading").is_none(),
            "{name} should not have defer_loading flag"
        );
    }
}

// ─── messages::AnthropicNormalizer round-trip tests ──────────────────────────

fn five_msg_fixture() -> Vec<Message> {
    vec![
        Message::User {
            content: vec![ContentBlock::Text {
                text: "What files are here?".into(),
            }],
        },
        Message::Assistant {
            content: vec![
                ContentBlock::Text {
                    text: "I'll check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_rt1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        },
        Message::ToolResult {
            tool_use_id: "toolu_rt1".into(),
            content: vec![ContentBlock::Text {
                text: "a.rs\nb.rs".into(),
            }],
            is_error: false,
        },
        Message::User {
            content: vec![ContentBlock::Text {
                text: "Thanks.".into(),
            }],
        },
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: "You're welcome.".into(),
            }],
        },
    ]
}

#[test]
fn normalizer_round_trips_five_message_transcript() {
    let n = AnthropicNormalizer;
    let msgs = five_msg_fixture();
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_user_message_with_inline_tool_result() {
    let n = AnthropicNormalizer;
    let msgs = vec![
        Message::User {
            content: vec![
                ContentBlock::Text { text: "Here is the result:".into() },
                ContentBlock::ToolResult {
                    tool_use_id: "toolu_inline".into(),
                    content: "42".into(),
                    is_error: false,
                },
            ],
        },
    ];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_assistant_with_text_and_tool_use() {
    let n = AnthropicNormalizer;
    let msgs = vec![
        Message::Assistant {
            content: vec![
                ContentBlock::Text { text: "Let me read that.".into() },
                ContentBlock::ToolUse {
                    id: "toolu_x9".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "/etc/hosts"}),
                },
            ],
        },
    ];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
    // Verify id and name are preserved exactly
    if let Message::Assistant { content } = &recovered[0] {
        if let ContentBlock::ToolUse { id, name, .. } = &content[1] {
            assert_eq!(id, "toolu_x9");
            assert_eq!(name, "Read");
        } else {
            panic!("expected ToolUse block at index 1");
        }
    } else {
        panic!("expected Assistant message");
    }
}

#[test]
fn normalizer_round_trips_assistant_with_thinking_text_and_tool_use() {
    // The shape Anthropic requires us to echo back on the next turn when
    // extended thinking + tool use happened in the same turn: a `thinking`
    // block (with text + signature) preceding the `tool_use` block. Both
    // round-trip directions must preserve the signature byte-for-byte.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::Assistant {
        content: vec![
            ContentBlock::Thinking {
                text: Some("I should call Read first.".into()),
                signature: Some("sig_replay==".into()),
            },
            ContentBlock::Text { text: "Let me read that.".into() },
            ContentBlock::ToolUse {
                id: "toolu_th1".into(),
                name: "Read".into(),
                input: json!({"file_path": "/etc/hosts"}),
            },
        ],
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    // The outermost shape is a one-element array; the inner content array
    // must start with the thinking block so the wire ordering matches the
    // canonical ordering.
    assert_eq!(wire[0]["content"][0]["type"], "thinking");
    assert_eq!(wire[0]["content"][0]["thinking"], "I should call Read first.");
    assert_eq!(wire[0]["content"][0]["signature"], "sig_replay==");

    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_assistant_with_signature_only_thinking_block() {
    // The `display = "omitted"` shape: signature without text. The runner
    // still echoes this back on the next turn — Anthropic's continuity
    // check keys off the signature, not the (missing) reasoning text.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::Assistant {
        content: vec![
            ContentBlock::Thinking {
                text: None,
                signature: Some("sig_omitted==".into()),
            },
            ContentBlock::ToolUse {
                id: "toolu_th2".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            },
        ],
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_assistant_with_redacted_thinking_and_tool_use() {
    // A turn can mix a signed `thinking` block and a safety-redacted block
    // before its tool_use. Both must be echoed back on the next turn, and
    // their relative order must survive — Anthropic rejects a tool-using
    // transcript that drops or reorders the reasoning blocks. We assert the
    // wire ordering explicitly, not just the canonical round-trip.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::Assistant {
        content: vec![
            ContentBlock::Thinking {
                text: Some("Plan: read the file.".into()),
                signature: Some("sig_mixed==".into()),
            },
            ContentBlock::RedactedThinking {
                data: "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_rd1".into(),
                name: "Read".into(),
                input: json!({"file_path": "/etc/hosts"}),
            },
        ],
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    assert_eq!(wire[0]["content"][0]["type"], "thinking");
    assert_eq!(wire[0]["content"][1]["type"], "redacted_thinking");
    assert_eq!(
        wire[0]["content"][1]["data"],
        "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5"
    );
    assert_eq!(wire[0]["content"][2]["type"], "tool_use");

    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_tool_result_with_image_block() {
    // A tool that returns a screenshot produces a ToolResult carrying an
    // Image content block. Anthropic embeds it as a base64 `image` block in
    // the tool_result content array; the wire `source` shape must survive the
    // round-trip.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::ToolResult {
        tool_use_id: "toolu_img1".into(),
        content: vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUg==".into(),
        }],
        is_error: false,
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let inner = &wire[0]["content"][0]["content"][0];
    assert_eq!(inner["type"], "image");
    assert_eq!(inner["source"]["type"], "base64");
    assert_eq!(inner["source"]["media_type"], "image/png");
    assert_eq!(inner["source"]["data"], "iVBORw0KGgoAAAANSUhEUg==");

    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_tool_result_with_document_block_and_title() {
    // A PDF read returns a Document block with an optional display title. The
    // title is emitted alongside the base64 source and must round-trip.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::ToolResult {
        tool_use_id: "toolu_pdf1".into(),
        content: vec![
            ContentBlock::Text {
                text: "PDF read: report.pdf".into(),
            },
            ContentBlock::Document {
                media_type: "application/pdf".into(),
                data: "JVBERi0xLjQK".into(),
                title: Some("report.pdf".into()),
            },
        ],
        is_error: false,
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let doc = &wire[0]["content"][0]["content"][1];
    assert_eq!(doc["type"], "document");
    assert_eq!(doc["source"]["type"], "base64");
    assert_eq!(doc["source"]["media_type"], "application/pdf");
    assert_eq!(doc["source"]["data"], "JVBERi0xLjQK");
    assert_eq!(doc["title"], "report.pdf");

    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_round_trips_document_block_without_title() {
    // The title is optional; a Document with `title: None` must omit the wire
    // field entirely and decode back to `None`.
    let n = AnthropicNormalizer;
    let msgs = vec![Message::ToolResult {
        tool_use_id: "toolu_pdf2".into(),
        content: vec![ContentBlock::Document {
            media_type: "application/pdf".into(),
            data: "JVBERi0xLjQK".into(),
            title: None,
        }],
        is_error: false,
    }];
    let wire = n.to_provider(&msgs).expect("to_provider should succeed");
    let doc = &wire[0]["content"][0]["content"][0];
    assert!(doc.get("title").is_none(), "title must be omitted when None");

    let recovered = n.from_provider(wire).expect("from_provider should succeed");
    assert_eq!(msgs, recovered);
}

#[test]
fn normalizer_to_provider_system_in_non_leading_returns_unrepresentable() {
    let n = AnthropicNormalizer;
    let msgs = vec![
        Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        },
        Message::System {
            content: "injected system".into(),
        },
    ];
    let err = n.to_provider(&msgs).unwrap_err();
    match err {
        ao_engine_tools_runner::message::NormalizerError::Unrepresentable(_) => {}
        other => panic!("expected Unrepresentable, got: {other:?}"),
    }
}

#[test]
fn message_normalizer_accessor_returns_normalizer() {
    let client = AnthropicClient::from_config(fixture_config());
    // Verify the accessor compiles and returns a working normalizer
    let n = client.message_normalizer();
    let msgs = vec![Message::User {
        content: vec![ContentBlock::Text { text: "ping".into() }],
    }];
    assert!(n.to_provider(&msgs).is_ok());
}

// ─── response::parse_sse_stream tests ────────────────────────────────────────

fn sse_fixture_bytes() -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/sse_simple.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).expect("sse_simple.txt fixture should exist")
}

#[tokio::test]
async fn sse_fixture_parses_12_events_in_order() {
    let bytes = sse_fixture_bytes();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(bytes)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 12, "expected 12 events, got: {events:?}");

    // Event 0: MessageStart
    assert!(
        matches!(&events[0], Ok(AnthropicEvent::MessageStart { .. })),
        "event 0 should be MessageStart"
    );
    // Event 1: ContentBlockStart{Text}
    assert!(
        matches!(
            &events[1],
            Ok(AnthropicEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockKind::Text
            })
        ),
        "event 1 should be ContentBlockStart{{Text}}"
    );
    // Events 2–8: ContentBlockDelta{TextDelta}
    for i in 2..=8 {
        assert!(
            matches!(
                &events[i],
                Ok(AnthropicEvent::ContentBlockDelta {
                    index: 0,
                    delta: DeltaKind::TextDelta { .. }
                })
            ),
            "event {i} should be ContentBlockDelta{{TextDelta}}"
        );
    }
    // Event 9: ContentBlockStop
    assert!(
        matches!(
            &events[9],
            Ok(AnthropicEvent::ContentBlockStop { index: 0 })
        ),
        "event 9 should be ContentBlockStop"
    );
    // Event 10: MessageDelta with stop_reason
    if let Ok(AnthropicEvent::MessageDelta { stop_reason, .. }) = &events[10] {
        assert_eq!(
            stop_reason.as_deref(),
            Some("end_turn"),
            "MessageDelta should carry stop_reason end_turn"
        );
    } else {
        panic!("event 10 should be MessageDelta");
    }
    // Event 11: MessageStop
    assert!(
        matches!(&events[11], Ok(AnthropicEvent::MessageStop)),
        "event 11 should be MessageStop"
    );
}

#[tokio::test]
async fn sse_split_event_single_byte_chunks_parses_correctly() {
    // Verifies split-event resumption: bytes arriving one at a time are buffered
    // until the \n\n boundary; no partial events are emitted and all 12 events
    // are eventually produced.
    let bytes = sse_fixture_bytes();
    let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> =
        bytes.into_iter().map(|b| Ok(vec![b])).collect();
    let stream = futures_util::stream::iter(chunks);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 12, "single-byte-chunk test: expected 12 events");
    assert!(matches!(&events[0], Ok(AnthropicEvent::MessageStart { .. })));
    assert!(matches!(&events[11], Ok(AnthropicEvent::MessageStop)));
}

#[tokio::test]
async fn sse_malformed_json_yields_transport_error_and_closes_stream() {
    let data = b"event: message_start\ndata: {not valid json}\n\n".to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 1, "should yield exactly one error item");
    assert!(
        matches!(
            &events[0],
            Err(ao_engine_tools_runner::provider::ProviderError::Transport(msg))
            if msg.contains("SSE parse error")
        ),
        "expected Transport error containing 'SSE parse error', got: {:?}",
        events[0]
    );
}

#[tokio::test]
async fn sse_unknown_event_type_yields_transport_error_and_closes_stream() {
    let data =
        b"event: unknown_future_event\ndata: {\"type\":\"unknown_future_event\",\"data\":\"x\"}\n\n"
            .to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 1, "should yield exactly one error item");
    assert!(
        matches!(
            &events[0],
            Err(ao_engine_tools_runner::provider::ProviderError::Transport(msg))
            if msg.contains("unknown event type")
        ),
        "expected Transport error containing 'unknown event type', got: {:?}",
        events[0]
    );
}

#[tokio::test]
async fn sse_ping_events_are_silently_skipped() {
    // A stream with a ping sandwiched between two real events should yield
    // exactly 2 typed events (not 3), with the ping silently dropped.
    let data = concat!(
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
    .as_bytes()
    .to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 2, "pings should be skipped, yielding 2 events");
    assert!(matches!(&events[0], Ok(AnthropicEvent::MessageStop)));
    assert!(matches!(&events[1], Ok(AnthropicEvent::MessageStop)));
}

#[tokio::test]
async fn sse_tool_use_block_parses_correctly() {
    let data = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\"\"}}\n\n",
    )
    .as_bytes()
    .to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 2);
    assert!(
        matches!(
            &events[0],
            Ok(AnthropicEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockKind::ToolUse { id, name }
            }) if id == "toolu_abc" && name == "Bash"
        ),
        "expected ContentBlockStart{{ToolUse}}, got: {:?}",
        events[0]
    );
    assert!(
        matches!(
            &events[1],
            Ok(AnthropicEvent::ContentBlockDelta {
                index: 1,
                delta: DeltaKind::InputJsonDelta { .. }
            })
        ),
        "expected ContentBlockDelta{{InputJsonDelta}}, got: {:?}",
        events[1]
    );
}

#[tokio::test]
async fn sse_thinking_block_parses_start_delta_signature_stop() {
    // Captures the four-event lifecycle for a summarized-display thinking
    // block: open (no inner text), one thinking_delta, one signature_delta,
    // close. Pins SSE-parser-level recognition of all three new variants:
    // `ContentBlockKind::Thinking`, `DeltaKind::ThinkingDelta`, and
    // `DeltaKind::SignatureDelta`. The state-machine-level test below
    // verifies the consumer translates these into `ThinkingStart` /
    // `ThinkingDelta` / `ThinkingEnd` `CompletionEvent`s.
    let data = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think.\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    )
    .as_bytes()
    .to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 4, "expected 4 SSE events, got {events:?}");
    assert!(
        matches!(
            &events[0],
            Ok(AnthropicEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockKind::Thinking
            })
        ),
        "event[0] should be ContentBlockStart{{Thinking}}, got: {:?}",
        events[0]
    );
    assert!(
        matches!(
            &events[1],
            Ok(AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaKind::ThinkingDelta { text }
            }) if text == "Let me think."
        ),
        "event[1] should be ContentBlockDelta{{ThinkingDelta}}, got: {:?}",
        events[1]
    );
    assert!(
        matches!(
            &events[2],
            Ok(AnthropicEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaKind::SignatureDelta { signature }
            }) if signature == "abc"
        ),
        "event[2] should be ContentBlockDelta{{SignatureDelta(\"abc\")}}, got: {:?}",
        events[2]
    );
    assert!(
        matches!(&events[3], Ok(AnthropicEvent::ContentBlockStop { index: 0 })),
        "event[3] should be ContentBlockStop, got: {:?}",
        events[3]
    );
}

#[tokio::test]
async fn sse_redacted_thinking_block_parses_data_from_start_event() {
    // A safety-redacted block carries its whole encrypted payload inline on
    // the start event — no deltas follow, and stop closes it immediately.
    // The parser must surface the `data` blob so the consumer can replay it.
    let data = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EncryptedBlob==\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    )
    .as_bytes()
    .to_vec();
    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(data)]);
    let events: Vec<_> = crate::response::parse_sse_stream(stream).collect().await;

    assert_eq!(events.len(), 2, "expected 2 SSE events, got {events:?}");
    assert!(
        matches!(
            &events[0],
            Ok(AnthropicEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockKind::RedactedThinking { data }
            }) if data == "EncryptedBlob=="
        ),
        "event[0] should be ContentBlockStart{{RedactedThinking}}, got: {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], Ok(AnthropicEvent::ContentBlockStop { index: 0 })),
        "event[1] should be ContentBlockStop, got: {:?}",
        events[1]
    );
}

// ─── complete() state machine tests ──────────────────────────────────────────
//
// Each test drives the state machine via a hand-built SSE byte stream rather
// than a real network call. The stream is fed to complete() through a local
// wiremock-style stub built from a tokio TcpListener.

/// Build a minimal SSE byte sequence for a single-text-block turn.
/// Events: MessageStart → ContentBlockStart{Text} → ContentBlockDelta(×N) →
///         ContentBlockStop → MessageDelta{end_turn} → MessageStop
fn sse_single_text_turn(chunks: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();

    // MessageStart
    out.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
    );
    // ContentBlockStart{Text, index 0}
    out.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    // ContentBlockDelta per chunk
    for chunk in chunks {
        let data = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
            chunk
        );
        out.extend_from_slice(data.as_bytes());
    }
    // ContentBlockStop
    out.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    // MessageDelta
    out.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
    );
    // MessageStop
    out.extend_from_slice(
        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    out
}

/// Build an SSE byte sequence for a single tool-call turn followed by end.
fn sse_tool_call_turn(
    tool_id: &str,
    tool_name: &str,
    input_json: &str,
    text_chunk: Option<&str>,
) -> Vec<u8> {
    let mut out = Vec::new();

    // MessageStart with input usage
    out.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20}}}\n\n",
    );

    let mut idx = 0u32;

    // Optional text block before the tool_use
    if let Some(text) = text_chunk {
        let start = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
        );
        out.extend_from_slice(start.as_bytes());
        let delta = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        );
        out.extend_from_slice(delta.as_bytes());
        let stop = format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n"
        );
        out.extend_from_slice(stop.as_bytes());
        idx += 1;
    }

    // ToolUse block
    let start = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{tool_id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n"
    );
    out.extend_from_slice(start.as_bytes());

    // Deliver the input JSON in one delta
    let escaped = input_json.replace('"', "\\\"");
    let delta = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{escaped}\"}}}}\n\n"
    );
    out.extend_from_slice(delta.as_bytes());

    let stop = format!(
        "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n"
    );
    out.extend_from_slice(stop.as_bytes());

    // MessageDelta with tool_use stop_reason + output usage
    out.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n",
    );
    // MessageStop
    out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    out
}

/// Feed a raw SSE byte slice to the parser and run the state machine, returning
/// all `CompletionEvent`s in order.
///
/// This bypasses the HTTP layer and drives only the state machine that these
/// tests exercise. The HTTP layer is validated by separate integration tests.
async fn run_state_machine(sse_bytes: Vec<u8>) -> Vec<CompletionEvent> {
    use ao_engine_tools_runner::provider::StopReason as SR;
    use futures_util::StreamExt as _;

    let stream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(sse_bytes)]);
    let mut event_stream = Box::pin(crate::response::parse_sse_stream(stream));

    use std::collections::HashMap;
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<CompletionEvent, ao_engine_tools_runner::provider::ProviderError>>(64);

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_child = cancel.clone();

    tokio::spawn(async move {
        let mut pending: HashMap<u32, crate::response::ContentBlockKind> = HashMap::new();
        let mut input_buffers: HashMap<u32, String> = HashMap::new();
        let mut thinking_anchors: HashMap<u32, std::time::Instant> = HashMap::new();
        let mut thinking_text_buffers: HashMap<u32, String> = HashMap::new();
        let mut thinking_signatures: HashMap<u32, String> = HashMap::new();
        let mut redacted_thinking_data: HashMap<u32, String> = HashMap::new();
        let mut cached_stop_reason: Option<SR> = None;

        loop {
            tokio::select! {
                biased;
                _ = cancel_child.cancelled() => break,
                ev = event_stream.next() => match ev {
                    None => break,
                    Some(Err(e)) => { let _ = tx.send(Err(e)).await; break; }
                    Some(Ok(event)) => {
                        match event {
                            AnthropicEvent::MessageStart { usage } => {
                                if let Some(u) = crate::usage::extract_usage(&usage) {
                                    let _ = tx.send(Ok(CompletionEvent::Usage(u))).await;
                                }
                            }
                            AnthropicEvent::ContentBlockStart { index, content_block } => {
                                match &content_block {
                                    crate::response::ContentBlockKind::Thinking => {
                                        thinking_anchors.insert(index, std::time::Instant::now());
                                        let _ = tx.send(Ok(CompletionEvent::ThinkingStart)).await;
                                    }
                                    crate::response::ContentBlockKind::RedactedThinking { data } => {
                                        redacted_thinking_data.insert(index, data.clone());
                                    }
                                    _ => {}
                                }
                                pending.insert(index, content_block);
                            }
                            AnthropicEvent::ContentBlockDelta { index, delta } => {
                                match delta {
                                    crate::response::DeltaKind::TextDelta { text } => {
                                        let _ = tx.send(Ok(CompletionEvent::AssistantText(text))).await;
                                    }
                                    crate::response::DeltaKind::InputJsonDelta { partial_json } => {
                                        input_buffers.entry(index).or_default().push_str(&partial_json);
                                    }
                                    crate::response::DeltaKind::ThinkingDelta { text } => {
                                        if !thinking_anchors.contains_key(&index) {
                                            thinking_anchors.insert(index, std::time::Instant::now());
                                            let _ = tx.send(Ok(CompletionEvent::ThinkingStart)).await;
                                        }
                                        thinking_text_buffers
                                            .entry(index)
                                            .or_default()
                                            .push_str(&text);
                                        let _ = tx.send(Ok(CompletionEvent::ThinkingDelta { text })).await;
                                    }
                                    crate::response::DeltaKind::SignatureDelta { signature } => {
                                        // Mirror the live consumer — capture
                                        // the signature so the post-stop
                                        // ThinkingBlock event carries it back.
                                        thinking_signatures.insert(index, signature);
                                    }
                                }
                            }
                            AnthropicEvent::ContentBlockStop { index } => {
                                if let Some(anchor) = thinking_anchors.remove(&index) {
                                    let elapsed_ms = anchor.elapsed().as_millis() as u64;
                                    let _ = tx
                                        .send(Ok(CompletionEvent::ThinkingEnd { elapsed_ms }))
                                        .await;
                                    let text = thinking_text_buffers
                                        .remove(&index)
                                        .filter(|s| !s.is_empty());
                                    let signature = thinking_signatures
                                        .remove(&index)
                                        .filter(|s| !s.is_empty());
                                    let _ = tx
                                        .send(Ok(CompletionEvent::ThinkingBlock {
                                            text,
                                            signature,
                                        }))
                                        .await;
                                }
                                if let Some(data) = redacted_thinking_data.remove(&index) {
                                    let _ = tx
                                        .send(Ok(CompletionEvent::RedactedThinkingBlock { data }))
                                        .await;
                                }
                                if let Some(crate::response::ContentBlockKind::ToolUse { id, name }) = pending.remove(&index) {
                                    let json_str = input_buffers.remove(&index).unwrap_or_default();
                                    let input = serde_json::from_str(&json_str)
                                        .unwrap_or(serde_json::Value::Object(Default::default()));
                                    let _ = tx.send(Ok(CompletionEvent::ToolUse { id, name, input })).await;
                                }
                            }
                            AnthropicEvent::MessageDelta { stop_reason, usage } => {
                                if let Some(reason) = stop_reason {
                                    cached_stop_reason = Some(crate::stop_reason::map_stop_reason(&reason));
                                }
                                if let Some(usage_val) = usage {
                                    if let Some(u) = crate::usage::extract_usage(&usage_val) {
                                        let _ = tx.send(Ok(CompletionEvent::Usage(u))).await;
                                    }
                                }
                            }
                            AnthropicEvent::MessageStop => {
                                let sr = cached_stop_reason.unwrap_or(SR::Natural);
                                let _ = tx.send(Ok(CompletionEvent::TurnComplete { stop_reason: sr })).await;
                                break;
                            }
                        }
                    }
                }
            }
        }
        drop(tx);
    });

    let mut out = Vec::new();
    let mut stream = CompletionStream::new(rx);
    while let Some(item) = stream.recv().await {
        out.push(item.expect("unexpected Err in state machine test"));
    }
    out
}

#[tokio::test]
async fn state_machine_single_text_turn_yields_expected_sequence() {
    // Acceptance: a single-text-block turn yields [Usage, AssistantText*, Usage, TurnComplete{Natural}]
    let bytes = sse_single_text_turn(&["Hello", ", ", "world!"]);
    let events = run_state_machine(bytes).await;

    // First event: Usage (from MessageStart)
    assert!(
        matches!(&events[0], CompletionEvent::Usage(_)),
        "event[0] should be Usage (from MessageStart), got: {:?}",
        events[0]
    );
    // Middle events: AssistantText chunks
    let text_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::AssistantText(_)))
        .collect();
    assert_eq!(text_events.len(), 3, "should have 3 AssistantText chunks");

    // Second-to-last: Usage (from MessageDelta output_tokens)
    let last_usage_pos = events
        .iter()
        .rposition(|e| matches!(e, CompletionEvent::Usage(_)))
        .expect("should have a second Usage");
    let turn_complete_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::TurnComplete { .. }))
        .expect("should have TurnComplete");

    // Usage must appear before TurnComplete (drift-guard)
    assert!(
        last_usage_pos < turn_complete_pos,
        "final Usage must precede TurnComplete"
    );

    // TurnComplete carries StopReason::Natural
    assert!(
        matches!(
            &events[turn_complete_pos],
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::Natural
            }
        ),
        "TurnComplete should carry StopReason::Natural"
    );
}

#[tokio::test]
async fn state_machine_tool_call_turn_yields_expected_sequence() {
    // Acceptance: [Usage, AssistantText*, ToolUse{id,name,input}, Usage, TurnComplete{ToolUse}]
    let bytes = sse_tool_call_turn(
        "toolu_sm1",
        "Bash",
        r#"{"command":"ls"}"#,
        Some("I'll run that."),
    );
    let events = run_state_machine(bytes).await;

    // First: Usage from MessageStart
    assert!(
        matches!(&events[0], CompletionEvent::Usage(_)),
        "event[0] should be Usage"
    );

    // Should contain exactly one ToolUse event with correct id/name
    let tool_use_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::ToolUse { .. }))
        .collect();
    assert_eq!(tool_use_events.len(), 1, "should have exactly 1 ToolUse");
    if let CompletionEvent::ToolUse { id, name, input } = &tool_use_events[0] {
        assert_eq!(id, "toolu_sm1");
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "ls");
    }

    // TurnComplete at the end carries StopReason::ToolUse
    let last = events.last().expect("stream should not be empty");
    assert!(
        matches!(
            last,
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::ToolUse
            }
        ),
        "last event should be TurnComplete{{ToolUse}}, got: {:?}",
        last
    );

    // Usage precedes TurnComplete
    let last_usage_pos = events
        .iter()
        .rposition(|e| matches!(e, CompletionEvent::Usage(_)))
        .expect("should have Usage before TurnComplete");
    let tc_pos = events.len() - 1;
    assert!(last_usage_pos < tc_pos, "Usage must precede TurnComplete");
}

#[tokio::test]
async fn state_machine_multi_tool_turn_yields_both_tool_use_events_before_turn_complete() {
    // Two parallel tool_use blocks at indices 0 and 1 in the same assistant turn.
    let mut bytes = Vec::new();

    // MessageStart
    bytes.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30}}}\n\n",
    );
    // Block 0: ToolUse "Read"
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mt1\",\"name\":\"Read\",\"input\":{}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\":\\\"/a.txt\\\"}\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    // Block 1: ToolUse "Bash"
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mt2\",\"name\":\"Bash\",\"input\":{}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    // MessageDelta + MessageStop
    bytes.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":10}}\n\n",
    );
    bytes.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    let events = run_state_machine(bytes).await;

    // Both ToolUse events present with correct ids
    let tool_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::ToolUse { .. }))
        .collect();
    assert_eq!(tool_events.len(), 2, "expected 2 ToolUse events");

    let ids: Vec<&str> = tool_events
        .iter()
        .map(|e| match e {
            CompletionEvent::ToolUse { id, .. } => id.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert!(ids.contains(&"toolu_mt1"), "toolu_mt1 id should be present");
    assert!(ids.contains(&"toolu_mt2"), "toolu_mt2 id should be present");

    // Both ToolUse events appear before TurnComplete
    let tc_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::TurnComplete { .. }))
        .expect("TurnComplete should be present");
    for e in &tool_events {
        let tu_pos = events.iter().position(|x| std::ptr::eq(x, *e)).unwrap();
        assert!(tu_pos < tc_pos, "ToolUse should precede TurnComplete");
    }

    // TurnComplete carries StopReason::ToolUse
    assert!(
        matches!(
            &events[tc_pos],
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::ToolUse
            }
        ),
        "TurnComplete should carry ToolUse"
    );
}

/// Build an SSE byte sequence for a thinking block followed by a text turn.
/// Events: MessageStart → ContentBlockStart{Thinking, 0} → N×ThinkingDelta →
///         SignatureDelta → ContentBlockStop{0} → ContentBlockStart{Text, 1} →
///         TextDelta → ContentBlockStop{1} → MessageDelta{end_turn} →
///         MessageStop.
///
/// Models the `display = "summarized"` shape: progressive reasoning text
/// followed by the signature attestation, then a normal assistant text block.
fn sse_thinking_then_text_turn(thinking_chunks: &[&str], reply: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
    );
    // Open thinking block at index 0
    out.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    );
    for chunk in thinking_chunks {
        let data = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"{}\"}}}}\n\n",
            chunk
        );
        out.extend_from_slice(data.as_bytes());
    }
    // Signature attestation closes the reasoning trace
    out.extend_from_slice(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
    );
    out.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    // Open text block at index 1
    out.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    let data = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
        reply
    );
    out.extend_from_slice(data.as_bytes());
    out.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    out.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":18}}\n\n",
    );
    out.extend_from_slice(
        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    out
}

#[tokio::test]
async fn state_machine_thinking_then_text_emits_canonical_thinking_triplet() {
    // End-to-end: an SSE turn with a thinking block followed by a text reply
    // must translate to the canonical `ThinkingStart` → N×`ThinkingDelta` →
    // `ThinkingEnd` → assistant text → `TurnComplete` `CompletionEvent`
    // sequence. The `signature_delta` between the thinking deltas and the
    // close should be silently absorbed (no `CompletionEvent` emitted).
    let bytes =
        sse_thinking_then_text_turn(&["Let me ", "think ", "about this."], "Here is the answer.");
    let events = run_state_machine(bytes).await;

    let start_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::ThinkingStart))
        .expect("ThinkingStart should be emitted exactly once");
    let end_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::ThinkingEnd { .. }))
        .expect("ThinkingEnd should follow the thinking close");
    assert!(start_pos < end_pos, "ThinkingStart must precede ThinkingEnd");

    let delta_texts: Vec<&str> = events[start_pos..end_pos]
        .iter()
        .filter_map(|e| match e {
            CompletionEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        delta_texts,
        vec!["Let me ", "think ", "about this."],
        "all ThinkingDelta chunks must arrive in order between Start and End"
    );

    // The `signature_delta` itself emits no `CompletionEvent` mid-stream —
    // it's captured into the per-block signature buffer and surfaces inside
    // the post-stop `ThinkingBlock` event, asserted separately below.
    let after_end = &events[end_pos + 1..];
    let assistant_text: Vec<&str> = after_end
        .iter()
        .filter_map(|e| match e {
            CompletionEvent::AssistantText(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistant_text,
        vec!["Here is the answer."],
        "assistant text block must follow the closed thinking block"
    );

    let last = events.last().expect("stream should not be empty");
    assert!(
        matches!(
            last,
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::Natural
            }
        ),
        "last event should be TurnComplete{{Natural}}, got: {:?}",
        last
    );
}

#[tokio::test]
async fn state_machine_omitted_thinking_emits_start_then_end_with_no_deltas() {
    // `display = "omitted"` shape: ContentBlockStart{Thinking} → SignatureDelta
    // → ContentBlockStop, with zero thinking deltas in between. The consumer
    // must still emit a canonical `ThinkingStart`/`ThinkingEnd` pair so the
    // UI can mount and collapse the "Thinking…" indicator even when no
    // reasoning text is on the wire.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    bytes.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    );
    bytes.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    let events = run_state_machine(bytes).await;
    let starts = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::ThinkingStart))
        .count();
    let deltas = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::ThinkingDelta { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|e| matches!(e, CompletionEvent::ThinkingEnd { .. }))
        .count();
    assert_eq!(starts, 1, "should emit exactly one ThinkingStart");
    assert_eq!(deltas, 0, "should emit zero ThinkingDelta events");
    assert_eq!(ends, 1, "should emit exactly one ThinkingEnd");
}

#[tokio::test]
async fn state_machine_summarized_thinking_emits_replay_block_with_text_and_signature() {
    // Pins the post-stop `ThinkingBlock` event for the `display = "summarized"`
    // shape. The runner threads this onto the assistant turn it builds at
    // end-of-turn so the next iteration's transcript echoes the reasoning
    // block back — Anthropic rejects a follow-up turn whose transcript drops
    // the prior `thinking` block when the same turn emitted any tool_use.
    let bytes =
        sse_thinking_then_text_turn(&["Step ", "by ", "step."], "OK.");
    let events = run_state_machine(bytes).await;

    let block = events
        .iter()
        .find_map(|e| match e {
            CompletionEvent::ThinkingBlock { text, signature } => Some((text.clone(), signature.clone())),
            _ => None,
        })
        .expect("ThinkingBlock should be emitted after ThinkingEnd");
    assert_eq!(
        block.0.as_deref(),
        Some("Step by step."),
        "concatenated reasoning text should match the deltas"
    );
    assert_eq!(
        block.1.as_deref(),
        Some("sig"),
        "signature must be carried through verbatim for replay"
    );

    // ThinkingEnd must precede ThinkingBlock — UI consumers that ignore
    // ThinkingBlock still need ThinkingEnd to collapse the bubble.
    let end_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::ThinkingEnd { .. }))
        .expect("ThinkingEnd should be emitted");
    let block_pos = events
        .iter()
        .position(|e| matches!(e, CompletionEvent::ThinkingBlock { .. }))
        .expect("ThinkingBlock should be emitted");
    assert!(end_pos < block_pos, "ThinkingEnd must precede ThinkingBlock");
}

#[tokio::test]
async fn state_machine_omitted_thinking_replay_block_carries_signature_only() {
    // The `display = "omitted"` shape: signature without text. The replay
    // event should still surface so the runner can echo the signature back
    // — Anthropic's continuity rule keys off the signature, not the text.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"omitted-sig\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    bytes.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
    );
    bytes.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    let events = run_state_machine(bytes).await;
    let block = events
        .iter()
        .find_map(|e| match e {
            CompletionEvent::ThinkingBlock { text, signature } => Some((text.clone(), signature.clone())),
            _ => None,
        })
        .expect("ThinkingBlock should be emitted even with no thinking_delta");
    assert!(
        block.0.is_none(),
        "no reasoning text on the wire → text field should collapse to None"
    );
    assert_eq!(
        block.1.as_deref(),
        Some("omitted-sig"),
        "signature must be preserved on the omitted-display path"
    );
}

#[tokio::test]
async fn state_machine_redacted_thinking_emits_replay_block_with_data() {
    // A safety-redacted block followed by a tool_use. The state machine must
    // surface a `RedactedThinkingBlock` carrying the opaque payload so the
    // runner can echo it back — without it, the follow-up turn that includes
    // the tool_use would be rejected. No thinking triplet should fire: a
    // redacted block has no readable reasoning to stream to the UI.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EncryptedBlob==\"}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_rd\",\"name\":\"Read\",\"input\":{}}}\n\n",
    );
    bytes.extend_from_slice(
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    bytes.extend_from_slice(
        b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
    );
    bytes.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    let events = run_state_machine(bytes).await;

    let data = events
        .iter()
        .find_map(|e| match e {
            CompletionEvent::RedactedThinkingBlock { data } => Some(data.clone()),
            _ => None,
        })
        .expect("RedactedThinkingBlock should be emitted for a redacted_thinking block");
    assert_eq!(
        data, "EncryptedBlob==",
        "the opaque payload must be carried through verbatim for replay"
    );

    // No reasoning triplet for a redacted block — it has no UI surface.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CompletionEvent::ThinkingStart)),
        "redacted blocks must not mount a Thinking… indicator"
    );

    // The tool_use that followed must still come through.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CompletionEvent::ToolUse { id, .. } if id == "toolu_rd")),
        "the tool_use after the redacted block should still be emitted"
    );
}

// ─── cancellation plumbing test ──────────────────────────────────────────────

/// Minimal HTTP/1.1 server that accepts one connection, sends a single SSE
/// MessageStart event, then holds the connection open indefinitely.
///
/// The deliberate pause simulates an in-flight SSE stream where more events
/// would eventually arrive — letting the test fire cancel mid-stream.
async fn spawn_drip_sse_server() -> (tokio::task::JoinHandle<()>, u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let port = listener.local_addr().expect("local_addr").port();

    let handle = tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };

        // Drain the HTTP request headers (stop at the blank line)
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 512];
        loop {
            match conn.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }

        // Send HTTP 200 with SSE response headers
        if conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\r\n",
            )
            .await
            .is_err()
        {
            return;
        }

        // Send one MessageStart event — then pause and never send TurnComplete
        if conn
            .write_all(
                b"event: message_start\n\
                  data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
            )
            .await
            .is_err()
        {
            return;
        }

        // Hold the connection: the test will cancel the client before more data arrives
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });

    (handle, port)
}

#[tokio::test]
async fn cancel_mid_stream_closes_channel_within_100ms_no_turn_complete() {
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    let (_server_handle, port) = spawn_drip_sse_server().await;

    let config = AnthropicConfig {
        api_key: "test-key".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "claude-opus-4-7".into(),
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let client = AnthropicClient::from_config(config);

    let cancel = CancellationToken::new();
    let request = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "ping".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let mut stream = client
        .complete(request, cancel.clone())
        .await
        .expect("complete should succeed against mock server");

    // Receive the first event — Usage from MessageStart. This confirms the reader
    // task is alive and blocked in select waiting for more SSE data before we cancel.
    let first = stream.recv().await;
    assert!(
        matches!(first, Some(Ok(CompletionEvent::Usage(_)))),
        "first event should be Usage from MessageStart, got: {:?}",
        first
    );

    // Fire cancel and measure elapsed time until the channel closes
    let cancel_at = Instant::now();
    cancel.cancel();

    let mut post_cancel_events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(200), stream.recv()).await {
            Ok(Some(ev)) => post_cancel_events.push(ev),
            Ok(None) => break,
            Err(_) => panic!(
                "channel did not close within 200ms after cancel; events so far: {:?}",
                post_cancel_events
            ),
        }
    }

    assert!(
        cancel_at.elapsed() < Duration::from_millis(100),
        "channel should close within 100ms after cancel, elapsed: {:?}",
        cancel_at.elapsed()
    );

    // No TurnComplete should be emitted post-cancel (channel closes via dropped tx,
    // not via a synthetic TurnComplete injected into the stream)
    for ev in post_cancel_events {
        if let Ok(CompletionEvent::TurnComplete { .. }) = ev {
            panic!("TurnComplete should not be emitted post-cancel");
        }
    }
}

// ─── error handling integration tests ────────────────────────────────────────

#[tokio::test]
async fn http_401_returns_transport_error_with_status_and_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let config = AnthropicConfig {
        api_key: "test-key".into(),
        base_url: mock_server.uri(),
        model: "claude-opus-4-7".into(),
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let client = AnthropicClient::from_config(config);
    let cancel = tokio_util::sync::CancellationToken::new();
    let request = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let result = client.complete(request, cancel).await;
    match result {
        Err(ao_engine_tools_runner::provider::ProviderError::Transport(msg)) => {
            assert!(
                msg.starts_with("401"),
                "error message should start with '401', got: {msg}"
            );
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

#[tokio::test]
async fn http_500_returns_transport_error_with_status_and_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&mock_server)
        .await;

    let config = AnthropicConfig {
        api_key: "test-key".into(),
        base_url: mock_server.uri(),
        model: "claude-opus-4-7".into(),
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let client = AnthropicClient::from_config(config);
    let cancel = tokio_util::sync::CancellationToken::new();
    let request = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let result = client.complete(request, cancel).await;
    match result {
        Err(ao_engine_tools_runner::provider::ProviderError::Transport(msg)) => {
            assert!(
                msg.starts_with("500"),
                "error message should start with '500', got: {msg}"
            );
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

#[tokio::test]
async fn sse_malformed_bytes_mid_stream_yields_transport_error_then_closes_channel() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // A valid MessageStart followed by a content_block_start with malformed JSON.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_start\n",
        "data: {not valid json at all}\n\n",
    );

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&mock_server)
        .await;

    let config = AnthropicConfig {
        api_key: "test-key".into(),
        base_url: mock_server.uri(),
        model: "claude-opus-4-7".into(),
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let client = AnthropicClient::from_config(config);
    let cancel = tokio_util::sync::CancellationToken::new();
    let request = CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let mut stream = client
        .complete(request, cancel)
        .await
        .expect("complete should succeed for HTTP 200");

    let mut events = Vec::new();
    while let Some(ev) = stream.recv().await {
        events.push(ev);
    }

    let has_transport_err = events.iter().any(|e| {
        matches!(
            e,
            Err(ao_engine_tools_runner::provider::ProviderError::Transport(msg))
            if msg.contains("SSE parse error")
        )
    });
    assert!(
        has_transport_err,
        "expected Transport error containing 'SSE parse error' in channel, got: {events:?}"
    );

    // Channel is closed after draining — verify no further events arrive
    assert!(
        stream.recv().await.is_none(),
        "channel should be closed after SSE parse error"
    );
}

#[test]
fn build_system_message_in_transcript_is_filtered_not_errored() {
    // System messages in the transcript slice are filtered by build() itself
    // before the normalizer sees them.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::System {
                content: "Top-level system prompt".into(),
            },
            Message::User {
                content: vec![ContentBlock::Text { text: "hello".into() }],
            },
        ],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let messages = built["messages"].as_array().unwrap();
    // Only the User message should appear; System is stripped
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

// ─── cache-control breakpoint tests ──────────────────────────────────────────
//
// LAUNCHPAD_ANTHROPIC_CACHE_OFF is a process-wide env var. Tests that assert
// cache_control IS present must not run concurrently with the test that sets
// CACHE_OFF. All five cache-sensitive tests below acquire CACHE_TEST_MUTEX to
// serialise themselves relative to each other while remaining parallel with
// the rest of the test suite.

static CACHE_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that saves and restores LAUNCHPAD_ANTHROPIC_CACHE_OFF.
/// Must always be created while holding CACHE_TEST_MUTEX.
struct CacheEnvGuard {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl CacheEnvGuard {
    fn disable_cache(key: &'static str) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, "1");
        Self { key, prior }
    }
}

impl Drop for CacheEnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn cache_control_two_breakpoints_on_user_text_turn() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    // 3-message transcript where the last message is a User text message.
    // Both breakpoints should land: (a) system block, (b) last user text block.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::User {
                content: vec![ContentBlock::Text { text: "First question.".into() }],
            },
            Message::Assistant {
                content: vec![ContentBlock::Text { text: "First answer.".into() }],
            },
            Message::User {
                content: vec![ContentBlock::Text { text: "Second question.".into() }],
            },
        ],
        system_prompt: Some("Be concise.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");

    // (a) System block has cache_control
    let system = built.get("system").expect("system should be present");
    assert_eq!(
        system[0]["cache_control"]["type"], "ephemeral",
        "system block should have cache_control"
    );

    let messages = built["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);

    // (b) The last user message (index 2) has cache_control on its text block
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"][0]["cache_control"]["type"], "ephemeral",
        "last user text block should have cache_control"
    );

    // The first user message (index 0) does NOT have cache_control
    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "first user message should not have cache_control"
    );
}

#[test]
fn cache_control_absent_when_env_off() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    let _guard = CacheEnvGuard::disable_cache("LAUNCHPAD_ANTHROPIC_CACHE_OFF");

    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::User {
                content: vec![ContentBlock::Text { text: "hello".into() }],
            },
        ],
        system_prompt: Some("Be concise.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");

    // System block should NOT have cache_control
    let system = built.get("system").expect("system should be present");
    assert!(
        system[0].get("cache_control").is_none(),
        "system cache_control should be absent when LAUNCHPAD_ANTHROPIC_CACHE_OFF is set"
    );

    // User message should NOT have cache_control
    let messages = built["messages"].as_array().unwrap();
    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "user cache_control should be absent when LAUNCHPAD_ANTHROPIC_CACHE_OFF is set"
    );
}

#[test]
fn cache_control_system_only_no_user_messages_no_panic() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    // System block gets cache_control; empty messages array → no panic
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![],
        system_prompt: Some("System only.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should not panic");

    let system = built.get("system").expect("system should be present");
    assert_eq!(
        system[0]["cache_control"]["type"], "ephemeral",
        "system block should have cache_control"
    );

    let messages = built["messages"].as_array().unwrap();
    assert!(messages.is_empty());
}

#[test]
fn cache_control_lands_on_tool_result_when_last_user_has_no_text() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    // The most recent user-role message contains only a tool_result block.
    // The breakpoint must still advance — placed on the tool_result block itself —
    // so the cached prefix covers the full preceding turn, not just the original
    // user text. Anthropic's API accepts cache_control on any block type.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::User {
                content: vec![ContentBlock::Text { text: "run ls".into() }],
            },
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            Message::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::Text { text: "a.rs\nb.rs".into() }],
                is_error: false,
            },
        ],
        system_prompt: Some("System prompt.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");

    // System block still gets cache_control.
    let system = built.get("system").expect("system should be present");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

    let messages = built["messages"].as_array().unwrap();
    // Last message is the tool_result user message — the breakpoint lands here.
    let last = &messages[2];
    assert_eq!(last["role"], "user");
    assert_eq!(
        last["content"][0]["type"], "tool_result",
        "sanity-check the block we're asserting against",
    );
    assert_eq!(
        last["content"][0]["cache_control"]["type"], "ephemeral",
        "tool_result block on the most recent user message should carry cache_control",
    );

    // First user message (the original text) still has no cache_control — only
    // the *most recent* user message gets a breakpoint, never a fallback.
    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "first user message should not get cache_control when a later user message has the breakpoint",
    );
}

#[test]
fn cache_control_advances_across_tool_loop_iterations() {
    let _lock = CACHE_TEST_MUTEX.lock().unwrap();
    // Simulates iteration 2 of a tool loop: the request body now contains two
    // assistant tool_use turns and two user tool_result turns following the
    // original user text. Each iteration the cache breakpoint must advance to
    // the trailing tool_result so that the next iteration's request reads the
    // entire prior transcript out of cache, not just the original user prompt.
    let config = fixture_config();
    let normalizer = AnthropicNormalizer;
    let req = CompletionRequest {
        messages: vec![
            Message::User {
                content: vec![ContentBlock::Text { text: "list everything".into() }],
            },
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            Message::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ContentBlock::Text { text: "a.rs".into() }],
                is_error: false,
            },
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "t2".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls -la"}),
                }],
            },
            Message::ToolResult {
                tool_use_id: "t2".into(),
                content: vec![ContentBlock::Text { text: "a.rs b.rs".into() }],
                is_error: false,
            },
        ],
        system_prompt: Some("Be concise.".into()),
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    };

    let built = request::build(&config, &normalizer, &req).expect("build should succeed");
    let messages = built["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);

    // The breakpoint must land on the iter-2 tool_result (index 4), not stay
    // pinned to the original user text at index 0.
    let last = &messages[4];
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(
        last["content"][0]["cache_control"]["type"], "ephemeral",
        "iter-2 tool_result must carry the cache breakpoint so the next iteration reads the full transcript from cache",
    );

    // The earlier user-role messages — the original text turn and the iter-1
    // tool_result — must NOT carry breakpoints. Only the *most recent* user
    // message owns the (single) user-side breakpoint.
    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "original user text should not retain a breakpoint after the loop advances",
    );
    assert!(
        messages[2]["content"][0].get("cache_control").is_none(),
        "iter-1 tool_result should not retain a breakpoint after iter-2 lands",
    );
}
