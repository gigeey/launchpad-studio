//! Unit tests for the Write tool.

use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, RunnerContext, ToolOutput};
use filetime::{set_file_mtime, FileTime};
use jsonschema::Validator;
use serde_json::{json, Value};
use tempfile::TempDir;

use super::Write;
use crate::edit::MAX_EDIT_FILE_SIZE;

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

fn assert_success_text(out: ToolOutput, contains: &str) {
    match out {
        ToolOutput::Text(s) => assert!(
            s.contains(contains),
            "success message {s:?} did not contain {contains:?}"
        ),
        other => panic!("expected ToolOutput::Text, got {other:?}"),
    }
}

fn record_full_read(ctx: &RunnerContext, path: &std::path::Path, content: &str, mtime: SystemTime) {
    ctx.read_file_state.record(
        path.to_path_buf(),
        ReadEntry {
            content: content.to_string(),
            mtime,
            offset: None,
            limit: None,
            surfaced_by_read: true,
        },
    );
}

// ── Metadata / schema ────────────────────────────────────────────────────────

#[test]
fn name_and_concurrency_not_safe() {
    assert_eq!(Write.name(), "Write");
    assert!(!Write.is_concurrency_safe());
}

#[test]
fn description_matches_prompt_constant() {
    assert_eq!(Write.description(), super::prompt::DESCRIPTION);
    assert!(!Write.description().is_empty());
}

#[test]
fn input_schema_matches_prompt_constant() {
    let expected: Value = serde_json::from_str(super::prompt::INPUT_SCHEMA).unwrap();
    assert_eq!(Write.input_schema(), expected);
}

#[test]
fn input_schema_is_valid_json_schema() {
    let schema = Write.input_schema();
    let validator = Validator::new(&schema).expect("schema must compile");

    let good = json!({"file_path": "/tmp/f.txt", "content": "hello"});
    assert!(validator.is_valid(&good));

    let missing_content = json!({"file_path": "/tmp/f.txt"});
    assert!(!validator.is_valid(&missing_content));

    let missing_path = json!({"content": "hello"});
    assert!(!validator.is_valid(&missing_path));
}

// ── Input validation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn non_absolute_path_returns_recoverable_error() {
    let out = Write
        .invoke(
            json!({"file_path": "relative/path.txt", "content": "hello"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "absolute path");
}

#[tokio::test]
async fn ipynb_path_returns_recoverable_redirect_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("notebook.ipynb");

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "{}"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "Jupyter notebooks");
}

#[tokio::test]
async fn size_cap_exceeded_on_existing_file_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.bin");
    // Sparse file — no real disk allocation; triggers the stat-based cap before any read.
    let f = tokio::fs::File::create(&path).await.unwrap();
    f.set_len(MAX_EDIT_FILE_SIZE + 1).await.unwrap();
    drop(f);

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "replacement"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "too large to write");
}

// ── Read-before-write gate ────────────────────────────────────────────────────

#[tokio::test]
async fn write_without_prior_read_returns_not_read_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unread.txt");
    tokio::fs::write(&path, "existing content\n").await.unwrap();

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "new content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "has not been read yet");
}

#[tokio::test]
async fn write_after_partial_read_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("partial.txt");
    tokio::fs::write(&path, "line 1\nline 2\nline 3\n")
        .await
        .unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    // Record a partial-view entry (offset set → is_partial_view() true).
    context.read_file_state.record(
        path.clone(),
        ReadEntry {
            content: "line 1\nline 2\nline 3\n".to_string(),
            mtime: meta.modified().unwrap(),
            offset: Some(0),
            limit: None,
            surfaced_by_read: true,
        },
    );

    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "new content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "partially read");
}

// ── Staleness gate ────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_check_blocks_write_when_mtime_and_content_changed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stale.txt");
    tokio::fs::write(&path, "original content\n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(
        &context,
        &path,
        "original content\n",
        meta.modified().unwrap(),
    );

    // Simulate external modification: different content + advanced mtime.
    tokio::fs::write(&path, "externally changed\n")
        .await
        .unwrap();
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "my replacement\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "modified since it was last read");
}

#[tokio::test]
async fn staleness_fallthrough_allows_write_when_mtime_advanced_but_content_same() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("untouched.txt");
    tokio::fs::write(&path, "hello world\n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "hello world\n", meta.modified().unwrap());

    // Advance mtime without changing content (cloud sync / antivirus touch).
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "hello tools\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated successfully");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "hello tools\n");
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_new_file_in_existing_directory() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("new_file.txt");

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "created content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "created content\n");

    // read_file_state must be recorded with full-read shape.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be recorded after create");
    assert_eq!(entry.content, "created content\n");
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn overwrite_existing_file_after_read() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overwrite_me.txt");
    let original = "original content\n";
    tokio::fs::write(&path, original).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, original, meta.modified().unwrap());

    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "completely new content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated successfully");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "completely new content\n");

    // read_file_state must reflect the post-write content.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be refreshed after write");
    assert_eq!(entry.content, "completely new content\n");
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn create_file_with_nested_missing_parent_dirs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a").join("b").join("c.txt");

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": "nested content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "nested content\n");
}

#[tokio::test]
async fn create_file_with_empty_content() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.txt");

    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": ""}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    let bytes = tokio::fs::read(&path).await.unwrap();
    assert!(bytes.is_empty(), "file must be zero bytes");

    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be recorded");
    assert_eq!(entry.content, "");
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn overwrite_existing_file_with_empty_content() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("truncate_me.txt");
    tokio::fs::write(&path, "some data\n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "some data\n", meta.modified().unwrap());

    let out = Write
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "content": ""}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated successfully");

    let bytes = tokio::fs::read(&path).await.unwrap();
    assert!(bytes.is_empty(), "file must be truncated to zero bytes");
}

#[tokio::test]
async fn create_file_at_symlinked_parent_dir() {
    let real_dir = TempDir::new().unwrap();
    let link_dir = TempDir::new().unwrap();
    let link_path = link_dir.path().join("link_to_real");

    // Create a symlink pointing to real_dir.
    #[cfg(unix)]
    std::os::unix::fs::symlink(real_dir.path(), &link_path).unwrap();
    #[cfg(not(unix))]
    {
        // Skip on non-Unix (symlinks require elevated perms on Windows).
        return;
    }

    let file_path = link_path.join("through_symlink.txt");
    let context = ctx();
    let out = Write
        .invoke(
            json!({"file_path": file_path.to_str().unwrap(), "content": "via symlink\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    // Verify the file was written to the real directory target.
    let real_file = real_dir.path().join("through_symlink.txt");
    let content = tokio::fs::read_to_string(&real_file).await.unwrap();
    assert_eq!(content, "via symlink\n");
}
