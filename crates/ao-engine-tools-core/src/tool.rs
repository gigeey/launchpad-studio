use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    context::RunnerContext,
    output::ToolOutput,
    permissions::{PermissionContext, PermissionDecision},
    policy::LoadPolicy,
};

/// Tools that perform IO against the local environment (filesystem, shell,
/// network, language servers). These are the "Read", "Write", "Edit",
/// "Bash", etc. surface from the master catalog.
///
/// `is_concurrency_safe` controls whether the dispatcher may fan this tool
/// out in parallel within a single assistant turn. Only read-only tools
/// (Read, Grep, Glob, WebFetch, WebSearch) should return `true` — anything
/// that mutates state must remain sequential.
///
/// `check_permissions` lets the tool veto, gate, or rewrite an invocation
/// before the runner spends time on it. The default returns
/// [`PermissionDecision::Allow`]; tools that need finer control (e.g.
/// `Bash` consulting an allow-list, `Edit` requiring confirmation for
/// out-of-cwd writes) override it.
#[async_trait]
pub trait IoTool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::AlwaysLoad
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// Signals that this tool interacts with external or unpredictable
    /// systems (network, subagent spawning, OS side-channels). MCP clients
    /// may apply looser permission-caching when this hint is `true`.
    ///
    /// Default is `false`. Override to `true` for tools that spawn child
    /// agents or make open-ended external calls where the permission cost
    /// should be attributed to the child rather than the spawn call itself.
    fn mcp_open_world_hint(&self) -> bool {
        false
    }

    /// Return `true` to opt this tool into the CLI-mode XML tool catalog.
    ///
    /// Default is `false` (fail-closed / opt-in): forgetting to mark a new
    /// tool means CLI agents simply don't see it, which is a boring miss vs.
    /// accidentally injecting an XML spec for something that requires
    /// `NativeAgentRunner`-only context. Override to `true` only when the
    /// tool's semantics are runner-independent — it must not duplicate a
    /// native binary capability or rely on `NativeAgentRunner`-only state.
    fn cli_compatible(&self) -> bool {
        false
    }

    /// Return `true` if this tool unconditionally mutates the filesystem
    /// regardless of its input. The permission gate consults this flag
    /// (via [`mutates_for_input`](IoTool::mutates_for_input)) to deny
    /// invocations in [`PermissionMode::Plan`].
    ///
    /// Override to `true` for tools whose primary purpose is to write,
    /// edit, or delete files (e.g. `Write`, `Edit`, `Bash`). Read-only
    /// tools leave this at the default `false`.
    fn mutates_filesystem(&self) -> bool {
        false
    }

    /// Return `true` if THIS specific invocation (given `input`) would
    /// mutate the filesystem. Defaults to [`mutates_filesystem`](IoTool::mutates_filesystem).
    ///
    /// Override when the tool's mutation behaviour depends on the input
    /// (e.g. a tool whose `action` field distinguishes reads from writes).
    fn mutates_for_input(&self, _input: &Value) -> bool {
        self.mutates_filesystem()
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError>;
}

/// Tools that operate on the runner itself: the planner, todo store, the
/// `Agent` spawner, `Skill` loader, hooks, scheduling, etc. These are the
/// tools that mutate `RunnerContext` rather than the world.
///
/// Engine tools are conventionally *not* concurrency-safe — most of them
/// are state-mutating turn-control operations.
///
/// `check_permissions` mirrors the `IoTool` hook: tools that should
/// participate in the permission gate override the default; everything
/// else inherits [`PermissionDecision::Allow`].
#[async_trait]
pub trait EngineTool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::AlwaysLoad
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    /// Signals that this tool interacts with external or unpredictable
    /// systems. See [`IoTool::mcp_open_world_hint`] for the full contract.
    fn mcp_open_world_hint(&self) -> bool {
        false
    }

    /// Return `true` to opt this tool into the CLI-mode XML tool catalog.
    ///
    /// Default is `false` (fail-closed / opt-in): forgetting to mark a new
    /// tool means CLI agents simply don't see it, which is a boring miss vs.
    /// accidentally injecting an XML spec for something that requires
    /// `NativeAgentRunner`-only context. Override to `true` only when the
    /// tool's semantics are runner-independent — it must not duplicate a
    /// native binary capability or rely on `NativeAgentRunner`-only state.
    fn cli_compatible(&self) -> bool {
        false
    }

    /// Return `true` if this tool unconditionally mutates the filesystem.
    /// Consult [`mutates_for_input`](EngineTool::mutates_for_input) for
    /// input-conditional checks. See [`IoTool::mutates_filesystem`] for
    /// the full contract.
    fn mutates_filesystem(&self) -> bool {
        false
    }

    /// Return `true` if THIS specific invocation would mutate the filesystem.
    /// Defaults to [`mutates_filesystem`](EngineTool::mutates_filesystem).
    fn mutates_for_input(&self, _input: &Value) -> bool {
        self.mutates_filesystem()
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Smoke-test impl: an IoTool that echoes its input back as Text.
    pub(crate) struct EchoIo;

    #[async_trait]
    impl IoTool for EchoIo {
        fn name(&self) -> &str {
            "echo_io"
        }
        fn description(&self) -> &str {
            "echoes input as text"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"msg": {"type": "string"}}})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn invoke(
            &self,
            input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text(input.to_string()))
        }
    }

    /// A CLI-compatible IoTool — overrides `cli_compatible` to `true`.
    pub(crate) struct EchoIoCompatible;

    #[async_trait]
    impl IoTool for EchoIoCompatible {
        fn name(&self) -> &str {
            "echo_io_compatible"
        }
        fn description(&self) -> &str {
            "cli-compatible echo"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"msg": {"type": "string"}}})
        }
        fn cli_compatible(&self) -> bool {
            true
        }
        async fn invoke(
            &self,
            input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text(input.to_string()))
        }
    }

    #[tokio::test]
    async fn iotool_dispatches_and_returns_text() {
        let t = EchoIo;
        let ctx = RunnerContext::new("sess", "agent").unwrap();
        let out = t.invoke(json!({"msg": "hi"}), &ctx).await.unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.contains("\"msg\":\"hi\"")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn iotool_cli_compatible_defaults_to_false() {
        assert!(!EchoIo.cli_compatible());
    }

    #[test]
    fn iotool_cli_compatible_override_observed() {
        assert!(EchoIoCompatible.cli_compatible());
    }
}
