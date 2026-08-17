//! Unit tests for the Read tool.
//!
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` 
//! per-tool folder layout — `tests.rs` is the same module as `mod.rs`, so
//! private items (constants, helpers) are in scope here.

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use jsonschema::Validator;
use serde_json::json;
use tempfile::TempDir;

use super::{
    Read, CANCELLED_MESSAGE, DEFAULT_LIMIT, EMPTY_FILE_REMINDER, FILE_UNCHANGED_STUB,
    LINE_TRUNCATION_MARKER, MAX_FILE_SIZE_BYTES, MAX_IMAGE_BYTES, MAX_LINE_LENGTH, MAX_PDF_BYTES,
    MAX_TOKENS,
};
use ao_engine_tools_core::ToolBlock;
use base64::Engine as _;

fn ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

fn assert_recoverable_error(out: ToolOutput, contains: &str) {
    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable, "expected recoverable error, got fatal");
            assert!(
                message.contains(contains),
                "error message {message:?} did not contain {contains:?}"
            );
        }
        other => panic!("expected ToolOutput::Error, got {other:?}"),
    }
}

#[test]
fn name_and_concurrency_safe() {
    let r = Read;
    assert_eq!(r.name(), "Read");
    assert!(r.is_concurrency_safe());
}

#[test]
fn description_returns_prompt_constant() {
    let r = Read;
    assert_eq!(r.description(), super::prompt::DESCRIPTION);
    assert!(!r.description().is_empty());
}

#[test]
fn input_schema_is_self_contained_and_valid() {
    let schema = Read.input_schema();
    // Must compile under the jsonschema crate (no $ref / external refs).
    let validator = Validator::new(&schema).expect("schema must compile");

    let good = json!({"file_path": "/tmp/x", "offset": 1, "limit": 10});
    assert!(validator.is_valid(&good));

    let missing_required = json!({"offset": 0});
    assert!(!validator.is_valid(&missing_required));

    let wrong_type = json!({"file_path": 5});
    assert!(!validator.is_valid(&wrong_type));
}

#[tokio::test]
async fn happy_path_cat_n_format_snapshot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hello.txt");
    tokio::fs::write(&path, "alpha\nbeta\ngamma\n")
        .await
        .unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    insta::assert_snapshot!("read_happy_path", text);
}

#[tokio::test]
async fn offset_and_limit_window() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nums.txt");
    let body: String = (1..=10)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": 4, "limit": 4}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    // offset=4 (1-based) → start at line 4. limit=4 → lines 4..=7.
    let expected = "\t4\tline4\n\t5\tline5\n\t6\tline6\n\t7\tline7";
    assert_eq!(text, expected);
}

#[tokio::test]
async fn offset_1based_fifty_starts_at_line_50() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hundred.txt");
    let body: String = (1..=100)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": 50}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    assert!(
        text.starts_with("\t50\tline50"),
        "offset=50 must start at line 50, got: {:?}",
        &text[..text.len().min(40)]
    );
}

#[tokio::test]
async fn device_paths_return_recoverable_error_immediately() {
    // Each blocked path must error without performing any I/O.
    let blocked = [
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/null",
        "/dev/tty",
        "/dev/fd/0",
        "/dev/fd/1",
    ];
    for path in &blocked {
        let out = Read
            .invoke(json!({"file_path": path}), &ctx())
            .await
            .unwrap();
        assert_recoverable_error(out, "device or pseudo file");
    }
}

#[tokio::test]
async fn relative_path_returns_recoverable_error() {
    let out = Read
        .invoke(json!({"file_path": "relative/path.txt"}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "absolute path");
}

#[tokio::test]
async fn missing_file_returns_recoverable_error_with_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does-not-exist.txt");
    let s = path.to_str().unwrap().to_string();

    let out = Read
        .invoke(json!({"file_path": s.clone()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(message.contains(&s), "missing-file error must name path");
            assert!(message.contains("does not exist"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_path_returns_recoverable_error_suggesting_glob() {
    let dir = TempDir::new().unwrap();
    let out = Read
        .invoke(json!({"file_path": dir.path().to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "Glob");
}

#[tokio::test]
async fn binary_file_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    // A non-media binary file (`.bin`) so the image/PDF media branch doesn't
    // claim it — the NUL byte in the first 8 KiB must trip the text/binary
    // heuristic on the normal read path.
    let path = dir.path().join("blob.bin");
    let bytes: Vec<u8> = vec![
        0x00, 0x01, 0x02, 0x03, b'n', b'o', b't', b'-', b't', b'e', b'x', b't',
    ];
    tokio::fs::write(&path, &bytes).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "binary");
}

#[tokio::test]
async fn empty_file_returns_system_reminder_text() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.txt");
    tokio::fs::write(&path, b"").await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => assert_eq!(s, EMPTY_FILE_REMINDER),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn long_line_is_truncated_with_marker() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("long.txt");

    let long: String = "a".repeat(MAX_LINE_LENGTH + 50);
    tokio::fs::write(&path, &long).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    assert!(
        text.ends_with(LINE_TRUNCATION_MARKER),
        "expected truncation marker, got tail {:?}",
        &text[text.len().saturating_sub(60)..]
    );

    // Body should be exactly: "\t1\t" + MAX_LINE_LENGTH chars + marker.
    let expected_len = "\t1\t".len() + MAX_LINE_LENGTH + LINE_TRUNCATION_MARKER.len();
    assert_eq!(text.len(), expected_len);
}

#[tokio::test]
async fn cancellation_returns_cancelled_within_100ms() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("big.txt");

    // Many short lines so the per-batch cancel poll fires.
    let mut body = String::with_capacity(200_000);
    for i in 0..20_000 {
        body.push_str(&format!("line {i}\n"));
    }
    tokio::fs::write(&path, &body).await.unwrap();

    let context = ctx();
    // Pre-cancel so the very first poll inside `invoke` returns Cancelled —
    // a deterministic version of the "mid-read cancel" check that doesn't
    // depend on scheduling.
    context.cancel.cancel();

    let path_s = path.to_str().unwrap().to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        Read.invoke(json!({"file_path": path_s}), &context),
    )
    .await
    .expect("must return within 100ms");

    match result {
        Err(AoError::Internal(msg)) => assert_eq!(msg, CANCELLED_MESSAGE),
        other => panic!("expected AoError::Internal(\"cancelled\"), got {other:?}"),
    }
}

#[tokio::test]
async fn defaults_start_line_1_and_limit_2000() {
    // Sanity: 2000-line file with no offset/limit returns all 2000 lines.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("two_thousand.txt");
    let body: String = (1..=DEFAULT_LIMIT)
        .map(|n| format!("L{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    let line_count = text.lines().count();
    assert_eq!(line_count, DEFAULT_LIMIT);
    assert!(text.starts_with("\t1\tL1\n"));
}

#[tokio::test]
async fn successful_read_populates_read_file_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.txt");
    tokio::fs::write(&path, "hello\nworld\n").await.unwrap();

    let context = ctx();
    Read.invoke(json!({"file_path": path.to_str().unwrap()}), &context)
        .await
        .unwrap();

    // Look up with the same path that was passed — Read records the key as-is.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("read_file_state must have an entry after a successful read");

    assert_eq!(entry.content, "hello\nworld\n");
    assert!(entry.is_full_read(), "no offset/limit → full read");
}

#[tokio::test]
async fn explicit_offset_and_limit_records_partial_view() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("partial.txt");
    let body: String = (1..=10).map(|n| format!("L{n}\n")).collect();
    tokio::fs::write(&path, &body).await.unwrap();

    let context = ctx();
    Read.invoke(
        json!({"file_path": path.to_str().unwrap(), "offset": 1, "limit": 2000}),
        &context,
    )
    .await
    .unwrap();

    let entry = context.read_file_state.get(&path).unwrap();
    assert!(
        entry.is_partial_view(),
        "explicit offset+limit (even at defaults) → partial view"
    );
}

#[tokio::test]
async fn no_offset_no_limit_records_full_read() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("full.txt");
    tokio::fs::write(&path, "only line\n").await.unwrap();

    let context = ctx();
    Read.invoke(json!({"file_path": path.to_str().unwrap()}), &context)
        .await
        .unwrap();

    let entry = context.read_file_state.get(&path).unwrap();
    assert!(entry.is_full_read());
    assert!(!entry.is_partial_view());
}

#[tokio::test]
async fn oversized_file_returns_recoverable_error_without_reading() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("big.bin");

    // Write MAX_FILE_SIZE_BYTES + 1 bytes of printable text so the binary
    // check would pass if we ever reached it — but we must not.
    let data = vec![b'x'; MAX_FILE_SIZE_BYTES + 1];
    tokio::fs::write(&path, &data).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    assert_recoverable_error(out, "too large");
}

#[tokio::test]
async fn file_at_size_cap_reads_successfully() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("exact.txt");

    // Exactly at the cap — must succeed (boundary is strictly >).
    let data = vec![b'a'; MAX_FILE_SIZE_BYTES];
    tokio::fs::write(&path, &data).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Text(_) => {}
        other => panic!("expected Text for file at cap, got {other:?}"),
    }
}

#[test]
fn coerce_usize_accepts_number_and_string() {
    use serde_json::json;
    // Direct number
    assert_eq!(super::coerce_usize(Some(&json!(50_u64))), Some(50));
    // String-encoded number
    assert_eq!(super::coerce_usize(Some(&json!("50"))), Some(50));
    // Trimmed whitespace
    assert_eq!(super::coerce_usize(Some(&json!(" 10 "))), Some(10));
    // Absent → None
    assert_eq!(super::coerce_usize(None), None);
    // Unparseable string → None (treated as omitted)
    assert_eq!(super::coerce_usize(Some(&json!("abc"))), None);
    // Negative string → None
    assert_eq!(super::coerce_usize(Some(&json!("-1"))), None);
}

#[tokio::test]
async fn string_offset_and_limit_behave_like_numbers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nums.txt");
    let body: String = (1..=10)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out_str = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": "4", "limit": "4"}),
            &ctx(),
        )
        .await
        .unwrap();
    let out_num = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": 4, "limit": 4}),
            &ctx(),
        )
        .await
        .unwrap();

    let text_str = match out_str {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };
    let text_num = match out_num {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    assert_eq!(text_str, text_num);
    assert_eq!(text_str, "\t4\tline4\n\t5\tline5\n\t6\tline6\n\t7\tline7");
}

#[tokio::test]
async fn string_offset_50_starts_at_line_50() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hundred.txt");
    let body: String = (1..=100)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": "50"}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    assert!(
        text.starts_with("\t50\tline50"),
        "string offset=\"50\" must start at line 50, got: {:?}",
        &text[..text.len().min(40)]
    );
}

#[tokio::test]
async fn unparseable_string_offset_treated_as_omitted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("default.txt");
    let body: String = (1..=5)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    // "abc" is unparseable — should fall back to default offset=1
    let out = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": "abc"}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    };

    assert!(
        text.starts_with("\t1\tline1"),
        "unparseable offset must fall back to line 1, got: {text:?}"
    );
}

#[tokio::test]
async fn dedup_returns_stub_on_second_identical_read() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stable.txt");
    tokio::fs::write(&path, "hello\nworld\n").await.unwrap();

    let context = ctx();
    let path_s = path.to_str().unwrap();

    // First read — returns full content.
    let out1 = Read
        .invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();
    assert!(
        matches!(out1, ToolOutput::Text(_)),
        "first read must be full content"
    );

    // Second identical read — same path, same view, mtime unchanged → stub.
    let out2 = Read
        .invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();
    match out2 {
        ToolOutput::Text(s) => assert_eq!(s, FILE_UNCHANGED_STUB, "second read must return stub"),
        other => panic!("expected stub Text, got {other:?}"),
    }
}

#[tokio::test]
async fn dedup_skips_stub_when_mtime_advances() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("changing.txt");
    tokio::fs::write(&path, "version1\n").await.unwrap();

    let context = ctx();
    let path_s = path.to_str().unwrap();

    Read.invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();

    // Advance mtime by overwriting with new content after a small sleep so
    // APFS/ext4 nanosecond timestamps advance. filetime is in scope via
    // the crate-level dependency; here we rely on write-then-stat instead.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    tokio::fs::write(&path, "version2\n").await.unwrap();

    // Second read — mtime changed → full content, not stub.
    let out = Read
        .invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert_ne!(s, FILE_UNCHANGED_STUB, "modified file must not return stub");
            assert!(s.contains("version2"), "must return updated content");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn dedup_skips_stub_when_offset_differs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("windowed.txt");
    let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
    tokio::fs::write(&path, &body).await.unwrap();

    let context = ctx();
    let path_s = path.to_str().unwrap();

    Read.invoke(json!({"file_path": path_s, "offset": 1}), &context)
        .await
        .unwrap();

    // Different offset → different view, not a match → full content.
    let out = Read
        .invoke(json!({"file_path": path_s, "offset": 5}), &context)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert_ne!(
                s, FILE_UNCHANGED_STUB,
                "different offset must not return stub"
            );
            assert!(s.starts_with("\t5\t"), "must start at line 5, got: {s:?}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn dedup_skips_stub_when_limit_differs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("limited.txt");
    let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
    tokio::fs::write(&path, &body).await.unwrap();

    let context = ctx();
    let path_s = path.to_str().unwrap();

    Read.invoke(json!({"file_path": path_s, "limit": 3}), &context)
        .await
        .unwrap();

    // Different limit → different view, not a match → full content.
    let out = Read
        .invoke(json!({"file_path": path_s, "limit": 5}), &context)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert_ne!(
                s, FILE_UNCHANGED_STUB,
                "different limit must not return stub"
            );
            assert!(s.contains("line5"), "must include content through line 5");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

/// An edit leaves behind a snapshot the model has not seen as Read output
/// (`surfaced_by_read: false`). Even when its mtime and view window match the
/// next Read exactly — which they do, because Edit records the post-write mtime
/// so a follow-up edit in the same turn doesn't trip the staleness check — the
/// re-read must surface the real (post-edit) content rather than the stub.
#[tokio::test]
async fn dedup_skips_stub_after_edit_snapshot() {
    use ao_engine_tools_core::ReadEntry;
    use std::time::SystemTime;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("edited.txt");
    tokio::fs::write(&path, "before\n").await.unwrap();

    let context = ctx();
    let path_s = path.to_str().unwrap();

    // Model reads the file once (records surfaced_by_read: true).
    Read.invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();

    // Simulate an Edit: change the bytes on disk and record a full-read snapshot
    // with the post-edit mtime but surfaced_by_read: false — exactly what the
    // Edit tool does after writing.
    tokio::fs::write(&path, "after\n").await.unwrap();
    let new_mtime = tokio::fs::metadata(&path)
        .await
        .unwrap()
        .modified()
        .unwrap_or_else(|_| SystemTime::now());
    context.read_file_state.record(
        path.clone(),
        ReadEntry {
            content: "after\n".to_string(),
            mtime: new_mtime,
            offset: None,
            limit: None,
            surfaced_by_read: false,
        },
    );

    // Re-read with the same full-file window. mtime and window both match the
    // recorded snapshot, but it was left by an edit → must surface real content.
    let out = Read
        .invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert_ne!(
                s, FILE_UNCHANGED_STUB,
                "re-read after edit must not return stub"
            );
            assert!(
                s.contains("after"),
                "re-read must surface post-edit content, got: {s:?}"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    // And a subsequent identical re-read (now surfaced_by_read: true) dedups.
    let out2 = Read
        .invoke(json!({"file_path": path_s}), &context)
        .await
        .unwrap();
    match out2 {
        ToolOutput::Text(s) => assert_eq!(s, FILE_UNCHANGED_STUB, "stable re-read must dedup"),
        other => panic!("expected stub Text, got {other:?}"),
    }
}

// ── path normalization tests ─────────────────────────────────────────────────

#[tokio::test]
async fn whitespace_around_abs_path_is_trimmed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("trimmed.txt");
    tokio::fs::write(&path, "content\n").await.unwrap();

    // Pad the absolute path with spaces on both sides.
    let padded = format!("  {}  ", path.to_str().unwrap());
    let out = Read
        .invoke(json!({"file_path": padded}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => assert!(s.contains("content")),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn tilde_expands_to_home_directory() {
    // Skip gracefully when HOME is not set (e.g. some CI environments).
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };

    // Use a path that definitely does not exist so we get "does not exist",
    // not an "absolute path" error — which proves expansion happened.
    let out = Read
        .invoke(
            json!({"file_path": "~/nonexistent_ao_read_test_xyz"}),
            &ctx(),
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, .. } => {
            assert!(
                message.contains("does not exist"),
                "tilde-expanded path should fail with 'does not exist', got: {message:?}"
            );
            // The error should mention the resolved path, not the tilde form.
            assert!(
                message.contains(&home),
                "error should contain the expanded home path, got: {message:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn tilde_only_expands_to_home() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };

    // "~" alone should expand to $HOME, which is a directory.
    let out = Read
        .invoke(json!({"file_path": "~"}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, .. } => {
            // Must mention the home path, not complain about non-absolute.
            assert!(
                !message.contains("absolute path"),
                "bare '~' should expand, not fail absolute-path check: {message:?}"
            );
            assert!(
                message.contains(&home) || message.contains("directory"),
                "error should reference expanded home or 'directory': {message:?}"
            );
        }
        other => panic!("expected Error for directory/missing, got {other:?}"),
    }
}

#[tokio::test]
async fn tilde_user_form_is_not_expanded() {
    // ~username forms must NOT be expanded — the absolute-path guard rejects them.
    let out = Read
        .invoke(json!({"file_path": "~someuser/path"}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "absolute path");
}

#[test]
fn expand_tilde_unit() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };

    assert_eq!(super::expand_tilde("~".to_string()), home);
    assert_eq!(
        super::expand_tilde("~/foo/bar".to_string()),
        format!("{home}/foo/bar")
    );
    // ~user not expanded
    assert_eq!(
        super::expand_tilde("~user/foo".to_string()),
        "~user/foo".to_string()
    );
    // absolute path unchanged
    assert_eq!(
        super::expand_tilde("/abs/path".to_string()),
        "/abs/path".to_string()
    );
}

// ── "did you mean?" hint tests ───────────────────────────────────────────────

#[test]
fn levenshtein_basic() {
    assert_eq!(super::levenshtein("", ""), 0);
    assert_eq!(super::levenshtein("abc", "abc"), 0);
    assert_eq!(super::levenshtein("redme", "readme"), 1);
    assert_eq!(super::levenshtein("foo", "bar"), 3);
    assert_eq!(super::levenshtein("kitten", "sitting"), 3);
}

#[tokio::test]
async fn missing_file_with_close_neighbor_suggests_it() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("readme.md");
    tokio::fs::write(&real, "# Readme").await.unwrap();

    // "redme.md" is one edit away from "readme.md"
    let typo = dir.path().join("redme.md");
    let out = Read
        .invoke(json!({"file_path": typo.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(message.contains("does not exist"));
            assert!(
                message.contains("readme.md"),
                "expected suggestion 'readme.md' in: {message:?}"
            );
            assert!(message.contains("did you mean"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_file_with_no_neighbors_returns_plain_message() {
    let dir = TempDir::new().unwrap();
    // Empty dir — nothing to suggest.
    let absent = dir.path().join("nope.txt");
    let out = Read
        .invoke(json!({"file_path": absent.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(message.contains("does not exist"));
            assert!(
                !message.contains("did you mean"),
                "should not suggest when no candidates: {message:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn similar_names_case_insensitive_match_scores_zero() {
    // Test similar_names directly so we avoid macOS case-insensitive FS
    // resolving "readme.md" to "README.md" before we reach the NotFound branch.
    let dir = TempDir::new().unwrap();
    // Create the real file with all-caps name.
    tokio::fs::write(dir.path().join("README.md"), "x")
        .await
        .unwrap();
    // Also create a clearly-different file so the dir is non-empty.
    tokio::fs::write(dir.path().join("CHANGELOG.md"), "y")
        .await
        .unwrap();

    // Ask similar_names for a lowercase name that differs only in case.
    let missing = dir.path().join("readme.md");
    let results = super::similar_names(&missing, 3).await;

    // Must find README.md (score 0 — case-insensitive exact match).
    assert!(
        !results.is_empty(),
        "expected at least one suggestion, got none"
    );
    let first = results[0].file_name().unwrap().to_str().unwrap();
    assert_eq!(
        first.to_lowercase(),
        "readme.md",
        "case-insensitive match must be first candidate"
    );
}

#[tokio::test]
async fn similar_names_best_match_ranks_first() {
    // Two close files: one 1-edit, one 2-edits away from the missing name.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("config.toml"), "a")
        .await
        .unwrap(); // dist 1 from "cnfig.toml"
    tokio::fs::write(dir.path().join("configs.toml"), "b")
        .await
        .unwrap(); // dist 2 from "cnfig.toml"

    let missing = dir.path().join("cnfig.toml");
    let results = super::similar_names(&missing, 3).await;

    assert!(!results.is_empty());
    let first = results[0].file_name().unwrap().to_str().unwrap();
    assert_eq!(first, "config.toml", "closer match must rank first");
}

// ── token-cap tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn token_cap_error_when_content_too_large() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("fat.txt");

    // Each line is 100 'a' chars. Formatted output per line: "\tN\t" + 100 chars.
    // ~106 chars/line × 2000 lines = ~212 000 chars → ~53 000 estimated tokens,
    // well above MAX_TOKENS (25 000). File size: ~101 KB — under the 256 KB cap.
    let line = "a".repeat(100);
    let body = (0..2000)
        .map(|_| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable, "token-cap error must be recoverable");
            assert!(
                message.contains("tokens"),
                "error must mention tokens: {message:?}"
            );
            assert!(
                message.contains("offset") && message.contains("limit"),
                "error must suggest offset/limit: {message:?}"
            );
        }
        other => panic!("expected Error for over-cap content, got {other:?}"),
    }
}

#[tokio::test]
async fn token_cap_not_triggered_for_small_slice_with_offset_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("fat_windowed.txt");

    let line = "a".repeat(100);
    let body = (0..2000)
        .map(|_| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    tokio::fs::write(&path, &body).await.unwrap();

    // Requesting only 10 lines: 10 × ~106 chars = ~1 060 chars → ~265 tokens.
    let out = Read
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "offset": 1, "limit": 10}),
            &ctx(),
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            let estimated = s.chars().count() / 4;
            assert!(
                estimated <= MAX_TOKENS,
                "small slice must stay under token cap, got ~{estimated} tokens"
            );
        }
        other => panic!("expected Text for small slice, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatches_via_registry() {
    use ao_engine_tools_core::Registry;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("r.txt");
    tokio::fs::write(&path, "hi\n").await.unwrap();

    let mut r = Registry::new();
    r.register_io(Arc::new(Read));
    let context = RunnerContext::new("s", "a")
        .unwrap()
        .with_registry(Arc::new(r));

    let tool = context.registry.lookup_io("Read").expect("registered");
    let out = tool
        .invoke(json!({"file_path": path.to_str().unwrap()}), &context)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "\t1\thi"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ─── image / PDF media reads ─────────────────────────────────────────────────

/// Minimal valid PNG (1×1 transparent pixel). Used to verify the image branch
/// returns a base64 image block carrying the exact bytes on disk.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

#[tokio::test]
async fn png_file_returns_base64_image_block() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("pixel.png");
    tokio::fs::write(&path, TINY_PNG).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            match &blocks[0] {
                ToolBlock::Image { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .expect("data must be valid base64");
                    assert_eq!(decoded, TINY_PNG, "round-tripped bytes must match the file");
                }
                other => panic!("expected Image block, got {other:?}"),
            }
        }
        other => panic!("expected Blocks, got {other:?}"),
    }
}

#[tokio::test]
async fn jpeg_extension_maps_to_image_jpeg_media_type() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("photo.JPEG"); // uppercase to exercise case-insensitivity
    tokio::fs::write(&path, b"\xFF\xD8\xFF\xE0not-a-real-jpeg")
        .await
        .unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Blocks(blocks) => match &blocks[0] {
            ToolBlock::Image { media_type, .. } => assert_eq!(media_type, "image/jpeg"),
            other => panic!("expected Image block, got {other:?}"),
        },
        other => panic!("expected Blocks, got {other:?}"),
    }
}

#[tokio::test]
async fn image_over_size_cap_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("huge.png");
    // One byte over the cap; content is irrelevant since the size check precedes
    // any decode.
    let oversized = vec![0u8; MAX_IMAGE_BYTES + 1];
    tokio::fs::write(&path, &oversized).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "too large to read as an image");
}

#[tokio::test]
async fn pdf_file_returns_document_block_with_summary_and_title() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("report.pdf");
    let pdf_bytes = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n";
    tokio::fs::write(&path, pdf_bytes).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Blocks(blocks) => {
            assert_eq!(blocks.len(), 2, "expected a text summary then the document");
            match &blocks[0] {
                ToolBlock::Text { text } => {
                    assert!(
                        text.contains("report.pdf"),
                        "summary should name the file: {text}"
                    );
                }
                other => panic!("expected leading Text summary, got {other:?}"),
            }
            match &blocks[1] {
                ToolBlock::Document {
                    media_type,
                    data,
                    title,
                } => {
                    assert_eq!(media_type, "application/pdf");
                    assert_eq!(title.as_deref(), Some("report.pdf"));
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .expect("data must be valid base64");
                    assert_eq!(decoded, pdf_bytes);
                }
                other => panic!("expected Document block, got {other:?}"),
            }
        }
        other => panic!("expected Blocks, got {other:?}"),
    }
}

#[tokio::test]
async fn pdf_over_size_cap_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("huge.pdf");
    let oversized = vec![0u8; MAX_PDF_BYTES + 1];
    tokio::fs::write(&path, &oversized).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert_recoverable_error(out, "too large to read as a PDF");
}

#[tokio::test]
async fn image_read_is_not_rejected_by_binary_sniff() {
    // A PNG contains NUL bytes that the text-path binary sniff would reject.
    // The media branch must run first, so the read succeeds as an image block.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("with_nuls.png");
    tokio::fs::write(&path, TINY_PNG).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Blocks(_)),
        "PNG must be returned as media, not rejected as binary"
    );
}

#[tokio::test]
async fn image_larger_than_text_cap_still_reads_as_image() {
    // A 300 KiB PNG exceeds MAX_FILE_SIZE_BYTES (the text-path cap) but is well
    // under MAX_IMAGE_BYTES. The media branch must bypass the text cap.
    assert!(MAX_FILE_SIZE_BYTES < MAX_IMAGE_BYTES);
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("big.png");
    let mut bytes = TINY_PNG.to_vec();
    bytes.resize(MAX_FILE_SIZE_BYTES + 50 * 1024, 0);
    tokio::fs::write(&path, &bytes).await.unwrap();

    let out = Read
        .invoke(json!({"file_path": path.to_str().unwrap()}), &ctx())
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Blocks(_)),
        "image above the text cap but below the image cap must still read"
    );
}
