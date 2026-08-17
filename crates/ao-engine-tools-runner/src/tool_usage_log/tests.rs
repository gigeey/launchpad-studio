use std::path::PathBuf;

use ao_engine_tools_core::{EventKind, TelemetryWriter, ToolUsageEvent};
use chrono::Utc;
use serde_json::Value;
use tempfile::TempDir;

use super::JsonlTelemetryWriter;

fn make_event() -> ToolUsageEvent {
    ToolUsageEvent {
        agent_id: "test-agent".to_string(),
        session_id: "session-1".to_string(),
        tool_name: "TestTool".to_string(),
        kind: EventKind::Invoked,
        ts: Utc::now(),
        metadata: Value::Object(Default::default()),
    }
}

fn rotated(path: &PathBuf) -> PathBuf {
    use std::ffi::OsString;
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

fn line_count(path: &PathBuf) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

#[tokio::test]
async fn fewer_than_limit_produces_single_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tool_usage.jsonl");
    let writer = JsonlTelemetryWriter::new(path.clone());

    for _ in 0..100 {
        writer.emit(make_event());
    }
    writer.flush().await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 100);
    for line in &lines {
        serde_json::from_str::<Value>(line).expect("each line is valid JSON");
    }
    assert!(!rotated(&path).exists(), ".1 file must not exist");
}

#[tokio::test]
async fn rotation_triggered_at_10001_events() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tool_usage.jsonl");
    // Channel must hold all 10001 events so none are dropped before flush.
    let writer = JsonlTelemetryWriter::new_with_capacity(path.clone(), 15_000);

    for _ in 0..10001 {
        writer.emit(make_event());
    }
    writer.flush().await;

    let r = rotated(&path);
    assert!(r.exists(), "rotated file must exist");
    assert_eq!(line_count(&r), 10000, ".jsonl.1 must have 10000 lines");
    assert_eq!(line_count(&path), 1, "fresh .jsonl must have 1 line");
}

#[tokio::test]
async fn second_rotation_overwrites_prior_backup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tool_usage.jsonl");
    // 21001 total events; channel must hold all without dropping.
    let writer = JsonlTelemetryWriter::new_with_capacity(path.clone(), 25_000);

    // First rotation: 10001 events → .jsonl.1 has 10000, .jsonl has 1.
    for _ in 0..10001 {
        writer.emit(make_event());
    }
    // Second rotation: 10000 more events fills .jsonl back to 10000 then
    // triggers another rotate, overwriting the prior .1.
    for _ in 0..10000 {
        writer.emit(make_event());
    }
    writer.flush().await;

    let r = rotated(&path);
    assert!(r.exists());
    assert_eq!(line_count(&r), 10000);
    assert_eq!(line_count(&path), 1);
}

#[tokio::test]
async fn write_failure_does_not_panic() {
    // Make the parent path a file so create_dir_all fails.
    let dir = TempDir::new().unwrap();
    let file_not_dir = dir.path().join("not_a_dir");
    std::fs::write(&file_not_dir, b"content").unwrap();
    let path = file_not_dir.join("tool_usage.jsonl");

    let writer = JsonlTelemetryWriter::new(path);
    writer.emit(make_event());
    writer.flush().await;
    // Must not panic; write error is only logged as a warning.
}
