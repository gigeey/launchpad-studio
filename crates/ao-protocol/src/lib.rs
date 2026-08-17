pub mod agent;
pub mod agent_home;
pub mod artifact;
pub mod assignment;
pub mod assignment_scratchpad;
pub mod attachment;
pub mod background_activity;
pub mod bookmark;
pub mod changelog;
pub mod channel_connection_state;
pub mod channel_cursor;
pub mod channel_lease;
pub mod contract_primitives;
pub mod conversation_registry;
pub mod data_root;
pub mod delegation;
pub mod error;
pub mod event;
pub mod extractor_contract;
pub mod instruction_file;
pub mod instructions;
pub mod linked_sender_list;
pub mod memory;
pub mod message;
pub mod outcome;
pub mod predicate;
pub mod preferences;
pub mod project;
pub mod reflection_candidate;
pub mod reflection_trigger;
pub mod rules;
pub mod scheduled_task;
pub mod skill_action;
pub mod system_prompt_context;
pub mod slack_connection;
pub mod slack_conversation_registry;
pub mod slack_manifest;
pub mod slack_test_connection;
pub mod slug;
pub mod tasklist;
pub mod team;
pub mod thread;
pub mod transcript;
pub mod watch_contract;
pub mod webhook_filter;
pub mod webhook_template;
pub mod workflow;
pub mod workspaces;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn sample_agent_profile() -> agent::AgentProfile {
        agent::AgentProfile {
            id: "test-agent".to_string(),
            name: "Test Agent".to_string(),
            description: "A test agent".to_string(),
            emoji: None,
            provider: agent::ProviderConfig::Cli(agent::CliProviderConfig {
                command: "claude".to_string(),
                args: vec!["--output-format".to_string(), "json".to_string()],
                normalizer: None,
                output_format: agent::OutputFormat::Json,
                input_mode: agent::InputMode::Arg,
                model_arg: Some("--model".to_string()),
                model_aliases: HashMap::new(),
                system_prompt_arg: Some("--system-prompt".to_string()),
                session_arg: Some("--session".to_string()),
                resume_args: vec!["--resume".to_string()],
                session_id_fields: vec!["session_id".to_string()],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: Some("sonnet".to_string()),
            skills: vec!["coding".to_string()],
            system_prompt: Some("You are a helpful assistant.".to_string()),
            tools: Some(agent::ToolsConfig {
                allow: vec!["Read".to_string()],
                deny: vec![],
                require_approval: vec![],
            }),
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    #[test]
    fn test_agent_profile_yaml_round_trip() {
        let profile = sample_agent_profile();
        let yaml = serde_yaml::to_string(&profile).expect("serialize to YAML");
        let deserialized: agent::AgentProfile =
            serde_yaml::from_str(&yaml).expect("deserialize from YAML");
        assert_eq!(profile, deserialized);
    }

    #[test]
    fn test_provider_config_yaml_tag() {
        let profile = sample_agent_profile();
        let yaml = serde_yaml::to_string(&profile).expect("serialize to YAML");
        assert!(
            yaml.contains("type: Cli"),
            "ProviderConfig should serialize with type: Cli tag in YAML. Got:\n{yaml}"
        );
    }

    #[test]
    fn test_agent_event_json_round_trip() {
        let event = event::AgentEvent {
            event_id: "evt-1".to_string(),
            run_id: "run-1".to_string(),
            seq: 0,
            ts: Utc::now(),
            agent_id: "test-agent".to_string(),
            thread_id: Some("thread-1".to_string()),
            payload: event::AgentEventPayload::TextDelta {
                text: "Hello".to_string(),
            },
        };
        let json = serde_json::to_string(&event).expect("serialize to JSON");
        let deserialized: event::AgentEvent =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(deserialized.event_id, event.event_id);
        assert_eq!(deserialized.run_id, event.run_id);
        assert_eq!(deserialized.seq, event.seq);
    }

    #[test]
    fn test_agent_event_payload_tag_discriminants() {
        let payloads = vec![
            (
                event::AgentEventPayload::RunStarted,
                "RunStarted",
            ),
            (
                event::AgentEventPayload::RunEnded {
                    reason: event::RunEndReason::Completed,
                },
                "RunEnded",
            ),
            (
                event::AgentEventPayload::TextDelta {
                    text: "hi".to_string(),
                },
                "TextDelta",
            ),
            (
                event::AgentEventPayload::TextComplete {
                    text: "hello".to_string(),
                },
                "TextComplete",
            ),
            (
                event::AgentEventPayload::Error {
                    message: "oops".to_string(),
                    recoverable: false,
                },
                "Error",
            ),
            (
                event::AgentEventPayload::Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 5,
                    cache_creation_tokens: 2,
                    total_tokens: 35,
                },
                "Usage",
            ),
        ];

        for (payload, expected_type) in payloads {
            let json = serde_json::to_string(&payload).expect("serialize payload");
            assert!(
                json.contains(&format!("\"type\":\"{expected_type}\"")),
                "Payload type tag should be '{expected_type}'. Got: {json}"
            );
        }
    }

    #[test]
    fn test_transcript_entry_json_round_trip() {
        let entry = transcript::TranscriptEntry {
            ts: Utc::now(),
            role: transcript::TranscriptRole::Agent {
                agent: "test-agent".to_string(),
            },
            content: "Hello, world!".to_string(),
            event_type: "text_complete".to_string(),
            metadata: Some(HashMap::from([(
                "tokens".to_string(),
                serde_json::json!(42),
            )])),
            hidden_from_user: false,
        };
        let json = serde_json::to_string(&entry).expect("serialize to JSON");
        let deserialized: transcript::TranscriptEntry =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(deserialized.content, entry.content);
        assert_eq!(deserialized.event_type, entry.event_type);
    }

    #[test]
    fn test_transcript_role_untagged() {
        // System role serializes as a plain string
        let system = transcript::TranscriptRole::System("user".to_string());
        let json = serde_json::to_string(&system).expect("serialize system role");
        assert_eq!(json, "\"user\"");

        // Agent role serializes as an object
        let agent_role = transcript::TranscriptRole::Agent {
            agent: "test-agent".to_string(),
        };
        let json = serde_json::to_string(&agent_role).expect("serialize agent role");
        assert!(json.contains("\"agent\""));
        let deserialized: transcript::TranscriptRole =
            serde_json::from_str(&json).expect("deserialize agent role");
        match deserialized {
            transcript::TranscriptRole::Agent { agent } => {
                assert_eq!(agent, "test-agent");
            }
            _ => panic!("Expected Agent variant"),
        }
    }

    #[test]
    fn test_thread_json_round_trip() {
        let now = Utc::now();
        let thread = thread::Thread {
            id: "thread-1".to_string(),
            title: Some("Test Thread".to_string()),
            auto_title: None,
            scope: thread::ThreadScope::AgentChat {
                agent_id: "test-agent".to_string(),
            },
            transcript_path: "/tmp/test.jsonl".to_string(),
            kind: thread::ThreadKind::Default,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&thread).expect("serialize to JSON");
        let deserialized: thread::Thread =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(deserialized.id, thread.id);
        assert_eq!(deserialized.title, thread.title);
        assert_eq!(deserialized.kind, thread::ThreadKind::Default);
        match deserialized.scope {
            thread::ThreadScope::AgentChat { agent_id } => {
                assert_eq!(agent_id, "test-agent");
            }
            _ => panic!("Expected AgentChat variant"),
        }
    }

    #[test]
    fn test_thread_branch_kind_json_round_trip() {
        let now = Utc::now();
        let thread = thread::Thread {
            id: "branch-1".to_string(),
            title: Some("Side investigation".to_string()),
            auto_title: None,
            scope: thread::ThreadScope::AgentChat {
                agent_id: "test-agent".to_string(),
            },
            transcript_path: "/tmp/branch.jsonl".to_string(),
            kind: thread::ThreadKind::Branch,
            history_floor_ts: Some(now),
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: Some(thread::BranchSource {
                source_thread_id: "default-test-agent".to_string(),
                branch_at: now,
                source_message_id: Some("msg-42".to_string()),
            }),
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&thread).expect("serialize");
        assert!(json.contains("\"kind\":\"branch\""), "kind should serialize as lowercase: {json}");
        let round: thread::Thread = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round.kind, thread::ThreadKind::Branch);
        assert!(round.history_floor_ts.is_some());
        assert!(round.branch_source.is_some());
    }

    #[test]
    fn test_thread_legacy_payload_back_compat() {
        // Pre-thread serialized shape, missing kind / history_floor_ts /
        // distilled_through_ts / branch_source. Must round-trip via serde
        // defaults — including a row persisted before the watermark field
        // existed at all.
        let json = r#"{
            "id": "t-1",
            "title": "Legacy",
            "scope": { "type": "AgentChat", "agent_id": "a-1" },
            "transcript_path": "/tmp/a-1.jsonl",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let t: thread::Thread = serde_json::from_str(json).expect("legacy payload");
        assert_eq!(t.kind, thread::ThreadKind::Default);
        assert!(t.history_floor_ts.is_none());
        assert!(t.distilled_through_ts.is_none());
        assert!(t.branch_source.is_none());
        assert!(t.archived_at.is_none());
    }

    #[test]
    fn test_distilled_through_ts_round_trips_when_set() {
        let now = Utc::now();
        let mut thread = thread::Thread {
            id: "thread-2".to_string(),
            title: None,
            auto_title: None,
            scope: thread::ThreadScope::AgentChat {
                agent_id: "test-agent".to_string(),
            },
            transcript_path: "/tmp/test2.jsonl".to_string(),
            kind: thread::ThreadKind::Default,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };
        assert!(thread.distilled_through_ts.is_none());

        thread.distilled_through_ts = Some(now);
        let json = serde_json::to_string(&thread).expect("serialize to JSON");
        assert!(json.contains("\"distilled_through_ts\""));
        let deserialized: thread::Thread =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(deserialized.distilled_through_ts, Some(now));
    }

    #[test]
    fn test_assignment_origin_round_trips_when_set() {
        let now = Utc::now();
        let mut thread = thread::Thread {
            id: "thread-3".to_string(),
            title: None,
            auto_title: None,
            scope: thread::ThreadScope::AgentChat {
                agent_id: "test-agent".to_string(),
            },
            transcript_path: "/tmp/test3.jsonl".to_string(),
            kind: thread::ThreadKind::Fresh,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };
        assert!(thread.assignment_origin.is_none());

        thread.assignment_origin = Some(thread::AssignmentBridgeOrigin {
            assignment_id: "assign-1".to_string(),
            run_id: Some("run-1".to_string()),
        });
        let json = serde_json::to_string(&thread).expect("serialize to JSON");
        assert!(json.contains("\"assignment_origin\""));
        assert!(json.contains("\"assignment_id\":\"assign-1\""));
        assert!(json.contains("\"run_id\":\"run-1\""));
        let deserialized: thread::Thread =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(
            deserialized.assignment_origin,
            Some(thread::AssignmentBridgeOrigin {
                assignment_id: "assign-1".to_string(),
                run_id: Some("run-1".to_string()),
            })
        );

        // A Dedicated-policy thread has no owning run — `run_id` is `None`
        // and, thanks to `skip_serializing_if`, absent from the JSON entirely
        // rather than serialized as `null`.
        thread.assignment_origin = Some(thread::AssignmentBridgeOrigin {
            assignment_id: "assign-2".to_string(),
            run_id: None,
        });
        let json = serde_json::to_string(&thread).expect("serialize to JSON");
        assert!(!json.contains("\"run_id\""));
        let deserialized: thread::Thread =
            serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(
            deserialized.assignment_origin,
            Some(thread::AssignmentBridgeOrigin {
                assignment_id: "assign-2".to_string(),
                run_id: None,
            })
        );
    }

    #[test]
    fn test_default_thread_id_helper() {
        assert_eq!(thread::default_thread_id("agent-1"), "default-agent-1");
        assert_ne!(thread::default_thread_id("a"), thread::default_thread_id("b"));
    }

    #[test]
    fn test_queued_message_json_round_trip() {
        let msg = message::QueuedMessage {
            message_id: "msg-1".to_string(),
            content: "Hello".to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: None,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: message::QueuedMessage =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.message_id, msg.message_id);
        assert_eq!(deserialized.content, msg.content);
    }

    #[test]
    fn test_delegation_status_variants() {
        let statuses = vec![
            delegation::DelegationStatus::Completed,
            delegation::DelegationStatus::Failed,
            delegation::DelegationStatus::TimedOut,
            delegation::DelegationStatus::Blocked,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).expect("serialize");
            let deserialized: delegation::DelegationStatus =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_delegation_request_round_trip() {
        let req = delegation::DelegationRequest {
            delegation_id: "d-123".to_string(),
            target_agent_id: "researcher".to_string(),
            task: "Research quantum computing".to_string(),
            prior_context: Some("Previous findings".to_string()),
            working_dir: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let deserialized: delegation::DelegationRequest =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, deserialized);
    }

    #[test]
    fn test_delegation_result_round_trip() {
        let result = delegation::DelegationResult {
            delegation_id: "d-123".to_string(),
            source_agent_id: "researcher".to_string(),
            status: delegation::DelegationStatus::Completed,
            result: "Found 5 papers".to_string(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: delegation::DelegationResult =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_team_member_yaml_round_trip() {
        let member = team::TeamMember {
            agent_id: "writer".to_string(),
            role_description: "Writes documentation".to_string(),
            working_dir: None,
        };
        let yaml = serde_yaml::to_string(&member).expect("serialize to YAML");
        let deserialized: team::TeamMember =
            serde_yaml::from_str(&yaml).expect("deserialize from YAML");
        assert_eq!(member, deserialized);
    }

    #[test]
    fn test_ao_error_display() {
        let err = error::AoError::AgentNotFound("agent-1".to_string());
        assert_eq!(err.to_string(), "Agent not found: agent-1");

        let err = error::AoError::AgentAlreadyExists("agent-1".to_string());
        assert_eq!(err.to_string(), "Agent already exists: agent-1");
    }
}
