//! Shared gating for excluding UI-only tools from channel-bridge turns.
//!
//! A channel-bridge thread (Telegram, Discord, email, Slack, ...) has no
//! surface to render an interactive form on, so a tool like
//! `AskUserQuestionWithForm` must never reach the model on such a turn — the
//! backend would otherwise suspend on a form answer nothing can ever
//! deliver. Every agent-runner path needs the exact same predicate and the
//! exact same admission-gate math, so both live here once instead of
//! drifting into separate copies per runner.

use std::collections::HashSet;

use ao_engine_tools_core::{Registry, ToolAdmission};
use ao_protocol::agent::{AgentProfile, ToolsConfig};
use ao_protocol::thread::ChannelBridgeOrigin;

/// Tool names that must be excluded from a channel-bridge turn (e.g. a
/// Telegram-relayed message) regardless of the agent's own `ToolsConfig`,
/// because they render a UI form with no channel-side surface to draw on.
/// Kept as a const, rather than inlined at each call site, so extending the
/// set for a future channel-incompatible tool is a one-line change.
pub const CHANNEL_BLOCKED_TOOLS: &[&str] = &["AskUserQuestionWithForm"];

/// Compute the session admission gate from an agent profile's `ToolsConfig`.
///
/// Returns `None` when no filtering is needed (every registered tool is
/// admitted). Otherwise returns a [`ToolAdmission`] gate that the query loop
/// applies when building each turn's tool array, so a denied tool never reaches
/// the model.
///
/// Semantics:
/// - `tools` is `None`, or both `allow` and `deny` are empty → `None` (no gate)
/// - `allow` is non-empty → closed-world [`ToolAdmission::Allow`] of exactly the
///   named (registered) tools, minus anything also listed in `deny`
/// - `allow` is empty but `deny` is non-empty → open-world [`ToolAdmission::Deny`]
///   of the named tools; every other registered tool stays admitted
///
/// Modeling deny as an exclusion set (rather than a pre-subtracted allow set)
/// keeps it correct for tools registered after this point — e.g. the
/// autonomous-only tools added during session init are admitted unless denied.
///
/// Unknown names in `allow` or `deny` are dropped with a `tracing::warn`. An
/// `Allow` gate that ends up empty also fires a `tracing::warn`.
///
/// `require_approval` is read but otherwise unused here; extend this helper when
/// the runtime permission UI lands — do not add a second filtering pass.
///
/// `extra_deny` is folded in on top of the profile-derived gate: it excludes
/// those names no matter which of the three base shapes (no gate, open-world
/// deny, closed-world allow) the profile produces. Callers pass tool names
/// here for exclusions that don't originate from `ToolsConfig` itself — e.g.
/// channel-bridge turns forcing out UI-form tools — so this function stays
/// decoupled from any particular caller's reason for excluding a tool.
pub fn compute_tool_admission(
    tools: Option<&ToolsConfig>,
    registry: &Registry,
    extra_deny: &[&str],
) -> Option<ToolAdmission> {
    let registered: HashSet<String> = registry.list().into_iter().collect();

    let base = match tools {
        None => None,
        Some(tc) if tc.allow.is_empty() && tc.deny.is_empty() => None,
        Some(tc) if tc.allow.is_empty() => {
            // Open-world: every registered tool except the denied ones.
            let mut deny: HashSet<String> = HashSet::new();
            for name in &tc.deny {
                if registered.contains(name) {
                    deny.insert(name.clone());
                } else {
                    tracing::warn!(
                        "agent tools.deny: unknown tool {:?} dropped from denylist",
                        name
                    );
                }
            }
            Some(ToolAdmission::Deny(deny))
        }
        Some(tc) => {
            // Closed-world: only the named registered tools, minus any also denied.
            let mut allow: HashSet<String> = HashSet::new();
            for name in &tc.allow {
                if registered.contains(name) {
                    allow.insert(name.clone());
                } else {
                    tracing::warn!(
                        "agent tools.allow: unknown tool {:?} dropped from allowlist",
                        name
                    );
                }
            }
            for name in &tc.deny {
                allow.remove(name);
            }
            if allow.is_empty() {
                tracing::warn!(
                    "agent tool config left it with no tools — model receives empty tools array"
                );
            }
            Some(ToolAdmission::Allow(allow))
        }
    };

    if extra_deny.is_empty() {
        return base;
    }

    match base {
        None => {
            let deny: HashSet<String> = extra_deny.iter().map(|s| s.to_string()).collect();
            Some(ToolAdmission::Deny(deny))
        }
        Some(ToolAdmission::Deny(mut deny)) => {
            deny.extend(extra_deny.iter().map(|s| s.to_string()));
            Some(ToolAdmission::Deny(deny))
        }
        Some(ToolAdmission::Allow(mut allow)) => {
            for name in extra_deny {
                allow.remove(*name);
            }
            Some(ToolAdmission::Allow(allow))
        }
    }
}

/// True iff `thread_id` is a dedicated bridge thread of an *enabled* channel
/// binding on `agent` — checked two ways, either sufficient on its own:
///
/// 1. The classic reverse lookup: some binding in `agent.channels` is
///    enabled and its `bridge_thread_id` equals `thread_id`. Covers every
///    channel that provisions exactly one thread per binding (Telegram,
///    Discord, email).
/// 2. `thread_channel_origin` — the thread's own `Thread::channel_origin`,
///    which the caller fetches once from persistence and passes in — names
///    a `binding_id` that's enabled in `agent.channels`. Covers a channel
///    that provisions one thread per *conversation* instead, where no
///    single `bridge_thread_id` ever names the thread at all (Slack; see
///    `ChannelBridgeOrigin`'s docstring and
///    `ao_engine::channels::slack::runner::resolve_bridge_thread`).
///
/// Both checks resolve "enabled" live against the current profile rather
/// than anything cached on the thread, so disabling a binding immediately
/// un-gates every thread it ever bridged, without touching a `Thread` row.
pub fn is_channel_bridge_thread(
    agent: &AgentProfile,
    thread_id: Option<&str>,
    thread_channel_origin: Option<&ChannelBridgeOrigin>,
) -> bool {
    let Some(tid) = thread_id else {
        return false;
    };
    let via_bridge_thread_id = agent
        .channels
        .iter()
        .any(|binding| binding.enabled && binding.bridge_thread_id.as_deref() == Some(tid));
    if via_bridge_thread_id {
        return true;
    }
    thread_channel_origin.is_some_and(|origin| {
        agent
            .channels
            .iter()
            .any(|binding| binding.enabled && binding.binding_id == origin.binding_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ao_protocol::agent::AgentRunnerMode;

    fn make_agent() -> AgentProfile {
        use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
        AgentProfile {
            id: "test-agent".to_string(),
            name: "Test".to_string(),
            description: "".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: Default::default(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: Some("You are a test agent.".to_string()),
            tools: None,
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: false,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Api,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    // -- is_channel_bridge_thread ------------------------------------------

    fn make_telegram_config(enabled: bool, bridge_thread_id: Option<&str>) -> ao_protocol::agent::TelegramConfig {
        ao_protocol::agent::TelegramConfig {
            enabled,
            bot_username: None,
            thread_mode: Default::default(),
            bridge_thread_id: bridge_thread_id.map(|s| s.to_string()),
            allowed_chat_ids: vec![],
            pending_pairing_code: None,
        }
    }

    fn make_channel_binding(
        binding_id: &str,
        kind: ao_protocol::agent::ChannelKind,
        enabled: bool,
        bridge_thread_id: Option<&str>,
    ) -> ao_protocol::agent::ChannelBinding {
        use ao_protocol::agent::{ChannelBinding, ChannelKind, ChannelKindConfig};
        let kind_config = match kind {
            ChannelKind::Telegram => ChannelKindConfig::Telegram {
                bot_username: None,
                thread_mode: Default::default(),
            },
            ChannelKind::Email => ChannelKindConfig::Email {
                address: "agent@example.com".to_string(),
                imap_host: String::new(),
                imap_port: 0,
                smtp_host: String::new(),
                smtp_port: 0,
                poll_secs: 0,
                require_auth_results: true,
            },
            ChannelKind::Discord => ChannelKindConfig::Discord {
                allowed_users: vec![],
                allowed_roles: vec![],
                allowed_channels: vec![],
                dm_role_auth_guild: None,
                require_mention: true,
                thread_follow: Default::default(),
                thread_idle_timeout_minutes: 15,
                thread_message_budget: 10,
                backfill_limit: 20,
            },
            ChannelKind::Slack => ChannelKindConfig::Slack {
                allowed_channels: vec![],
                allowed_users: vec![],
                connection_id: None,
                conversation_mode: Default::default(),
            },
            other => panic!("make_channel_binding: unsupported kind {other:?} in this test helper"),
        };
        ChannelBinding {
            binding_id: binding_id.to_string(),
            kind,
            enabled,
            bridge_thread_id: bridge_thread_id.map(|s| s.to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config,
        }
    }

    #[test]
    fn is_channel_bridge_thread_true_for_telegram_binding_enabled_and_matching() {
        // No-regression case: this is the path that already works today.
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(true, Some("bridge-1"))));
        assert!(is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_true_for_discord_binding_enabled_and_matching() {
        // This is the bug: before the fix, a Discord-only binding was
        // invisible to this predicate because it only read Telegram.
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "discord",
            ao_protocol::agent::ChannelKind::Discord,
            true,
            Some("bridge-1"),
        ));
        assert!(is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_true_for_email_binding_enabled_and_matching() {
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "email",
            ao_protocol::agent::ChannelKind::Email,
            true,
            Some("bridge-1"),
        ));
        assert!(is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_no_channels() {
        let agent = make_agent();
        assert!(!is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_disabled() {
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(false, Some("bridge-1"))));
        assert!(!is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_bridge_thread_id_none() {
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(true, None)));
        assert!(!is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_thread_id_differs() {
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(true, Some("bridge-1"))));
        assert!(!is_channel_bridge_thread(&agent, Some("other-thread"), None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_thread_id_none() {
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(true, Some("bridge-1"))));
        assert!(!is_channel_bridge_thread(&agent, None, None));
    }

    #[test]
    fn is_channel_bridge_thread_false_when_no_binding_matches_thread_id() {
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "discord",
            ao_protocol::agent::ChannelKind::Discord,
            true,
            Some("bridge-1"),
        ));
        assert!(!is_channel_bridge_thread(&agent, Some("no-such-thread"), None));
    }

    #[test]
    fn is_channel_bridge_thread_true_when_only_second_of_multiple_bindings_matches() {
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "telegram",
            ao_protocol::agent::ChannelKind::Telegram,
            true,
            Some("other-thread"),
        ));
        agent.channels.push(make_channel_binding(
            "discord",
            ao_protocol::agent::ChannelKind::Discord,
            true,
            Some("bridge-1"),
        ));
        assert!(is_channel_bridge_thread(&agent, Some("bridge-1"), None));
    }

    #[test]
    fn is_channel_bridge_thread_true_for_slack_via_channel_origin_with_no_bridge_thread_id() {
        // The bug this predicate used to have for Slack: a per-conversation
        // Slack thread never has a matching `bridge_thread_id` anywhere (see
        // `ChannelBridgeOrigin`'s docstring — Slack is one thread per
        // conversation, not per binding), so `bridge_thread_id` is left
        // `None` here on purpose. Only the thread's own recorded
        // `channel_origin` — fetched by the caller and passed in — can name
        // it as a bridge thread.
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "slack",
            ao_protocol::agent::ChannelKind::Slack,
            true,
            None,
        ));
        let origin = ChannelBridgeOrigin {
            kind: ao_protocol::agent::ChannelKind::Slack,
            binding_id: "slack".to_string(),
        };
        assert!(is_channel_bridge_thread(&agent, Some("slack-convo-thread"), Some(&origin)));
    }

    #[test]
    fn is_channel_bridge_thread_false_for_slack_channel_origin_when_binding_disabled() {
        // "Enabled" is always resolved live against the current profile, not
        // cached on the thread — disabling the binding must immediately
        // un-gate every thread it ever bridged, origin-based or not.
        let mut agent = make_agent();
        agent.channels.push(make_channel_binding(
            "slack",
            ao_protocol::agent::ChannelKind::Slack,
            false,
            None,
        ));
        let origin = ChannelBridgeOrigin {
            kind: ao_protocol::agent::ChannelKind::Slack,
            binding_id: "slack".to_string(),
        };
        assert!(!is_channel_bridge_thread(&agent, Some("slack-convo-thread"), Some(&origin)));
    }

    #[test]
    fn is_channel_bridge_thread_false_for_channel_origin_when_binding_no_longer_exists() {
        // The named binding was deleted from the profile entirely (not just
        // disabled) since the thread was stamped. Treated the same as
        // disabled: un-gated.
        let agent = make_agent();
        let origin = ChannelBridgeOrigin {
            kind: ao_protocol::agent::ChannelKind::Slack,
            binding_id: "slack".to_string(),
        };
        assert!(!is_channel_bridge_thread(&agent, Some("slack-convo-thread"), Some(&origin)));
    }

    // -- compute_tool_admission: extra_deny fold ---------------------------

    fn make_registry_with_ask_form() -> Registry {
        let mut registry = Registry::new();
        registry.register_engine(Arc::new(ao_engine_tools_engine::AskUserQuestionWithForm));
        // A second registered tool, used to prove pre-existing entries in the
        // base admission survive the extra_deny fold untouched.
        registry.register_engine(Arc::new(ao_engine_tools_engine::Sleep));
        registry
    }

    #[test]
    fn compute_tool_admission_extra_deny_on_none_base_produces_deny() {
        let registry = make_registry_with_ask_form();
        // No ToolsConfig at all -> base admission is None (no gate).
        let admission = compute_tool_admission(None, &registry, CHANNEL_BLOCKED_TOOLS);
        match &admission {
            Some(ToolAdmission::Deny(deny)) => {
                assert!(deny.contains("AskUserQuestionWithForm"));
            }
            other => panic!("expected Some(Deny(..)), got {other:?}"),
        }
        assert!(!admission.unwrap().permits("AskUserQuestionWithForm"));
    }

    #[test]
    fn compute_tool_admission_extra_deny_on_deny_base_still_denied() {
        let registry = make_registry_with_ask_form();
        let tools = ToolsConfig {
            allow: vec![],
            deny: vec!["Sleep".to_string()],
            require_approval: vec![],
        };
        let admission = compute_tool_admission(Some(&tools), &registry, CHANNEL_BLOCKED_TOOLS);
        match &admission {
            Some(ToolAdmission::Deny(deny)) => {
                assert!(deny.contains("AskUserQuestionWithForm"));
                assert!(deny.contains("Sleep"));
            }
            other => panic!("expected Some(Deny(..)), got {other:?}"),
        }
        assert!(!admission.unwrap().permits("AskUserQuestionWithForm"));
    }

    #[test]
    fn compute_tool_admission_extra_deny_removes_from_allow_base() {
        let registry = make_registry_with_ask_form();
        let tools = ToolsConfig {
            allow: vec!["AskUserQuestionWithForm".to_string()],
            deny: vec![],
            require_approval: vec![],
        };
        let admission = compute_tool_admission(Some(&tools), &registry, CHANNEL_BLOCKED_TOOLS);
        match &admission {
            Some(ToolAdmission::Allow(allow)) => {
                assert!(!allow.contains("AskUserQuestionWithForm"));
            }
            other => panic!("expected Some(Allow(..)), got {other:?}"),
        }
        assert!(!admission.unwrap().permits("AskUserQuestionWithForm"));
    }

    #[test]
    fn compute_tool_admission_non_bridge_turn_leaves_admission_unchanged() {
        let registry = make_registry_with_ask_form();
        // Simulates a non-bridge turn: is_channel_bridge_thread would be
        // false, so the call site passes an empty extra_deny slice.
        let tools = ToolsConfig {
            allow: vec!["AskUserQuestionWithForm".to_string()],
            deny: vec![],
            require_approval: vec![],
        };
        let admission = compute_tool_admission(Some(&tools), &registry, &[]);
        match &admission {
            Some(ToolAdmission::Allow(allow)) => {
                assert!(allow.contains("AskUserQuestionWithForm"));
            }
            other => panic!("expected Some(Allow(..)), got {other:?}"),
        }
        assert!(admission.unwrap().permits("AskUserQuestionWithForm"));

        // Also verify with no ToolsConfig at all -> stays None (no gate),
        // so the tool remains admitted.
        let admission_none = compute_tool_admission(None, &registry, &[]);
        assert!(admission_none.is_none());
    }

    /// An empty `extra_deny` slice must return the base admission completely
    /// untouched, for every one of the three base shapes. This is the
    /// invariant that makes gating bridge-only tools on bridge turns safe:
    /// every non-bridge turn passes `&[]`, so its tool set can never change.
    #[test]
    fn compute_tool_admission_empty_extra_deny_is_byte_identical_to_base_for_all_shapes() {
        let registry = make_registry_with_ask_form();

        // base = None (no ToolsConfig at all).
        let base_none = compute_tool_admission(None, &registry, &[]);
        let with_empty_deny_none = compute_tool_admission(None, &registry, &[]);
        assert_eq!(base_none, with_empty_deny_none);
        assert_eq!(base_none, None);

        // base = Deny(set) (open-world with an explicit denylist).
        let deny_tools = ToolsConfig {
            allow: vec![],
            deny: vec!["Sleep".to_string()],
            require_approval: vec![],
        };
        let base_deny = compute_tool_admission(Some(&deny_tools), &registry, &[]);
        let with_empty_deny_deny = compute_tool_admission(Some(&deny_tools), &registry, &[]);
        assert_eq!(base_deny, with_empty_deny_deny);
        match &base_deny {
            Some(ToolAdmission::Deny(deny)) => {
                assert_eq!(deny.len(), 1);
                assert!(deny.contains("Sleep"));
            }
            other => panic!("expected Some(Deny(..)), got {other:?}"),
        }

        // base = Allow(set) (closed-world with an explicit allowlist).
        let allow_tools = ToolsConfig {
            allow: vec!["AskUserQuestionWithForm".to_string()],
            deny: vec![],
            require_approval: vec![],
        };
        let base_allow = compute_tool_admission(Some(&allow_tools), &registry, &[]);
        let with_empty_deny_allow = compute_tool_admission(Some(&allow_tools), &registry, &[]);
        assert_eq!(base_allow, with_empty_deny_allow);
        match &base_allow {
            Some(ToolAdmission::Allow(allow)) => {
                assert_eq!(allow.len(), 1);
                assert!(allow.contains("AskUserQuestionWithForm"));
            }
            other => panic!("expected Some(Allow(..)), got {other:?}"),
        }
    }

    // -- channel-bridge sessions get a non-interactive form bridge ----------

    #[test]
    fn on_channel_bridge_turn_ask_form_tool_is_excluded_from_admitted_set() {
        // End-to-end sanity check tying the two halves of Part 1 together:
        // a bridge thread's resolved admission gate must deny the form tool,
        // and a non-bridge thread's gate must not.
        let mut agent = make_agent();
        agent.set_telegram_config(Some(make_telegram_config(true, Some("bridge-1"))));
        let registry = make_registry_with_ask_form();

        let on_bridge = is_channel_bridge_thread(&agent, Some("bridge-1"), None);
        assert!(on_bridge);
        let extra_deny: &[&str] = if on_bridge { CHANNEL_BLOCKED_TOOLS } else { &[] };
        let admission = compute_tool_admission(agent.tools.as_ref(), &registry, extra_deny);
        assert!(!admission.unwrap().permits("AskUserQuestionWithForm"));

        let off_bridge = is_channel_bridge_thread(&agent, Some("some-other-thread"), None);
        assert!(!off_bridge);
        let extra_deny: &[&str] = if off_bridge { CHANNEL_BLOCKED_TOOLS } else { &[] };
        let admission = compute_tool_admission(agent.tools.as_ref(), &registry, extra_deny);
        assert!(admission.is_none() || admission.unwrap().permits("AskUserQuestionWithForm"));
    }
}
