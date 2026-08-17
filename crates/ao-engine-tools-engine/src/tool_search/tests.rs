use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    EngineTool, EventKind, LoadPolicy, NoopTelemetryWriter, Registry, RunnerContext,
    TelemetryWriter, ToolOutput, ToolUsageEvent,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::ToolSearch;

// --- mock tool helpers ---

struct MockDeferred {
    tool_name: &'static str,
    tool_desc: &'static str,
}

#[async_trait]
impl EngineTool for MockDeferred {
    fn name(&self) -> &str {
        self.tool_name
    }
    fn description(&self) -> &str {
        self.tool_desc
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

struct MockAlways {
    tool_name: &'static str,
    tool_desc: &'static str,
}

#[async_trait]
impl EngineTool for MockAlways {
    fn name(&self) -> &str {
        self.tool_name
    }
    fn description(&self) -> &str {
        self.tool_desc
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

// --- test helpers ---

fn build_test_registry() -> Registry {
    let mut reg = Registry::new();
    // Deferred tools with distinguishable names/descriptions
    reg.register_engine(Arc::new(MockDeferred {
        tool_name: "PlanTool",
        tool_desc: "Enter planning mode to organize and plan work tasks.",
    }));
    reg.register_engine(Arc::new(MockDeferred {
        tool_name: "NoteTool",
        tool_desc: "Write a note to document findings and observations.",
    }));
    reg.register_engine(Arc::new(MockDeferred {
        tool_name: "QueryTool",
        tool_desc: "Run a database query and return structured results.",
    }));
    // An always-loaded tool that should never appear in results
    reg.register_engine(Arc::new(MockAlways {
        tool_name: "Brief",
        tool_desc: "Send a brief message summary to the user.",
    }));
    reg.build_deferred_index();
    reg
}

fn make_ctx(
    registry: Registry,
    always_load: HashSet<String>,
    activated: HashSet<String>,
) -> RunnerContext {
    RunnerContext::new("session-1", "agent-1")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(Arc::new(always_load))
        .with_activated_tools(Arc::new(Mutex::new(activated)))
        .with_telemetry(Arc::new(NoopTelemetryWriter))
}

fn result_names(output: &ToolOutput) -> Vec<String> {
    let v = match output {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

fn result_scores(output: &ToolOutput) -> Vec<f64> {
    let v = match output {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["score"].as_f64().unwrap())
        .collect()
}

// --- tests ---

#[tokio::test]
async fn keyword_search_returns_relevant_tools_first() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "plan", "max_results": 3}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    // PlanTool contains "plan" in its name → highest score; others may appear after
    assert!(!names.is_empty(), "should return at least one result");
    assert_eq!(names[0], "PlanTool", "PlanTool should rank first for 'plan'");
}

#[tokio::test]
async fn always_loaded_tools_absent_from_results() {
    let always_load: HashSet<String> = ["PlanTool".to_string()].into();
    let ctx = make_ctx(build_test_registry(), always_load, HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "plan"}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    assert!(
        !names.contains(&"PlanTool".to_string()),
        "PlanTool is in always_load, should not appear in results"
    );
    // Brief is always-load by policy but not in our custom always_load set;
    // it won't appear in the deferred_index regardless (it has AlwaysLoad policy)
    assert!(
        !names.contains(&"Brief".to_string()),
        "Brief has AlwaysLoad policy, should not be in deferred_index"
    );
}

#[tokio::test]
async fn activated_tools_absent_from_results() {
    let activated: HashSet<String> = ["NoteTool".to_string()].into();
    let ctx = make_ctx(build_test_registry(), HashSet::new(), activated);
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "note"}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    assert!(
        !names.contains(&"NoteTool".to_string()),
        "NoteTool is activated, should not appear in results"
    );
}

#[tokio::test]
async fn empty_query_returns_all_unloaded_deferred_alphabetically() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "", "max_results": 100}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    let scores = result_scores(&out);

    // All three deferred tools should appear (Brief is always-load, not in index)
    assert!(names.contains(&"PlanTool".to_string()));
    assert!(names.contains(&"NoteTool".to_string()));
    assert!(names.contains(&"QueryTool".to_string()));
    assert!(!names.contains(&"Brief".to_string()));

    // All scores are 0 for empty query
    assert!(scores.iter().all(|&s| s == 0.0));

    // Results are alphabetically sorted
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "empty query results must be alphabetically sorted");
}

#[tokio::test]
async fn whitespace_only_query_treated_as_empty() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "   ", "max_results": 100}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    let scores = result_scores(&out);

    // Same as empty query: all deferred tools, alphabetically, score 0
    assert_eq!(names.len(), 3);
    assert!(scores.iter().all(|&s| s == 0.0));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[tokio::test]
async fn max_results_caps_the_list() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "", "max_results": 2}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    assert_eq!(names.len(), 2, "max_results=2 must cap the list at 2");
}

#[tokio::test]
async fn missing_query_field_returns_error() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool.invoke(json!({}), &ctx).await.unwrap();
    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "missing query should return ToolOutput::Error"
    );
}

#[tokio::test]
async fn max_results_defaults_to_five() {
    // Build a registry with 6 deferred tools
    let mut reg = Registry::new();
    for i in 0..6 {
        let name = format!("Tool{i:02}");
        reg.register_engine(Arc::new(MockDeferred {
            tool_name: Box::leak(name.into_boxed_str()),
            tool_desc: "a tool",
        }));
    }
    reg.build_deferred_index();
    let ctx = make_ctx(reg, HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": ""}), &ctx)
        .await
        .unwrap();
    let names = result_names(&out);
    assert_eq!(names.len(), 5, "default max_results should be 5");
}

// ---- loaded_deferred_tools mutation tests ----

#[tokio::test]
async fn name_param_deferred_resolves_and_mutates_loaded_deferred_tools() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"name": "PlanTool"}), &ctx)
        .await
        .unwrap();
    // Returns Text output with schema
    assert!(
        matches!(out, ToolOutput::Text(_)),
        "deferred tool via name: should return ToolOutput::Text"
    );
    // Inserted into loaded_deferred_tools
    let loaded = ctx.loaded_deferred_tools.read().unwrap();
    assert!(
        loaded.contains("PlanTool"),
        "PlanTool should be in loaded_deferred_tools after name: resolution"
    );
}

#[tokio::test]
async fn name_param_unknown_returns_error_no_mutation() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"name": "NonExistentTool"}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "unknown tool via name: should return ToolOutput::Error"
    );
    let loaded = ctx.loaded_deferred_tools.read().unwrap();
    assert!(
        loaded.is_empty(),
        "loaded_deferred_tools must not be mutated for unknown tool"
    );
}

#[tokio::test]
async fn name_param_always_load_returns_text_no_mutation() {
    // Brief is registered as AlwaysLoad in build_test_registry
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"name": "Brief"}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Text(_)),
        "AlwaysLoad tool via name: should return ToolOutput::Text"
    );
    let loaded = ctx.loaded_deferred_tools.read().unwrap();
    assert!(
        loaded.is_empty(),
        "AlwaysLoad tool should not be inserted into loaded_deferred_tools"
    );
}

#[tokio::test]
async fn select_deferred_tool_populates_loaded_deferred_tools() {
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let _out = tool
        .invoke(json!({"query": "select:PlanTool,NoteTool"}), &ctx)
        .await
        .unwrap();
    let loaded = ctx.loaded_deferred_tools.read().unwrap();
    assert!(
        loaded.contains("PlanTool"),
        "PlanTool should be in loaded_deferred_tools after select:"
    );
    assert!(
        loaded.contains("NoteTool"),
        "NoteTool should be in loaded_deferred_tools after select:"
    );
}

#[tokio::test]
async fn select_always_load_not_added_to_loaded_deferred_tools() {
    // Brief is always-load; selecting it should NOT add it to loaded_deferred_tools
    let ctx = make_ctx(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let _out = tool
        .invoke(json!({"query": "select:Brief"}), &ctx)
        .await
        .unwrap();
    let loaded = ctx.loaded_deferred_tools.read().unwrap();
    assert!(
        !loaded.contains("Brief"),
        "AlwaysLoad tool should not be inserted into loaded_deferred_tools via select:"
    );
}

// ---- select: activation path tests ----

/// Spy telemetry writer that captures emitted events.
struct SpyTelemetry {
    events: Arc<Mutex<Vec<ToolUsageEvent>>>,
}

impl TelemetryWriter for SpyTelemetry {
    fn emit(&self, event: ToolUsageEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn make_ctx_with_spy(
    registry: Registry,
    always_load: HashSet<String>,
    activated: HashSet<String>,
) -> (RunnerContext, Arc<Mutex<Vec<ToolUsageEvent>>>) {
    let events: Arc<Mutex<Vec<ToolUsageEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let spy = Arc::new(SpyTelemetry {
        events: events.clone(),
    });
    let ctx = RunnerContext::new("session-1", "agent-1")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(Arc::new(always_load))
        .with_activated_tools(Arc::new(Mutex::new(activated)))
        .with_telemetry(spy);
    (ctx, events)
}

fn activated_names(output: &ToolOutput) -> Vec<String> {
    let v = match output {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    v["activated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

fn unresolved_names(output: &ToolOutput) -> Vec<String> {
    let v = match output {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    v["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn select_activates_known_tools_and_returns_schemas() {
    let (ctx, events) = make_ctx_with_spy(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "select:PlanTool,NoteTool"}), &ctx)
        .await
        .unwrap();

    let names = activated_names(&out);
    assert!(names.contains(&"PlanTool".to_string()));
    assert!(names.contains(&"NoteTool".to_string()));
    assert_eq!(unresolved_names(&out).len(), 0);

    // schemas must be present
    let v = match &out {
        ToolOutput::Structured(v) => v,
        _ => panic!(),
    };
    for entry in v["activated"].as_array().unwrap() {
        assert!(entry.get("schema").is_some(), "schema field must be present");
    }

    // two Selected telemetry events emitted
    let ev = events.lock().unwrap();
    assert_eq!(ev.len(), 2);
    assert!(ev.iter().all(|e| matches!(e.kind, EventKind::Selected)));

    // tools added to activated_tools
    let activated = ctx.activated_tools.lock().unwrap();
    assert!(activated.contains("PlanTool"));
    assert!(activated.contains("NoteTool"));
}

#[tokio::test]
async fn select_unknown_names_land_in_unresolved() {
    let (ctx, _) = make_ctx_with_spy(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "select:NoSuchTool"}), &ctx)
        .await
        .unwrap();

    assert_eq!(activated_names(&out).len(), 0);
    assert_eq!(unresolved_names(&out), vec!["NoSuchTool".to_string()]);
}

#[tokio::test]
async fn select_mixed_valid_and_invalid() {
    let (ctx, _) = make_ctx_with_spy(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "select:PlanTool,NoSuchTool"}), &ctx)
        .await
        .unwrap();

    let activated = activated_names(&out);
    let unresolved = unresolved_names(&out);
    assert_eq!(activated, vec!["PlanTool".to_string()]);
    assert_eq!(unresolved, vec!["NoSuchTool".to_string()]);
}

#[tokio::test]
async fn select_already_activated_tool_is_idempotent() {
    let pre_activated: HashSet<String> = ["PlanTool".to_string()].into();
    let (ctx, events) =
        make_ctx_with_spy(build_test_registry(), HashSet::new(), pre_activated);
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "select:PlanTool"}), &ctx)
        .await
        .unwrap();

    // Still returns the tool in activated list with schema
    let names = activated_names(&out);
    assert_eq!(names, vec!["PlanTool".to_string()]);
    assert_eq!(unresolved_names(&out).len(), 0);

    // Telemetry still emitted (idempotent call still records the selection)
    let ev = events.lock().unwrap();
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0].kind, EventKind::Selected));
}

#[tokio::test]
async fn select_always_loaded_tool_returns_schema_and_emits_telemetry() {
    // Brief is registered as AlwaysLoad in build_test_registry
    let always_load: HashSet<String> = ["Brief".to_string()].into();
    let (ctx, events) =
        make_ctx_with_spy(build_test_registry(), always_load, HashSet::new());
    let tool = ToolSearch;
    let out = tool
        .invoke(json!({"query": "select:Brief"}), &ctx)
        .await
        .unwrap();

    let names = activated_names(&out);
    assert_eq!(names, vec!["Brief".to_string()]);
    assert_eq!(unresolved_names(&out).len(), 0);

    let ev = events.lock().unwrap();
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0].kind, EventKind::Selected));
}

#[tokio::test]
async fn select_case_insensitive_prefix() {
    let (ctx, _) = make_ctx_with_spy(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    // Upper-case SELECT: prefix should be treated the same as select:
    let out = tool
        .invoke(json!({"query": "SELECT:PlanTool"}), &ctx)
        .await
        .unwrap();

    assert_eq!(activated_names(&out), vec!["PlanTool".to_string()]);
    assert_eq!(unresolved_names(&out).len(), 0);
}

#[tokio::test]
async fn select_all_unresolved_still_returns_ok() {
    let (ctx, _) = make_ctx_with_spy(build_test_registry(), HashSet::new(), HashSet::new());
    let tool = ToolSearch;
    let result = tool
        .invoke(json!({"query": "select:Ghost1,Ghost2"}), &ctx)
        .await;
    assert!(result.is_ok(), "select with all-unresolved must return Ok");
    let out = result.unwrap();
    assert_eq!(activated_names(&out).len(), 0);
    assert_eq!(unresolved_names(&out).len(), 2);
}
