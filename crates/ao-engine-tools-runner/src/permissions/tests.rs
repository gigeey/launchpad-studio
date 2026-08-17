//! Unit tests for the permission subsystem (rule parser + combinator).
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` so private items
//! remain in scope.

use ao_engine_tools_core::PermissionDecision;
use serde_json::json;

use crate::permissions::rule::{parse_rule, rule_matches, RuleParseError};

fn allow() -> PermissionDecision {
    PermissionDecision::Allow
}

fn deny() -> PermissionDecision {
    PermissionDecision::Deny {
        reason: "test".into(),
    }
}

// ---------- parse_rule ----------

#[test]
fn parse_rule_with_args_extracts_tool_name_and_pattern() {
    let rule = parse_rule("Bash(git *)", allow()).expect("parse");
    assert_eq!(rule.tool_name, "Bash");
    assert!(rule.arg_pattern.is_some(), "expected arg pattern");
    assert!(matches!(rule.decision, PermissionDecision::Allow));

    // The compiled glob should match the canonical command string.
    let matcher = rule.arg_pattern.as_ref().unwrap();
    assert!(matcher.is_match("git push origin main"));
    assert!(!matcher.is_match("rm -rf /"));
}

#[test]
fn parse_rule_bare_tool_name_has_no_arg_pattern() {
    let rule = parse_rule("Bash", deny()).expect("parse");
    assert_eq!(rule.tool_name, "Bash");
    assert!(rule.arg_pattern.is_none(), "bare rule should have no glob");
    assert!(matches!(rule.decision, PermissionDecision::Deny { .. }));
}

#[test]
fn parse_rule_trims_surrounding_whitespace_and_around_tool_name() {
    let rule = parse_rule("  Bash ( git * )  ", allow()).expect("parse");
    assert_eq!(rule.tool_name, "Bash");
    let matcher = rule.arg_pattern.expect("glob compiled");
    // Whitespace inside the glob is preserved verbatim, so the pattern
    // is literally ` git * ` — match the corresponding input shape.
    assert!(matcher.is_match(" git push "));
}

#[test]
fn parse_rule_rejects_empty_string() {
    let err = parse_rule("", allow()).expect_err("empty must fail");
    assert!(matches!(err, RuleParseError::Empty), "got {err:?}");
}

#[test]
fn parse_rule_rejects_whitespace_only() {
    let err = parse_rule("   ", allow()).expect_err("whitespace must fail");
    assert!(matches!(err, RuleParseError::Empty), "got {err:?}");
}

#[test]
fn parse_rule_rejects_missing_closing_paren() {
    let err = parse_rule("Bash(git *", allow()).expect_err("missing close");
    assert!(
        matches!(err, RuleParseError::UnbalancedParens(_)),
        "got {err:?}"
    );
}

#[test]
fn parse_rule_rejects_stray_closing_paren() {
    let err = parse_rule("Bash)", allow()).expect_err("stray close");
    assert!(
        matches!(err, RuleParseError::UnbalancedParens(_)),
        "got {err:?}"
    );
}

#[test]
fn parse_rule_rejects_trailing_chars_after_close_paren() {
    let err = parse_rule("Bash(git *)abc", allow()).expect_err("trailing");
    assert!(
        matches!(err, RuleParseError::UnbalancedParens(_)),
        "got {err:?}"
    );
}

#[test]
fn parse_rule_rejects_empty_tool_name() {
    let err = parse_rule("(git *)", allow()).expect_err("empty tool");
    assert!(matches!(err, RuleParseError::EmptyToolName), "got {err:?}");
}

#[test]
fn parse_rule_surfaces_invalid_glob_error() {
    // Unclosed character class is rejected by globset's compiler.
    let err = parse_rule("Bash([abc)", allow()).expect_err("invalid glob");
    assert!(
        matches!(err, RuleParseError::InvalidGlob { .. }),
        "got {err:?}"
    );
}

// ---------- rule_matches ----------

#[test]
fn rule_matches_bash_command_glob_matches_matching_command() {
    let rule = parse_rule("Bash(git *)", allow()).unwrap();
    assert!(rule_matches(
        &rule,
        "Bash",
        &json!({ "command": "git push origin main" })
    ));
}

#[test]
fn rule_matches_bash_command_glob_rejects_non_matching_command() {
    let rule = parse_rule("Bash(git *)", allow()).unwrap();
    assert!(!rule_matches(
        &rule,
        "Bash",
        &json!({ "command": "rm -rf /" })
    ));
}

#[test]
fn rule_matches_read_path_glob_matches_under_etc() {
    let rule = parse_rule("Read(/etc/**)", deny()).unwrap();
    assert!(rule_matches(
        &rule,
        "Read",
        &json!({ "file_path": "/etc/passwd" })
    ));
}

#[test]
fn rule_matches_read_path_glob_rejects_unrelated_path() {
    let rule = parse_rule("Read(/etc/**)", deny()).unwrap();
    assert!(!rule_matches(
        &rule,
        "Read",
        &json!({ "file_path": "/home/user/file.txt" })
    ));
}

#[test]
fn rule_matches_bare_tool_rule_matches_any_input() {
    let rule = parse_rule("Bash", deny()).unwrap();
    assert!(rule_matches(&rule, "Bash", &json!({ "command": "anything" })));
    assert!(rule_matches(&rule, "Bash", &json!({})));
    assert!(rule_matches(&rule, "Bash", &json!(null)));
}

#[test]
fn rule_matches_returns_false_when_tool_names_differ() {
    let rule = parse_rule("Bash(git *)", allow()).unwrap();
    assert!(!rule_matches(
        &rule,
        "Read",
        &json!({ "command": "git status" })
    ));

    // Even bare rules must match the tool name exactly.
    let bare = parse_rule("Bash", deny()).unwrap();
    assert!(!rule_matches(&bare, "Read", &json!({})));
}

#[test]
fn rule_matches_unknown_tool_falls_back_to_compact_json_string() {
    // Unknown tool name → matcher runs against `serde_json::to_string`
    // of the input. `{ "foo": "bar" }` serializes to `{"foo":"bar"}`,
    // so a glob containing `foo` matches and one containing `qux`
    // doesn't. (Avoid `{...}` literals in the pattern itself — globset
    // treats braces as alternation syntax.)
    let positive = parse_rule("CustomTool(*foo*)", allow()).unwrap();
    assert!(rule_matches(
        &positive,
        "CustomTool",
        &json!({ "foo": "bar" })
    ));
    assert!(!rule_matches(
        &positive,
        "CustomTool",
        &json!({ "other": "value" })
    ));

    let negative = parse_rule("CustomTool(*qux*)", allow()).unwrap();
    assert!(!rule_matches(
        &negative,
        "CustomTool",
        &json!({ "foo": "bar" })
    ));
}

#[test]
fn rule_matches_edit_and_write_use_file_path_field() {
    let edit = parse_rule("Edit(/tmp/**)", allow()).unwrap();
    assert!(rule_matches(
        &edit,
        "Edit",
        &json!({ "file_path": "/tmp/scratch.txt" })
    ));

    let write = parse_rule("Write(/tmp/**)", allow()).unwrap();
    assert!(rule_matches(
        &write,
        "Write",
        &json!({ "file_path": "/tmp/out.log" })
    ));
    assert!(!rule_matches(
        &write,
        "Write",
        &json!({ "file_path": "/etc/passwd" })
    ));
}

#[test]
fn rule_matches_webfetch_uses_url_field() {
    let rule = parse_rule("WebFetch(https://*.internal/**)", allow()).unwrap();
    assert!(rule_matches(
        &rule,
        "WebFetch",
        &json!({ "url": "https://api.internal/v1/ping" })
    ));
    assert!(!rule_matches(
        &rule,
        "WebFetch",
        &json!({ "url": "https://example.com/api" })
    ));
}

#[test]
fn rule_matches_runskill_uses_skill_field() {
    let rule = parse_rule("RunSkill(review:*)", allow()).unwrap();
    assert!(rule_matches(&rule, "RunSkill", &json!({ "skill": "review:foo" })));
    assert!(!rule_matches(&rule, "RunSkill", &json!({ "skill": "deploy:bar" })));
}

#[test]
fn rule_matches_with_missing_canonical_field_falls_back_to_compact_json() {
    // Bash rule but the input lacks `command` — falls through to the
    // compact-JSON branch, which won't match `git *`.
    let rule = parse_rule("Bash(git *)", allow()).unwrap();
    assert!(!rule_matches(
        &rule,
        "Bash",
        &json!({ "not_command": "git push" })
    ));
}

// ---------- evaluate_permission (decision combinator) ----------

mod gate {
    use std::sync::Arc;

    use ao_engine_tools_core::{
        DenialTracker, PermissionContext, PermissionDecision, PermissionMode,
    };
    use serde_json::json;

    use crate::hooks::HookOutcome;
    use crate::hooks::config::PermissionsConfig;
    use crate::permissions::{PermissionVerdict, evaluate_permission};
    use crate::prompt_bridge::{AskOutcome, InMemoryDenialTracker, ScriptedBridge, StubBridge};

    fn settings(threshold: u32) -> PermissionsConfig {
        PermissionsConfig {
            concurrent_tool_cap: 10,
            deny_count_threshold: threshold,
            rules: Vec::new(),
        }
    }

    fn ctx(mode: PermissionMode) -> (PermissionContext, Arc<InMemoryDenialTracker>) {
        let tracker = Arc::new(InMemoryDenialTracker::new());
        let dyn_tracker: Arc<dyn DenialTracker> = tracker.clone();
        let pc = PermissionContext::new(mode, "agent-x", "session-y").with_tracker(dyn_tracker);
        (pc, tracker)
    }

    // --- Rule 1: BypassPermissions short-circuits to Allow. ---

    #[tokio::test]
    async fn rule1_bypass_mode_short_circuits_to_allow_regardless_of_tool_deny() {
        let (pc, _) = ctx(PermissionMode::BypassPermissions);
        let v = evaluate_permission(
            PermissionDecision::Deny {
                reason: "would deny".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "rm -rf /"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    #[tokio::test]
    async fn rule1_bypass_mode_ignores_hook_deny_too() {
        let (pc, _) = ctx(PermissionMode::BypassPermissions);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Deny {
                reason: "hook nope".into(),
            },
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    // --- Rule 2: hook outcome (non-Continue) wins over tool decision. ---

    #[tokio::test]
    async fn rule2_hook_deny_overrides_tool_allow() {
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Deny {
                reason: "hook says no".into(),
            },
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(reason.contains("hook says no"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rule2_hook_mutate_overrides_tool_allow_with_allow_mutated() {
        let (pc, _) = ctx(PermissionMode::Default);
        let mutated = json!({"file_path": "/tmp/scratch.txt"});
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Mutate {
                updated_input: mutated.clone(),
            },
            &settings(3),
            &pc,
            &StubBridge,
            "Edit",
            &json!({"file_path": "/etc/passwd"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::AllowMutated(mutated));
    }

    #[tokio::test]
    async fn rule2_hook_allow_overrides_tool_deny() {
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Deny {
                reason: "tool refuses".into(),
            },
            HookOutcome::Allow,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    #[tokio::test]
    async fn rule2_hook_continue_falls_through_to_tool_decision() {
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    // --- Rule 3: every Allow* variant resolves to Allow. ---

    #[tokio::test]
    async fn rule3_every_allow_variant_yields_allow() {
        for d in [
            PermissionDecision::Allow,
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowSession,
        ] {
            let (pc, _) = ctx(PermissionMode::Default);
            let v = evaluate_permission(
                d.clone(),
                HookOutcome::Continue,
                &settings(3),
                &pc,
                &StubBridge,
                "Bash",
                &json!({}),
                false,
                ao_engine_tools_core::SessionKind::Interactive,
                &[],
            )
            .await;
            assert_eq!(v, PermissionVerdict::Allow, "decision={d:?}");
        }
    }

    // --- Rule 4: tool Mutate yields AllowMutated. ---

    #[tokio::test]
    async fn rule4_tool_mutate_yields_allow_mutated_with_updated_input() {
        let (pc, _) = ctx(PermissionMode::Default);
        let mutated = json!({"file_path": "/tmp/scratch"});
        let v = evaluate_permission(
            PermissionDecision::Mutate {
                updated_input: mutated.clone(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Edit",
            &json!({"file_path": "/etc/passwd"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::AllowMutated(mutated));
    }

    // --- Rule 5: tool Deny yields Deny with reason. ---

    #[tokio::test]
    async fn rule5_tool_deny_yields_deny_carrying_reason() {
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Deny {
                reason: "policy says no".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(reason.contains("policy says no"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // --- Rule 6: Ask consults the denial counter and the bridge. ---

    #[tokio::test]
    async fn rule6_tool_ask_invokes_bridge_and_allow_yields_allow() {
        let (pc, _) = ctx(PermissionMode::Default);
        let bridge = ScriptedBridge::new([AskOutcome::Allow]);
        let v = evaluate_permission(
            PermissionDecision::Ask {
                reason: "needs approval".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &bridge,
            "Bash",
            &json!({"command": "ls"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
        assert_eq!(bridge.remaining(), 0, "bridge must have been consulted");
    }

    #[tokio::test]
    async fn rule6_tool_ask_denied_by_bridge_increments_denial_tracker() {
        let (pc, tracker) = ctx(PermissionMode::Default);
        let bridge = ScriptedBridge::new([AskOutcome::Deny]);
        assert_eq!(tracker.count("agent-x", "Bash"), 0, "starts at 0");

        let v = evaluate_permission(
            PermissionDecision::Ask {
                reason: "approval".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &bridge,
            "Bash",
            &json!({"command": "ls"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        match v {
            PermissionVerdict::Deny(_) => {}
            other => panic!("expected Deny, got {other:?}"),
        }
        assert_eq!(
            tracker.count("agent-x", "Bash"),
            1,
            "denial tracker should have been incremented exactly once"
        );
    }

    #[tokio::test]
    async fn rule6_ask_at_or_past_threshold_auto_denies_without_calling_bridge() {
        let (pc, tracker) = ctx(PermissionMode::Default);
        // Seed the tracker so it's already at the threshold.
        for _ in 0..3 {
            tracker.record_denial("agent-x", "Bash");
        }
        // Bridge is scripted to Allow — the gate must not consult it.
        let bridge = ScriptedBridge::new([AskOutcome::Allow]);

        let v = evaluate_permission(
            PermissionDecision::Ask {
                reason: "needs approval".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &bridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(
                    reason.contains("threshold"),
                    "expected counter-exhausted reason, got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
        // Bridge stayed un-popped — proves auto-deny short-circuited.
        assert_eq!(bridge.remaining(), 1);
        // Tracker count stays at threshold; we never re-incremented.
        assert_eq!(tracker.count("agent-x", "Bash"), 3);
    }

    #[tokio::test]
    async fn rule6_hook_ask_routes_through_bridge_too() {
        // When the hook produces Ask, the gate must still consult the
        // bridge — the hook's Ask path is treated identically to a tool
        // Ask once it has overridden the tool's decision.
        let (pc, _) = ctx(PermissionMode::Default);
        let bridge = ScriptedBridge::new([AskOutcome::Allow]);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Ask {
                reason: "double-check".into(),
            },
            &settings(3),
            &pc,
            &bridge,
            "Bash",
            &json!({}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
        assert_eq!(bridge.remaining(), 0);
    }

    // --- Rule 7: Plan mode denies tools where mutates_for_input = true. ---

    #[tokio::test]
    async fn rule7_plan_mode_allows_non_mutating_tool() {
        // Any tool with mutates_for_input = false is allowed in plan mode.
        for tool in ["Read", "Glob", "Grep", "Brief", "AskUserQuestionWithForm"] {
            let (pc, _) = ctx(PermissionMode::Plan);
            let v = evaluate_permission(
                PermissionDecision::Allow,
                HookOutcome::Continue,
                &settings(3),
                &pc,
                &StubBridge,
                tool,
                &json!({"file_path": "/etc/passwd"}),
                false, // mutates_for_input = false
                ao_engine_tools_core::SessionKind::Interactive,
                &[],
            )
            .await;
            assert_eq!(v, PermissionVerdict::Allow, "tool={tool}");
        }
    }

    #[tokio::test]
    async fn rule7_plan_mode_denies_mutating_tool_with_plan_mode_reason() {
        let (pc, _) = ctx(PermissionMode::Plan);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "ls"}),
            true, // mutates_for_input = true
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(reason) => {
                let lower = reason.to_ascii_lowercase();
                assert!(
                    lower.contains("plan mode"),
                    "expected plan-mode reason, got: {reason}"
                );
                assert!(
                    reason.contains("Bash"),
                    "expected reason to name the tool, got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rule7_plan_mode_denies_edit_and_write_as_mutating() {
        for tool in ["Edit", "Write"] {
            let (pc, _) = ctx(PermissionMode::Plan);
            let v = evaluate_permission(
                PermissionDecision::Allow,
                HookOutcome::Continue,
                &settings(3),
                &pc,
                &StubBridge,
                tool,
                &json!({"file_path": "/tmp/x.txt"}),
                true, // mutates_for_input = true
                ao_engine_tools_core::SessionKind::Interactive,
                &[],
            )
            .await;
            match v {
                PermissionVerdict::Deny(reason) => assert!(
                    reason.to_ascii_lowercase().contains("plan mode"),
                    "tool={tool} got: {reason}"
                ),
                other => panic!("expected Deny for {tool}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn rule7_plan_mode_demotes_allow_mutated_for_mutating_tool() {
        let (pc, _) = ctx(PermissionMode::Plan);
        let mutated = json!({"file_path": "/tmp/x"});
        let v = evaluate_permission(
            PermissionDecision::Mutate {
                updated_input: mutated,
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Edit",
            &json!({"file_path": "/etc/passwd"}),
            true,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(reason) => assert!(
                reason.to_ascii_lowercase().contains("plan mode"),
                "got: {reason}"
            ),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rule7_plan_mode_leaves_tool_deny_untouched() {
        // A pre-existing Deny verdict should survive plan-mode layering
        // (plan mode never UPGRADES a Deny).
        let (pc, _) = ctx(PermissionMode::Plan);
        let v = evaluate_permission(
            PermissionDecision::Deny {
                reason: "tool said no".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({}),
            true,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(
                    reason.contains("tool said no"),
                    "plan mode should not have rewritten the original reason; got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rule7_non_plan_mode_ignores_mutates_for_input_flag() {
        // In Default mode, mutates_for_input = true must NOT deny an
        // otherwise-allowed tool. Byte-identical to pre-story behaviour.
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Edit",
            &json!({"file_path": "/etc/passwd"}),
            true, // mutates_for_input = true, but mode is Default
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(
            v,
            PermissionVerdict::Allow,
            "Default mode must not apply the plan-mode deny rule"
        );
    }

    // --- settings rules apply, classification flows into denial ---

    fn settings_with_rule(match_str: &str, decision_str: &str) -> PermissionsConfig {
        use crate::hooks::config::RawPermissionRule;
        PermissionsConfig {
            concurrent_tool_cap: 10,
            deny_count_threshold: 3,
            rules: vec![RawPermissionRule {
                r#match: match_str.to_string(),
                decision: decision_str.to_string(),
            }],
        }
    }

    // Simulates what BashTool::check_permissions returns: Ask with classification tag.
    fn bash_ask(cmd: &str, classification: &str) -> PermissionDecision {
        PermissionDecision::Ask {
            reason: format!("[classification: {classification}] execute bash: {cmd}"),
        }
    }

    #[tokio::test]
    async fn bash_deny_rule_includes_classification_in_denial_message() {
        // register a permission rule that denies Bash(git push *)
        // invoke Bash with command `git push origin main`
        // assert the resulting denial message contains [classification: GitMutating]
        let (pc, _) = ctx(PermissionMode::Default);
        let s = settings_with_rule("Bash(git push *)", "deny");
        let tool_decision = bash_ask("git push origin main", "GitMutating");

        let v = evaluate_permission(
            tool_decision,
            HookOutcome::Continue,
            &s,
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "git push origin main"}),
            true,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(
                    reason.contains("[classification: GitMutating]"),
                    "expected [classification: GitMutating] in denial, got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_no_matching_allow_rule_classification_in_user_deny() {
        // When no rule matches, the tool's Ask flows to the bridge (StubBridge
        // always denies), and the classification appears in the "user denied: …" message.
        let (pc, _) = ctx(PermissionMode::Default);
        // Rule only matches git push, NOT ls commands.
        let s = settings_with_rule("Bash(git push *)", "deny");
        let tool_decision = bash_ask("ls /tmp", "ReadOnly");

        let v = evaluate_permission(
            tool_decision,
            HookOutcome::Continue,
            &s,
            &pc,
            &StubBridge, // StubBridge denies all prompts
            "Bash",
            &json!({"command": "ls /tmp"}),
            true,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(
                    reason.contains("[classification: ReadOnly]"),
                    "expected [classification: ReadOnly] in denial, got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_allow_rule_drops_classification_silently() {
        // When a matching allow rule fires, the verdict is Allow — the
        // classification never appears in any output. Proves the UX layer
        // does not leak into the model's success view.
        let (pc, _) = ctx(PermissionMode::Default);
        let s = settings_with_rule("Bash(ls *)", "allow");
        let tool_decision = bash_ask("ls /tmp", "ReadOnly");

        let v = evaluate_permission(
            tool_decision,
            HookOutcome::Continue,
            &s,
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "ls /tmp"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        assert_eq!(v, PermissionVerdict::Allow, "allow rule must not surface classification");
    }

    #[tokio::test]
    async fn settings_rules_empty_list_unchanged_behavior() {
        // With no rules, the gate behaves exactly as before (tool's Allow → Allow).
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Allow,
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "ls"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    #[tokio::test]
    async fn hook_deny_overrides_allow_rule() {
        // Hook outcome takes priority over settings rules. Even if a rule
        // says Allow, a hook Deny wins.
        let (pc, _) = ctx(PermissionMode::Default);
        let s = settings_with_rule("Bash", "allow");

        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "needs approval".into() },
            HookOutcome::Deny { reason: "hook blocked it".into() },
            &s,
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "git push"}),
            true,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        match v {
            PermissionVerdict::Deny(reason) => {
                assert!(reason.contains("hook blocked it"), "got: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── Autonomous ask resolution ───────────────────────────────────────────

    #[tokio::test]
    async fn autonomous_ask_with_no_approve_rules_auto_denies() {
        // In Autonomous sessions an Ask with no matching auto-approve rules must
        // return AutoDeny — never calling the UserPromptBridge.
        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "needs approval".into() },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge, // StubBridge would deny if called — but must NOT be called
            "Bash",
            &json!({"command": "rm -rf /tmp/x"}),
            false,
            ao_engine_tools_core::SessionKind::Autonomous,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::AutoDeny(reason) => {
                assert!(reason.contains("Bash"), "expected tool name in reason, got: {reason}");
                assert!(
                    reason.contains("autonomous session"),
                    "expected autonomous session in reason, got: {reason}"
                );
            }
            other => panic!("expected AutoDeny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn autonomous_ask_with_matching_auto_approve_rule_allows() {
        // A matching auto-approve rule overrides the Ask in Autonomous sessions.
        use crate::permissions::rule::parse_rule;
        let (pc, _) = ctx(PermissionMode::Default);
        let allow_rule = parse_rule("Bash(ls *)", PermissionDecision::Allow).unwrap();
        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "needs approval".into() },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "ls /tmp"}),
            false,
            ao_engine_tools_core::SessionKind::Autonomous,
            &[allow_rule],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow, "matching auto-approve rule must yield Allow");
    }

    // ── RunSkill permission gate ─────────────────────────────────────────────

    #[tokio::test]
    async fn runskill_review_allow_rule_permits_matching_skill() {
        // RunSkill(review:*) allow rule matches review:foo → Allow without consulting bridge.
        let (pc, _) = ctx(PermissionMode::Default);
        let s = settings_with_rule("RunSkill(review:*)", "allow");

        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "run skill: review:foo".into() },
            HookOutcome::Continue,
            &s,
            &pc,
            &StubBridge,
            "RunSkill",
            &json!({"skill": "review:foo"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
    }

    #[tokio::test]
    async fn runskill_review_allow_rule_denies_non_matching_skill() {
        // RunSkill(review:*) allow rule does not match deploy:bar → Ask reaches StubBridge → Deny.
        let (pc, _) = ctx(PermissionMode::Default);
        let s = settings_with_rule("RunSkill(review:*)", "allow");

        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "run skill: deploy:bar".into() },
            HookOutcome::Continue,
            &s,
            &pc,
            &StubBridge,
            "RunSkill",
            &json!({"skill": "deploy:bar"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(_) => {}
            other => panic!("expected Deny for non-matching skill, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runskill_no_rules_allow_via_permitting_bridge() {
        // With no rules configured the Ask falls through to the bridge.
        // A bridge that allows preserves the pre-gate Allow behavior.
        let (pc, _) = ctx(PermissionMode::Default);
        let bridge = ScriptedBridge::new([AskOutcome::Allow]);

        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "run skill: any-skill".into() },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &bridge,
            "RunSkill",
            &json!({"skill": "any-skill"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        assert_eq!(v, PermissionVerdict::Allow);
        assert_eq!(bridge.remaining(), 0, "bridge must have been consulted");
    }

    #[tokio::test]
    async fn interactive_ask_still_calls_bridge() {
        // Confirming that Interactive sessions reach the bridge for Ask decisions
        // (byte-identical behaviour to the prior implementation).
        let (pc, _) = ctx(PermissionMode::Default);
        // StubBridge always denies — so we get Deny, not AutoDeny.
        let v = evaluate_permission(
            PermissionDecision::Ask { reason: "needs approval".into() },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            &StubBridge,
            "Bash",
            &json!({"command": "rm -rf /tmp"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;
        match v {
            PermissionVerdict::Deny(_) => {}
            other => panic!("expected Deny (from bridge), got {other:?}"),
        }
    }

    // ── End-to-end: full gate driving a live, form-backed permission bridge ──
    //
    // The other Ask tests in this module hand `evaluate_permission` a
    // `ScriptedBridge`, which skips the form machinery entirely. This test
    // wires the *real* `LivePermissionBridge` (the bridge native interactive
    // sessions use) into the gate and proves the whole chain end to end:
    //
    //   gate sees Ask
    //     → raises LivePermissionBridge::ask
    //       → emits a UserEvent::FormRequest through the form channel
    //         → a peer "operator" task delivers an "Allow" selection
    //       → the bridge maps the answer to AskOutcome::Allow
    //     → the gate returns PermissionVerdict::Allow
    //
    // This is the truest in-process realization of "make a tool call that
    // requires the permission bridge" short of standing up the HTTP route.
    #[tokio::test]
    async fn gate_drives_live_permission_bridge_form_to_allow() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use ao_engine_tools_core::{EventSink, UserEvent};
        use tokio_util::sync::CancellationToken;

        use crate::prompt_bridge::{
            FormAnswer, FormResponse, LiveFormBridge, LivePermissionBridge,
        };

        // These literals mirror the bridge's private form contract
        // (PERM_FIELD_DECISION / PERM_OPT_ALLOW in prompt_bridge::mod). If the
        // bridge renames them this test will fail loudly, which is the point —
        // the operator answer must speak the same field/option ids the bridge
        // minted.
        const DECISION_FIELD: &str = "decision";
        const ALLOW_OPTION: &str = "allow";

        struct CapturingSink {
            events: Arc<Mutex<Vec<UserEvent>>>,
        }
        #[async_trait::async_trait]
        impl EventSink for CapturingSink {
            async fn emit(&self, event: UserEvent) -> Result<(), ao_protocol::error::AoError> {
                self.events.lock().unwrap().push(event);
                Ok(())
            }
        }

        let events = Arc::new(Mutex::new(Vec::<UserEvent>::new()));
        let sink = Arc::new(CapturingSink {
            events: events.clone(),
        }) as Arc<dyn EventSink + Send + Sync>;
        let form_bridge = Arc::new(LiveFormBridge::new(sink));
        let perm_bridge = Arc::new(LivePermissionBridge::new(
            form_bridge.clone(),
            CancellationToken::new(),
        ));

        // Peer "operator": wait for the form to surface, then deliver Allow.
        let deliverer = tokio::spawn({
            let events = events.clone();
            let form_bridge = form_bridge.clone();
            async move {
                let form_id = loop {
                    {
                        let ev = events.lock().unwrap();
                        if let Some(UserEvent::FormRequest { id, .. }) = ev.first() {
                            break id.clone();
                        }
                    }
                    tokio::task::yield_now().await;
                };
                let mut answers = HashMap::new();
                answers.insert(
                    DECISION_FIELD.to_string(),
                    FormAnswer::Selections(vec![ALLOW_OPTION.to_string()]),
                );
                form_bridge
                    .deliver_form_answer(
                        &form_id,
                        FormResponse {
                            form_id: form_id.clone(),
                            answers,
                            ..Default::default()
                        },
                    )
                    .expect("operator answer must reach the pending form");
            }
        });

        let (pc, _) = ctx(PermissionMode::Default);
        let v = evaluate_permission(
            PermissionDecision::Ask {
                reason: "needs approval".into(),
            },
            HookOutcome::Continue,
            &settings(3),
            &pc,
            perm_bridge.as_ref(),
            "Bash",
            &json!({"command": "ls"}),
            false,
            ao_engine_tools_core::SessionKind::Interactive,
            &[],
        )
        .await;

        assert_eq!(
            v,
            PermissionVerdict::Allow,
            "delivered Allow selection must produce an Allow verdict"
        );
        deliverer.await.expect("deliverer task joined");

        // The gate must actually have raised a form (not short-circuited).
        let saw_form = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, UserEvent::FormRequest { .. }));
        assert!(saw_form, "gate must have emitted a FormRequest through the bridge");
    }
}
