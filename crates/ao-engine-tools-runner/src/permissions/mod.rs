//! Permission subsystem — rule grammar parser (see [`rule`]) and the
//! decision combinator that fuses tool-declared decisions, hook
//! outcomes, and the user-prompt bridge into a final allow / deny
//! verdict.
//!
//! The combinator entry point is [`evaluate_permission`]. It walks four
//! signals in priority order (active mode → hook outcome → tool
//! decision → user prompt) and returns a [`PermissionVerdict`] the
//! runner can act on without further branching.

use ao_engine_tools_core::{PermissionContext, PermissionDecision, PermissionMode, SessionKind};
use serde_json::Value;

use crate::hooks::HookOutcome;
use crate::hooks::config::PermissionsConfig;
use crate::prompt_bridge::{AskOutcome, AskRequest, UserPromptBridge};
pub use rule::{parse_rule, rule_matches, PermissionRule};

pub mod rule;

/// Final verdict returned by [`evaluate_permission`].
///
/// The runner uses this to decide whether to invoke the tool, what
/// input to pass when invoking, or what error message to emit in a
/// `tool_result` block when the call is denied.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionVerdict {
    /// Proceed with the original input.
    Allow,
    /// Proceed, but replace the original input with this value first.
    AllowMutated(Value),
    /// Deny the call. The runner emits a `tool_result` error block
    /// carrying this reason as its message.
    Deny(String),
    /// Autonomous-session auto-deny: no human was available to approve the
    /// `Ask` decision and the call did not match any `auto_approve` rule.
    /// Emitted as `ToolOutput::Error { recoverable: true }` so the model can
    /// adapt its plan rather than treating this as a hard failure.
    AutoDeny(String),
}

/// Combine a tool-declared [`PermissionDecision`], a pre-tool
/// [`HookOutcome`], and the operator's answer (via `bridge`) into a
/// final [`PermissionVerdict`].
///
/// Rules are evaluated in priority order; the first rule whose
/// premise holds determines the verdict:
///
/// 1. `ctx.mode == BypassPermissions` — short-circuit to
///    [`PermissionVerdict::Allow`] without consulting any other input.
/// 2. The hook returned a non-`Continue` outcome — the hook's verdict
///    replaces the tool's own decision (for example, a hook `Deny`
///    overrides a tool `Allow`; a hook `Mutate` overrides any tool
///    decision and produces [`PermissionVerdict::AllowMutated`]).
/// 3. The effective decision is `Allow` / `AllowOnce` /
///    `AllowSession` — verdict is [`PermissionVerdict::Allow`].
/// 4. The effective decision is `Mutate` — verdict is
///    [`PermissionVerdict::AllowMutated`] carrying `updated_input`.
/// 5. The effective decision is `Deny` — verdict is
///    [`PermissionVerdict::Deny`] with the supplied reason.
/// 6. The effective decision is `Ask` — consult the denial counter on
///    `ctx.denial_tracker`. If the count for `(agent_id, tool_name)`
///    has already reached `settings.deny_count_threshold`, auto-deny
///    without disturbing the user. Otherwise call `bridge.ask(...)`;
///    if the bridge replies [`AskOutcome::Deny`], record a denial via
///    [`DenialTracker::record_denial`](ao_engine_tools_core::DenialTracker::record_denial)
///    and return [`PermissionVerdict::Deny`].
/// 7. If the resulting verdict is `Allow` or `AllowMutated` and
///    `ctx.mode == Plan` and `mutates_for_input` is `true`, demote to
///    [`PermissionVerdict::Deny`]. Tools that report `mutates_for_input =
///    false` (read-only tools, question-asking tools, etc.) flow through
///    unchanged.
///
/// The denial tracker is updated through the trait's
/// `record_denial(agent, tool)` method, which records under an empty
/// session bucket on [`InMemoryDenialTracker`](crate::prompt_bridge::InMemoryDenialTracker).
/// Session-scoped reset still works because `count` keys on
/// `(agent_id, tool_name)` regardless of which session recorded the
/// denial.
pub async fn evaluate_permission(
    tool_decision: PermissionDecision,
    hook_outcome: HookOutcome,
    settings: &PermissionsConfig,
    ctx: &PermissionContext,
    bridge: &dyn UserPromptBridge,
    tool_name: &str,
    input: &Value,
    mutates_for_input: bool,
    session_kind: SessionKind,
    auto_approve: &[PermissionRule],
) -> PermissionVerdict {
    // Rule 1: bypass posture wins over everything else.
    if ctx.mode == PermissionMode::BypassPermissions {
        return PermissionVerdict::Allow;
    }

    // Rule 2: settings rules determine the base decision (overriding the
    // tool's own opinion). First matching rule wins; when a deny rule fires
    // on a Bash Ask that carries a classification tag, the Ask reason is
    // prepended so the model sees "[classification: X]" in the denial.
    let base_decision = {
        let mut matched: Option<PermissionDecision> = None;
        for raw in &settings.rules {
            if let Ok(decision) = raw.to_decision() {
                if let Ok(rule) = parse_rule(&raw.r#match, decision) {
                    if rule_matches(&rule, tool_name, input) {
                        let rule_decision = rule.decision;
                        // When a deny rule fires and the tool had an Ask
                        // containing a classification tag, combine the reasons
                        // so the classification flows into the denial message.
                        let combined = if let PermissionDecision::Deny { reason: ref rule_reason } = rule_decision {
                            if let PermissionDecision::Ask { reason: ref tool_reason } = tool_decision {
                                PermissionDecision::Deny {
                                    reason: format!("{tool_reason}\n{rule_reason}"),
                                }
                            } else {
                                rule_decision
                            }
                        } else {
                            rule_decision
                        };
                        matched = Some(combined);
                        break;
                    }
                }
            }
        }
        matched.unwrap_or(tool_decision)
    };

    // Rule 2b: a non-Continue hook outcome overrides the base decision
    // (hooks take priority over static settings rules).
    let effective = match hook_outcome {
        HookOutcome::Continue => base_decision,
        HookOutcome::Allow => PermissionDecision::Allow,
        HookOutcome::Deny { reason } => PermissionDecision::Deny { reason },
        HookOutcome::Ask { reason } => PermissionDecision::Ask { reason },
        HookOutcome::Mutate { updated_input } => PermissionDecision::Mutate { updated_input },
    };

    // Rules 3-6: turn the effective decision into a verdict.
    let verdict = match effective {
        PermissionDecision::Allow
        | PermissionDecision::AllowOnce
        | PermissionDecision::AllowSession => PermissionVerdict::Allow,

        PermissionDecision::Mutate { updated_input } => {
            PermissionVerdict::AllowMutated(updated_input)
        }

        PermissionDecision::Deny { reason } => PermissionVerdict::Deny(reason),

        PermissionDecision::Ask { reason } => {
            // In Autonomous sessions no human is present to click a dialog.
            // Check the per-launch auto_approve allowlist first; if any rule
            // matches, allow without raising the bridge. Otherwise auto-deny
            // with recoverable=true so the model can adapt.
            if session_kind == SessionKind::Autonomous {
                for rule in auto_approve {
                    if rule_matches(rule, tool_name, input) {
                        return PermissionVerdict::Allow;
                    }
                }
                return PermissionVerdict::AutoDeny(format!(
                    "autonomous session: no approver present and no auto-approve rule matched \
                     for '{tool_name}' — {reason}"
                ));
            }

            let prior = ctx.denial_tracker.count(&ctx.agent_id, tool_name);
            if prior >= settings.deny_count_threshold {
                return PermissionVerdict::Deny(format!(
                    "auto-denied: ({}, {}) reached the denial threshold of {}",
                    ctx.agent_id, tool_name, settings.deny_count_threshold
                ));
            }

            let outcome = bridge
                .ask(AskRequest {
                    tool_name: tool_name.to_string(),
                    input: input.clone(),
                    reason: reason.clone(),
                    agent_id: ctx.agent_id.clone(),
                    session_id: ctx.session_id.clone(),
                })
                .await;

            match outcome {
                AskOutcome::Allow | AskOutcome::AllowOnce | AskOutcome::AllowSession => {
                    PermissionVerdict::Allow
                }
                AskOutcome::Deny => {
                    ctx.denial_tracker
                        .record_denial(&ctx.agent_id, tool_name);
                    PermissionVerdict::Deny(format!("user denied: {reason}"))
                }
            }
        }
    };

    // Rule 7: Plan mode demotes Allow / AllowMutated for tools that
    // report mutates_for_input = true. Non-mutating tools are unaffected.
    // AutoDeny is already a denial — leave it unchanged.
    if ctx.mode == PermissionMode::Plan && mutates_for_input {
        match verdict {
            PermissionVerdict::Allow | PermissionVerdict::AllowMutated(_) => {
                return PermissionVerdict::Deny(format!(
                    "plan mode: tool '{tool_name}' is not allowed to mutate"
                ));
            }
            PermissionVerdict::Deny(_) | PermissionVerdict::AutoDeny(_) => {}
        }
    }

    verdict
}

#[cfg(test)]
mod tests;
