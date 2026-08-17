//! Unit tests for the BashStatus tool.

use ao_engine_tools_core::{IoTool, Registry, RunnerContext, ToolOutput};
use serde_json::json;

use super::{prompt, BashStatus};
use crate::bash::background::spawn_and_register;

fn test_ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

// ── schema / registration ────────────────────────────────────────────────────

#[test]
fn description_matches_prompt_constant() {
    assert_eq!(BashStatus::default().description(), prompt::DESCRIPTION);
}

#[test]
fn register_bash_status_lookup_succeeds() {
    let mut r = Registry::new();
    super::register_bash_status(&mut r);
    assert!(r.lookup_io("BashStatus").is_some());
}

#[test]
fn input_schema_is_valid_json() {
    let schema: serde_json::Value =
        serde_json::from_str(prompt::INPUT_SCHEMA).expect("INPUT_SCHEMA is valid JSON");
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["process_id"].is_object());
}

// ── unknown id ───────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn unknown_id_returns_validation_error() {
    let ctx = test_ctx();
    let tool = BashStatus;
    let result = tool
        .invoke(json!({"process_id": "bash_99999"}), &ctx)
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("bash_99999"), "error should name the id: {err}");
}

// ── running then exited ──────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn status_running_then_exited() {
    let ctx = test_ctx();

    // Spawn a command that runs briefly and exits.
    let (id, _path) = spawn_and_register("echo hello_bg", &ctx).await.unwrap();

    let tool = BashStatus;

    // Poll until the command exits (or give up after a few iterations).
    let mut last_status = String::new();
    for _ in 0..20 {
        let out = tool
            .invoke(json!({"process_id": id.to_string()}), &ctx)
            .await
            .unwrap();
        if let ToolOutput::Structured(v) = &out {
            last_status = v["status"].as_str().unwrap_or("").to_string();
            if last_status.starts_with("exited") {
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    assert!(
        last_status.starts_with("exited"),
        "expected exited status, got: {last_status}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn status_output_contains_command_output() {
    let ctx = test_ctx();

    let (id, _path) = spawn_and_register("echo unique_marker_xyz", &ctx)
        .await
        .unwrap();

    let tool = BashStatus;
    // Give it time to produce output.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let out = tool
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    if let ToolOutput::Structured(v) = out {
        let output = v["output"].as_str().unwrap_or("");
        assert!(
            output.contains("unique_marker_xyz"),
            "output should contain command output: {output:?}"
        );
        assert!(v["next_offset"].as_u64().unwrap() > 0);
    } else {
        panic!("expected structured output");
    }
}

// ── incremental reads ────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn incremental_offset_yields_only_new_bytes() {
    let ctx = test_ctx();

    // Produce two distinct lines with a tiny pause between them.
    let (id, _path) =
        spawn_and_register("echo line_a; sleep 0.1; echo line_b", &ctx)
            .await
            .unwrap();

    let tool = BashStatus;
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // First read — get everything.
    let out1 = tool
        .invoke(json!({"process_id": id.to_string(), "offset": 0}), &ctx)
        .await
        .unwrap();

    let (first_output, next_offset) = if let ToolOutput::Structured(v) = &out1 {
        (
            v["output"].as_str().unwrap_or("").to_string(),
            v["next_offset"].as_u64().unwrap(),
        )
    } else {
        panic!("expected structured output");
    };

    assert!(first_output.contains("line_a"), "first read: {first_output:?}");

    // Second read at next_offset — should be empty (no new bytes).
    let out2 = tool
        .invoke(
            json!({"process_id": id.to_string(), "offset": next_offset}),
            &ctx,
        )
        .await
        .unwrap();

    if let ToolOutput::Structured(v) = out2 {
        let second_output = v["output"].as_str().unwrap_or("");
        assert!(
            second_output.is_empty(),
            "second read should be empty: {second_output:?}"
        );
    } else {
        panic!("expected structured output");
    }
}
