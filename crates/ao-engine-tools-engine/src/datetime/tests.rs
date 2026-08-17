use super::DateTime;
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use serde_json::json;
use std::path::PathBuf;

fn fresh_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn happy_path_returns_required_fields() {
    let ctx = fresh_ctx();
    let out = DateTime.invoke(json!({}), &ctx).await.unwrap();
    let body = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {:?}", other),
    };
    assert!(body.contains("Current time:"), "missing header in {:?}", body);
    assert!(body.contains("- UTC: "), "missing UTC line in {:?}", body);
    assert!(body.contains("- Local ("), "missing Local line in {:?}", body);
    assert!(body.contains("- Unix epoch: "), "missing epoch line in {:?}", body);
}

#[tokio::test]
async fn utc_timestamp_parses_back_as_iso_8601() {
    use chrono::DateTime as ChronoDateTime;
    let ctx = fresh_ctx();
    let out = DateTime.invoke(json!({}), &ctx).await.unwrap();
    let body = match out {
        ToolOutput::Text(s) => s,
        _ => unreachable!(),
    };
    // Extract the UTC value between "- UTC: " and the newline
    let utc_line = body
        .lines()
        .find(|l| l.starts_with("- UTC: "))
        .expect("UTC line present");
    let utc_value = utc_line.trim_start_matches("- UTC: ");
    let parsed = ChronoDateTime::parse_from_rfc3339(utc_value);
    assert!(parsed.is_ok(), "UTC line not RFC3339-parseable: {:?}", utc_value);
}

#[tokio::test]
async fn epoch_is_recent_and_positive() {
    let ctx = fresh_ctx();
    let out = DateTime.invoke(json!({}), &ctx).await.unwrap();
    let body = match out {
        ToolOutput::Text(s) => s,
        _ => unreachable!(),
    };
    let epoch_line = body
        .lines()
        .find(|l| l.starts_with("- Unix epoch: "))
        .expect("epoch line present");
    let epoch: i64 = epoch_line
        .trim_start_matches("- Unix epoch: ")
        .parse()
        .expect("epoch is numeric");
    // 2026-01-01T00:00:00Z = 1767225600 ; anything newer than that is fine
    // for this test. Lower-bound check guards against a 0-or-bogus value
    // sneaking past the format string.
    assert!(epoch > 1_767_225_600, "epoch suspiciously low: {}", epoch);
}

#[test]
fn cli_compatible_is_true() {
    assert!(DateTime.cli_compatible());
}

#[test]
fn input_schema_is_empty_object() {
    let schema = DateTime.input_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let props = schema["properties"].as_object().expect("properties present");
    assert!(props.is_empty(), "schema should accept no params, got {:?}", props);
}

#[test]
fn name_is_datetime() {
    assert_eq!(DateTime.name(), "DateTime");
}
