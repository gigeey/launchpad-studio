//! Unit tests for the Edit tool.
//!
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` —
//! private items (helpers, constants) from `mod.rs` are in scope here.

use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, RunnerContext, ToolOutput};
use filetime::{set_file_mtime, FileTime};
use jsonschema::Validator;
use serde_json::{json, Value};
use tempfile::TempDir;

use super::{decode_content, Edit, MAX_EDIT_FILE_SIZE};

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
    assert_eq!(Edit.name(), "Edit");
    assert!(!Edit.is_concurrency_safe());
}

#[test]
fn description_matches_prompt_constant() {
    assert_eq!(Edit.description(), super::prompt::DESCRIPTION);
    assert!(!Edit.description().is_empty());
}

#[test]
fn input_schema_matches_prompt_constant() {
    let expected: Value = serde_json::from_str(super::prompt::INPUT_SCHEMA).unwrap();
    assert_eq!(Edit.input_schema(), expected);
}

#[test]
fn input_schema_is_valid_json_schema() {
    let schema = Edit.input_schema();
    let validator = Validator::new(&schema).expect("schema must compile");

    let good = json!({"file_path": "/tmp/f.txt", "old_string": "a", "new_string": "b"});
    assert!(validator.is_valid(&good));

    let with_replace_all = json!({"file_path": "/tmp/f.txt", "old_string": "a", "new_string": "b", "replace_all": true});
    assert!(validator.is_valid(&with_replace_all));

    let missing_new = json!({"file_path": "/tmp/f.txt", "old_string": "a"});
    assert!(!validator.is_valid(&missing_new));
}

// ── Input validation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn non_absolute_path_returns_recoverable_error() {
    let out = Edit
        .invoke(
            json!({"file_path": "relative/path.txt", "old_string": "foo", "new_string": "bar"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "absolute path");
}

#[tokio::test]
async fn same_string_returns_no_changes_error() {
    let out = Edit
        .invoke(
            json!({"file_path": "/tmp/any.txt", "old_string": "same", "new_string": "same"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "identical");
}

#[tokio::test]
async fn enoent_with_non_empty_old_string_returns_error() {
    let out = Edit
        .invoke(
            json!({"file_path": "/tmp/__edit_test_nonexistent_4f2a.txt", "old_string": "foo", "new_string": "bar"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "does not exist");
}

#[tokio::test]
async fn empty_old_string_enoent_creates_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("new_file.txt");

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "", "new_string": "created content"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "created content");

    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be recorded after create");
    assert_eq!(entry.content, "created content");
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn empty_old_string_enoent_with_missing_parent_dir_creates_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a").join("b").join("nested.txt");

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "", "new_string": "nested content"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "File created successfully at:");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "nested content");
}

#[tokio::test]
async fn empty_old_string_existing_file_returns_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("existing.txt");
    tokio::fs::write(&path, "existing content").await.unwrap();

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "", "new_string": "new content"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "Cannot create file: file already exists");
}

#[test]
fn max_edit_file_size_constant_is_one_gib() {
    assert_eq!(MAX_EDIT_FILE_SIZE, 1u64 << 30);
}

#[tokio::test]
async fn size_cap_exceeded_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.bin");
    // Use a sparse file (no actual disk allocation) to trigger the stat-based cap check.
    // The read never happens because the cap check fires first.
    let f = tokio::fs::File::create(&path).await.unwrap();
    f.set_len(MAX_EDIT_FILE_SIZE + 1).await.unwrap();
    drop(f);

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "foo", "new_string": "bar"}),
            &context,
        )
        .await
        .unwrap();
    // Message must include "too large" and the actual byte size.
    assert_recoverable_error(out, "too large to edit");
}

// ── Read-before-write gate ────────────────────────────────────────────────────

#[tokio::test]
async fn edit_without_prior_read_returns_not_read_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unread.txt");
    tokio::fs::write(&path, "some content\n").await.unwrap();

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "some", "new_string": "other"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "has not been read yet");
}

#[tokio::test]
async fn edit_after_partial_read_returns_recoverable_error() {
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

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "line 1", "new_string": "line A"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "partially read");
}

// ── Staleness gate ────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_check_blocks_edit_when_mtime_and_content_changed() {
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

    // External modification: different content + mtime in the future.
    tokio::fs::write(&path, "externally changed\n")
        .await
        .unwrap();
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "original", "new_string": "modified"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "modified since it was last read");
}

#[tokio::test]
async fn staleness_fallthrough_allows_edit_when_mtime_advanced_but_content_same() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("untouched.txt");
    tokio::fs::write(&path, "hello world\n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "hello world\n", meta.modified().unwrap());

    // Advance mtime without changing content (cloud sync / antivirus touch).
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "world", "new_string": "tools"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "hello tools\n");
}

// ── String matching ───────────────────────────────────────────────────────────

#[tokio::test]
async fn string_not_found_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nope.txt");
    tokio::fs::write(&path, "hello world\n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "hello world\n", meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "nonexistent", "new_string": "x"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "not found in file");
}

#[tokio::test]
async fn multi_match_without_replace_all_returns_count_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("multi.txt");
    let content = "foo bar foo baz foo\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "foo", "new_string": "qux"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "Found 3 matches of the string. Either include more surrounding context to make old_string unique, or pass replace_all: true to replace all occurrences.");
}

#[tokio::test]
async fn replace_all_true_with_three_occurrences_replaces_all_and_appends_suffix() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("multi_replace.txt");
    let content = "foo bar foo baz foo\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "foo", "new_string": "qux", "replace_all": true}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "(all occurrences replaced)");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "qux bar qux baz qux\n");
}

#[tokio::test]
async fn replace_all_true_with_zero_occurrences_returns_not_found_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("zero_occurrences.txt");
    let content = "hello world\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "nonexistent", "new_string": "x", "replace_all": true}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "not found in file");
}

#[tokio::test]
async fn ipynb_path_returns_recoverable_redirect_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("notebook.ipynb");
    // Write a minimal stub so stat succeeds.
    tokio::fs::write(&path, b"{}").await.unwrap();

    let context = ctx();
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "foo", "new_string": "bar"}),
            &context,
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "Jupyter notebooks");
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_edits_unique_string_and_refreshes_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("edit_me.txt");
    let original = "hello world\n";
    tokio::fs::write(&path, original).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, original, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "world", "new_string": "tools"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "hello tools\n");

    // read_file_state must reflect the post-edit content.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be refreshed after edit");
    assert_eq!(entry.content, "hello tools\n");
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn replace_all_replaces_every_occurrence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("all.txt");
    let content = "cat cat cat\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "cat", "new_string": "dog", "replace_all": true}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "(all occurrences replaced)");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "dog dog dog\n");
}

#[tokio::test]
async fn replace_first_occurrence_only_when_replace_all_false() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("first.txt");
    let content = "aaa bbb aaa\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    // Wrap old_string in enough context to make it unique — use full first token.
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "aaa bbb", "new_string": "zzz bbb"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(new_content, "zzz bbb aaa\n");
}

// ── Quote normalisation ───────────────────────────────────────────────────────

#[tokio::test]
async fn curly_apostrophe_file_straight_needle_edit_succeeds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("prose.txt");
    // File contains a curly apostrophe (U+2019).
    let original = "I don\u{2019}t know what to do.\n";
    tokio::fs::write(&path, original).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, original, meta.modified().unwrap());

    // Model emits straight apostrophe in old_string.
    let out = Edit
        .invoke(
            json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "don't know",
                "new_string": "will not"
            }),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let new_content = tokio::fs::read_to_string(&path).await.unwrap();
    // Surrounding text must be unmodified; "will not" has no apostrophe so no
    // quote-style regression is possible.
    assert_eq!(new_content, "I will not what to do.\n");
}

// ── CRLF round-trip ───────────────────────────────────────────────────────────

#[tokio::test]
async fn crlf_file_round_trip_edit_succeeds_and_preserves_endings() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("windows.txt");
    // File on disk has CRLF line endings (as a Windows-committed file would).
    let crlf_bytes = b"hello\r\nworld\r\n";
    tokio::fs::write(&path, crlf_bytes).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    // The Read tool stores raw bytes as a string (including \r\n).
    record_full_read(
        &context,
        &path,
        "hello\r\nworld\r\n",
        meta.modified().unwrap(),
    );

    // Model derives old_string from Read's LF-normalised display output.
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "world", "new_string": "tools"}),
            &context,
        )
        .await
        .unwrap();

    // (i) Must not trip the staleness gate.
    // (ii) Match must succeed against the LF old_string.
    assert_success_text(out, "has been updated");

    // (iii) On-disk bytes must still contain CRLF.
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_eq!(on_disk, b"hello\r\ntools\r\n");
}

#[tokio::test]
async fn lf_file_never_gains_crlf_after_edit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unix.txt");
    let content = "alpha\nbeta\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "alpha", "new_string": "gamma"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(on_disk, "gamma\nbeta\n");
    assert!(
        !on_disk.contains('\r'),
        "LF file must not gain CR after edit"
    );
}

// ── Deletion cleanup ──────────────────────────────────────────────────────────

#[tokio::test]
async fn deletion_absorbs_trailing_newline_leaving_no_blank_line() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("del.txt");
    let content = "first line\nmiddle\nlast line\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "middle", "new_string": ""}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "first line\nlast line\n");
    assert!(
        !result.contains("\n\n"),
        "no dangling blank line after deletion"
    );
}

#[tokio::test]
async fn deletion_replace_all_absorbs_trailing_newlines() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("del_all.txt");
    let content = "keep\nremove\nkeep\nremove\nend\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "remove", "new_string": "", "replace_all": true}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "all occurrences replaced");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "keep\nkeep\nend\n");
}

// ── XML-token desanitization fallback ─────────────────────────────────────────

#[tokio::test]
async fn sanitized_old_string_matches_real_file_content() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("xml_tags.txt");
    // File contains the full XML tag form that the API sanitizes.
    let content = "prefix <function_results> suffix\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    // Model emits the abbreviated stand-in because the API sanitized the original.
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "prefix <fnr> suffix", "new_string": "replaced"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "replaced\n");
}

// ── Trailing-whitespace stripping ─────────────────────────────────────────────

#[tokio::test]
async fn trailing_whitespace_stripped_in_replacement() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spaces.rs");
    let content = "fn old() {}\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    // new_string has trailing spaces on some lines.
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "fn old() {}", "new_string": "fn new() {   \n    let x = 1;  \n}"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        !result.contains("   \n"),
        "trailing spaces must be stripped"
    );
    assert!(!result.contains("  \n"), "trailing spaces must be stripped");
    // The multi-line replacement: lines are preserved but trailing spaces gone.
    assert!(result.contains("fn new() {"), "function opening preserved");
    assert!(result.contains("let x = 1;"), "body preserved");
}

#[tokio::test]
async fn trailing_whitespace_not_stripped_for_md_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("doc.md");
    let content = "# Title\n\nold paragraph\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    // Markdown uses two trailing spaces as a hard line break — must be preserved.
    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "old paragraph", "new_string": "line one  \nline two"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        result.contains("line one  \n"),
        "trailing spaces must be preserved in .md"
    );
}

// ── replace_all boolean coercion ──────────────────────────────────────────────

#[test]
fn coerce_bool_handles_all_accepted_forms() {
    use serde_json::json;
    // Real booleans
    assert!(super::coerce_bool(Some(&json!(true))));
    assert!(!super::coerce_bool(Some(&json!(false))));
    // String forms
    assert!(super::coerce_bool(Some(&json!("true"))));
    assert!(super::coerce_bool(Some(&json!("TRUE"))));
    assert!(super::coerce_bool(Some(&json!("1"))));
    assert!(!super::coerce_bool(Some(&json!("false"))));
    assert!(!super::coerce_bool(Some(&json!("FALSE"))));
    assert!(!super::coerce_bool(Some(&json!("0"))));
    // Numeric forms
    assert!(super::coerce_bool(Some(&json!(1))));
    assert!(!super::coerce_bool(Some(&json!(0))));
    // Absent / unknown
    assert!(!super::coerce_bool(None));
    assert!(!super::coerce_bool(Some(&json!(null))));
}

#[tokio::test]
async fn replace_all_as_string_true_replaces_all_occurrences() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("coerce.txt");
    let content = "cat cat cat\n";
    tokio::fs::write(&path, content).await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, content, meta.modified().unwrap());

    let out = Edit
        .invoke(
            // Provider sends replace_all as a string "true" instead of boolean true.
            json!({"file_path": path.to_str().unwrap(), "old_string": "cat", "new_string": "dog", "replace_all": "true"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "all occurrences replaced");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "dog dog dog\n");
}

// ── Empty old_string on empty/whitespace-only file ────────────────────────────

#[tokio::test]
async fn empty_old_string_on_truly_empty_file_allowed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty_file.txt");
    tokio::fs::write(&path, b"").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "", meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "", "new_string": "new content"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "new content");
}

#[tokio::test]
async fn empty_old_string_on_whitespace_only_file_allowed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("blank.txt");
    tokio::fs::write(&path, b"   \n  \n").await.unwrap();

    let context = ctx();
    let meta = tokio::fs::metadata(&path).await.unwrap();
    record_full_read(&context, &path, "   \n  \n", meta.modified().unwrap());

    let out = Edit
        .invoke(
            json!({"file_path": path.to_str().unwrap(), "old_string": "", "new_string": "real content\n"}),
            &context,
        )
        .await
        .unwrap();
    assert_success_text(out, "has been updated");

    let result = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(result, "real content\n");
}

// ── Decode helper ─────────────────────────────────────────────────────────────

#[test]
fn decode_content_passes_utf8_through() {
    let bytes = b"hello world";
    assert_eq!(decode_content(bytes), "hello world");
}

#[test]
fn decode_content_handles_utf16_le_bom() {
    // Encode "hi" as UTF-16-LE with BOM.
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // BOM
    for ch in "hi".encode_utf16() {
        bytes.extend_from_slice(&ch.to_le_bytes());
    }
    assert_eq!(decode_content(&bytes), "hi");
}

#[test]
fn decode_content_no_bom_uses_utf8_lossy() {
    let bytes = b"plain ascii";
    assert_eq!(decode_content(bytes), "plain ascii");
}
