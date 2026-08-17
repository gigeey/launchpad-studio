//! Permission primitives shared between tools, the runner's permission
//! gate, hook subprocesses, and the user-facing prompt bridge.
//!
//! The runner combines four signals when deciding whether to execute a
//! tool call:
//!
//! 1. The tool's own opinion, returned from
//!    [`IoTool::check_permissions`](crate::IoTool::check_permissions) /
//!    [`EngineTool::check_permissions`](crate::EngineTool::check_permissions).
//! 2. Pre-tool-use hook decisions loaded from `settings.json`.
//! 3. The active [`PermissionMode`] (default / plan / bypass).
//! 4. The user's answer to an `Ask` prompt, fenced by a
//!    [`DenialTracker`] so the runner stops asking after a configurable
//!    number of refusals.
//!
//! Tools express one of six [`PermissionDecision`] variants. The runner
//! folds them with hook outcomes and the prompt bridge into a final
//! verdict; that combinator lives in the runner crate.

use std::sync::Arc;

use ao_protocol::agent::AgentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool's (or hook's) opinion about whether an invocation should be
/// allowed to proceed. The runner consults this BEFORE executing the
/// tool, and then combines it with hook outcomes and the active
/// [`PermissionMode`].
///
/// `Allow`, `AllowOnce`, and `AllowSession` differ only in how long the
/// permission is remembered: `Allow` is the unconditional default,
/// `AllowOnce` is single-shot (the runner does not cache it), and
/// `AllowSession` instructs the runner to remember the answer for the
/// rest of the session. The execution semantics for the current call
/// are identical for all three.
///
/// `Mutate` lets the tool rewrite its own input before execution; the
/// runner replaces the original input with `updated_input` and proceeds
/// as if the model had emitted the new shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowOnce,
    AllowSession,
    Deny { reason: String },
    Ask { reason: String },
    Mutate { updated_input: Value },
}

/// The session-wide permission posture chosen by the operator.
///
/// - `Default` — consult tool decisions, hooks, and the prompt bridge.
/// - `Plan` — read-only tools may run; everything else is denied so the
///   model can draft a plan without touching the world.
/// - `BypassPermissions` — short-circuit every gate to `Allow`. Reserved
///   for trusted automation (CI, scripted runs).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Default,
    Plan,
    BypassPermissions,
}

/// Whether a human operator is present and watching this session.
///
/// - `Interactive` (default) — a human is at the keyboard. The runner
///   raises permission dialogs, hides autonomous-only tools (e.g. `Sleep`),
///   and drains injected messages immediately each turn.
/// - `Autonomous` — no human is attending in real time. The runner
///   registers the autonomous tool tier (Sleep et al.), emits a pacing
///   guidance block in the system prompt, holds low-priority injected
///   messages across `Sleep` turns, and auto-resolves permission `ask`
///   decisions without raising a dialog.
///
/// Hangs off `RunnerConfig` parallel to [`PermissionMode`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Interactive,
    Autonomous,
}

/// Tracks how many times a given (agent, tool) pair has been denied
/// during a session. The runner consults this counter before re-asking
/// the user about an `Ask` decision; once the count reaches the
/// configured threshold, subsequent `Ask` outcomes auto-deny without
/// disturbing the user.
///
/// Implementations must be cheaply clonable as `Arc<dyn DenialTracker>`
/// and are required to be thread-safe (`Send + Sync`).
pub trait DenialTracker: Send + Sync {
    /// Increment the denial counter for `(agent_id, tool_name)`.
    fn record_denial(&self, agent_id: &str, tool_name: &str);

    /// Read the denial counter for `(agent_id, tool_name)`.
    fn count(&self, agent_id: &str, tool_name: &str) -> u32;

    /// Drop every counter associated with `session_id`. Implementations
    /// that key only on `(agent_id, tool_name)` and ignore session may
    /// no-op here, but the runner relies on this method to reclaim
    /// memory between sessions.
    fn reset_session(&self, session_id: &str);
}

/// Default tracker that never records anything and always returns 0.
/// Used by SDK / non-interactive sessions that don't run the prompt
/// bridge, and as the placeholder in [`PermissionContext::default`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopDenialTracker;

impl DenialTracker for NoopDenialTracker {
    fn record_denial(&self, _agent_id: &str, _tool_name: &str) {}

    fn count(&self, _agent_id: &str, _tool_name: &str) -> u32 {
        0
    }

    fn reset_session(&self, _session_id: &str) {}
}

/// Context handed to a tool's `check_permissions` method (and threaded
/// through the runner's permission combinator). Carries identity, the
/// active mode, and a handle to the session's denial counter so the
/// combinator can fence runaway `Ask` loops.
#[derive(Clone)]
pub struct PermissionContext {
    pub mode: PermissionMode,
    pub agent_id: AgentId,
    pub session_id: String,
    pub denial_tracker: Arc<dyn DenialTracker>,
}

impl PermissionContext {
    /// Construct a context with the supplied identity and mode, backed
    /// by a [`NoopDenialTracker`]. Tests and SDK callers that don't
    /// care about denial fencing use this; the runner replaces the
    /// tracker with a real shared instance.
    pub fn new(
        mode: PermissionMode,
        agent_id: impl Into<AgentId>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            mode,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            denial_tracker: Arc::new(NoopDenialTracker),
        }
    }

    pub fn with_tracker(mut self, tracker: Arc<dyn DenialTracker>) -> Self {
        self.denial_tracker = tracker;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(decision: &PermissionDecision) -> PermissionDecision {
        let s = serde_json::to_string(decision).expect("serialize");
        serde_json::from_str(&s).expect("deserialize")
    }

    #[test]
    fn permission_decision_allow_round_trips() {
        let d = PermissionDecision::Allow;
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn permission_decision_allow_once_round_trips() {
        let d = PermissionDecision::AllowOnce;
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn permission_decision_allow_session_round_trips() {
        let d = PermissionDecision::AllowSession;
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn permission_decision_deny_round_trips() {
        let d = PermissionDecision::Deny {
            reason: "policy says no".into(),
        };
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn permission_decision_ask_round_trips() {
        let d = PermissionDecision::Ask {
            reason: "needs approval".into(),
        };
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn permission_decision_mutate_round_trips() {
        let d = PermissionDecision::Mutate {
            updated_input: json!({"file_path": "/tmp/safe.txt"}),
        };
        assert_eq!(round_trip(&d), d);
    }

    #[test]
    fn decision_serializes_with_tagged_decision_field() {
        let d = PermissionDecision::Deny {
            reason: "nope".into(),
        };
        let v: Value = serde_json::to_value(&d).unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "nope");
    }

    #[test]
    fn permission_mode_default_is_default_variant() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn permission_mode_round_trips() {
        for m in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::BypassPermissions,
        ] {
            let s = serde_json::to_string(&m).unwrap();
            let back: PermissionMode = serde_json::from_str(&s).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn noop_denial_tracker_returns_zero() {
        let t = NoopDenialTracker;
        // Recording is a no-op even when called repeatedly.
        for _ in 0..10 {
            t.record_denial("agent-a", "Bash");
        }
        assert_eq!(t.count("agent-a", "Bash"), 0);
        assert_eq!(t.count("any-agent", "any-tool"), 0);
        // Reset is a no-op too — and does not panic.
        t.reset_session("session-1");
        assert_eq!(t.count("agent-a", "Bash"), 0);
    }

    #[test]
    fn noop_tracker_dyn_dispatch_is_object_safe() {
        let t: Arc<dyn DenialTracker> = Arc::new(NoopDenialTracker);
        t.record_denial("a", "t");
        assert_eq!(t.count("a", "t"), 0);
    }

    #[test]
    fn permission_context_carries_identity_and_default_tracker() {
        let ctx = PermissionContext::new(PermissionMode::Default, "agent-x", "session-y");
        assert_eq!(ctx.mode, PermissionMode::Default);
        assert_eq!(ctx.agent_id, "agent-x");
        assert_eq!(ctx.session_id, "session-y");
        assert_eq!(ctx.denial_tracker.count("agent-x", "Bash"), 0);
    }
}
