use super::Sleep;
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

fn fresh_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

fn text(out: ToolOutput) -> String {
    match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn normal_sleep_returns_waited_message() {
    let ctx = fresh_ctx();
    let out = Sleep.invoke(json!({"duration_seconds": 1}), &ctx).await.unwrap();
    assert_eq!(text(out), "Waited 1 seconds");
}

#[tokio::test]
async fn early_cancellation_returns_interrupted_message() {
    let ctx = fresh_ctx();
    let token = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let out = Sleep.invoke(json!({"duration_seconds": 60}), &ctx).await.unwrap();
    let msg = text(out);
    assert!(
        msg.starts_with("Interrupted after"),
        "expected interrupted message, got: {:?}",
        msg
    );
}

#[tokio::test]
async fn zero_seconds_is_rejected() {
    let ctx = fresh_ctx();
    let out = Sleep.invoke(json!({"duration_seconds": 0}), &ctx).await.unwrap();
    let msg = text(out);
    assert!(
        msg.contains("at least"),
        "expected at-least error, got: {:?}",
        msg
    );
}

#[tokio::test]
async fn over_max_is_rejected() {
    let ctx = fresh_ctx();
    let out = Sleep.invoke(json!({"duration_seconds": 3601}), &ctx).await.unwrap();
    let msg = text(out);
    assert!(
        msg.contains("at most"),
        "expected at-most error, got: {:?}",
        msg
    );
}

#[tokio::test]
async fn string_encoded_duration_is_coerced() {
    let ctx = fresh_ctx();
    let out = Sleep.invoke(json!({"duration_seconds": "1"}), &ctx).await.unwrap();
    assert_eq!(text(out), "Waited 1 seconds");
}

#[tokio::test]
async fn invalid_duration_returns_error_string() {
    let ctx = fresh_ctx();
    let out = Sleep.invoke(json!({"duration_seconds": "not-a-number"}), &ctx).await.unwrap();
    let msg = text(out);
    assert!(
        msg.contains("must be a positive integer"),
        "expected type error, got: {:?}",
        msg
    );
}

#[tokio::test]
async fn concurrent_execution_with_other_tool() {
    use crate::datetime::DateTime;

    let ctx1 = fresh_ctx();
    let ctx2 = fresh_ctx();
    let (sleep_out, dt_out) = tokio::join!(
        Sleep.invoke(json!({"duration_seconds": 1}), &ctx1),
        DateTime.invoke(json!({}), &ctx2),
    );
    assert_eq!(text(sleep_out.unwrap()), "Waited 1 seconds");
    let dt_text = text(dt_out.unwrap());
    assert!(dt_text.contains("Current time:"), "datetime output missing: {:?}", dt_text);
}

#[test]
fn is_concurrency_safe() {
    assert!(Sleep.is_concurrency_safe());
}

#[test]
fn name_is_sleep() {
    assert_eq!(Sleep.name(), "Sleep");
}
