//! Unit tests for the NotebookEdit tool scaffold, ipynb model, replace mode, and insert mode.

use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, Registry, RunnerContext, ToolOutput};
use filetime::{set_file_mtime, FileTime};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::{ipynb::Notebook, prompt, NotebookEdit};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Write a notebook to disk and record its content in ReadFileState.
async fn seed_notebook(ctx: &RunnerContext, path: &std::path::Path, content: &str) {
    tokio::fs::write(path, content).await.unwrap();
    let mtime = tokio::fs::metadata(path).await.unwrap().modified().unwrap();
    record_full_read(ctx, path, content, mtime);
}

// ---------------------------------------------------------------------------
// Scaffold tests
// ---------------------------------------------------------------------------

#[test]
fn description_matches_prompt_constant() {
    assert_eq!(NotebookEdit.description(), prompt::DESCRIPTION);
}

#[test]
fn register_notebook_edit_lookup_returns_some() {
    let mut registry = Registry::new();
    super::register_notebook_edit(&mut registry);
    assert!(registry.lookup_io("NotebookEdit").is_some());
}

// ---------------------------------------------------------------------------
// ipynb model tests
// ---------------------------------------------------------------------------

/// Minimal notebook fixture — no cells.
const MINIMAL_NB: &str = r#"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": []
}"#;

/// Two-cell notebook with id fields.
const TWO_CELL_NB: &str = r##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "markdown",
   "id": "cell-a",
   "metadata": {},
   "source": "# Hello"
  },
  {
   "cell_type": "code",
   "id": "cell-b",
   "metadata": {},
   "outputs": [],
   "execution_count": null,
   "source": "print('hi')"
  }
 ]
}"##;

#[test]
fn parse_minimal_notebook() {
    let nb = Notebook::parse(MINIMAL_NB.as_bytes()).expect("parse should succeed");
    assert_eq!(nb.cells().len(), 0);
}

#[test]
fn parse_two_cells_round_trip() {
    let nb = Notebook::parse(TWO_CELL_NB.as_bytes()).expect("parse should succeed");
    let serialised = nb.serialise();
    // Re-parse to verify stable round-trip (byte equality after re-parse).
    let nb2 = Notebook::parse(serialised.as_bytes()).expect("re-parse should succeed");
    assert_eq!(nb2.serialise(), serialised);
}

#[test]
fn resolve_by_numeric_index() {
    let nb = Notebook::parse(TWO_CELL_NB.as_bytes()).unwrap();
    assert_eq!(nb.resolve_cell_id("0").unwrap(), 0);
    assert_eq!(nb.resolve_cell_id("1").unwrap(), 1);
}

#[test]
fn resolve_by_cell_id_string() {
    let nb = Notebook::parse(TWO_CELL_NB.as_bytes()).unwrap();
    assert_eq!(nb.resolve_cell_id("cell-a").unwrap(), 0);
    assert_eq!(nb.resolve_cell_id("cell-b").unwrap(), 1);
}

#[test]
fn resolve_index_out_of_bounds() {
    let nb = Notebook::parse(TWO_CELL_NB.as_bytes()).unwrap();
    let err = nb.resolve_cell_id("99").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("out of bounds"), "got: {msg}");
    assert!(msg.contains("99"), "got: {msg}");
}

#[test]
fn resolve_unknown_id() {
    let nb = Notebook::parse(TWO_CELL_NB.as_bytes()).unwrap();
    let err = nb.resolve_cell_id("does-not-exist").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does-not-exist"), "got: {msg}");
}

#[test]
fn parse_utf16_bom_rejected() {
    // UTF-16 BE BOM prefix followed by arbitrary bytes.
    let bytes = [0xFE_u8, 0xFF, b'{', b'}'];
    let err = Notebook::parse(&bytes).unwrap_err();
    assert!(err.to_string().contains("UTF-16"), "got: {err}");

    // UTF-16 LE BOM.
    let bytes_le = [0xFF_u8, 0xFE, b'{', b'}'];
    let err_le = Notebook::parse(&bytes_le).unwrap_err();
    assert!(err_le.to_string().contains("UTF-16"), "got: {err_le}");
}

#[test]
fn parse_malformed_json_surfaces_serde_error() {
    let err = Notebook::parse(b"not json at all").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Failed to parse notebook JSON"), "got: {msg}");
}

#[test]
fn cells_mut_errors_when_cells_missing() {
    let raw = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{}}"#;
    let mut nb = Notebook::parse(raw.as_bytes()).unwrap();
    let err = nb.cells_mut().unwrap_err();
    assert!(
        err.to_string().contains("not an array") || err.to_string().contains("missing"),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Replace mode tests
// ---------------------------------------------------------------------------

/// Notebook with two cells for replace tests.
const REPLACE_NB: &str = r##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "code",
   "id": "code-cell",
   "metadata": {},
   "outputs": [],
   "execution_count": null,
   "source": "x = 1"
  },
  {
   "cell_type": "markdown",
   "id": "md-cell",
   "metadata": {},
   "source": "# Title"
  }
 ]
}"##;

#[tokio::test]
async fn replace_happy_path_code_cell() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, REPLACE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "code-cell",
                "new_source": "x = 42"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "updated successfully");

    // Verify file was written with new source.
    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(nb["cells"][0]["source"], json!("x = 42"));

    // Verify ReadFileState was updated.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be recorded");
    assert_eq!(entry.content, written);
    assert!(entry.is_full_read());
}

#[tokio::test]
async fn replace_with_cell_type_change_code_to_markdown() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, REPLACE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "code-cell",
                "new_source": "## New heading",
                "cell_type": "markdown"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "updated successfully");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(nb["cells"][0]["cell_type"], json!("markdown"));
    assert_eq!(nb["cells"][0]["source"], json!("## New heading"));
    // outputs key must be dropped for code→markdown.
    assert!(nb["cells"][0].get("outputs").is_none() || nb["cells"][0]["outputs"].is_null());
}

#[tokio::test]
async fn replace_with_cell_type_change_markdown_to_code() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, REPLACE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "md-cell",
                "new_source": "print('hello')",
                "cell_type": "code"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "updated successfully");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(nb["cells"][1]["cell_type"], json!("code"));
    assert_eq!(nb["cells"][1]["source"], json!("print('hello')"));
    // outputs must be inserted as empty array for markdown→code.
    assert_eq!(nb["cells"][1]["outputs"], json!([]));
}

#[tokio::test]
async fn replace_without_read_state_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    tokio::fs::write(&path, REPLACE_NB).await.unwrap();

    // No ReadFileState seeded.
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "new"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "You must Read");
}

#[tokio::test]
async fn replace_with_stale_mtime_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");

    let context = ctx();
    seed_notebook(&context, &path, REPLACE_NB).await;

    // Externally change the file with a future mtime.
    tokio::fs::write(
        &path,
        r#"{"cells":[],"nbformat":4,"nbformat_minor":5,"metadata":{}}"#,
    )
    .await
    .unwrap();
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "new"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "modified since it was last read");
}

#[tokio::test]
async fn replace_unknown_cell_id_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, REPLACE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "no-such-cell",
                "new_source": "new"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "no-such-cell");
}

#[tokio::test]
async fn replace_missing_new_source_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "replace",
                "cell_id": "0"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "replace mode requires both cell_id and new_source");
}

#[tokio::test]
async fn replace_on_non_ipynb_path_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.py",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "new"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, ".ipynb");
}

#[tokio::test]
async fn replace_on_relative_path_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "relative/path.ipynb",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "new"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "must be absolute");
}

#[tokio::test]
async fn replace_with_enoent_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/__notebook_edit_nonexistent_3f9b.ipynb",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "new"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "does not exist");
}

// ---------------------------------------------------------------------------
// Insert mode tests
// ---------------------------------------------------------------------------

/// Two-cell notebook for insert tests (cells have id fields for cell_id resolution).
const INSERT_NB: &str = r##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "markdown",
   "id": "cell-a",
   "metadata": {},
   "source": "# Intro"
  },
  {
   "cell_type": "code",
   "id": "cell-b",
   "metadata": {},
   "outputs": [],
   "execution_count": null,
   "source": "x = 1"
  }
 ]
}"##;

#[tokio::test]
async fn insert_at_end_no_cell_id_no_read_state() {
    // Sub-mode (b): end-append bypasses ReadFileState gate entirely.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    // Write file to disk but do NOT seed ReadFileState.
    tokio::fs::write(&path, INSERT_NB).await.unwrap();

    let context = ctx();
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "new_source": "appended",
                "cell_type": "markdown"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "inserted into");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
    assert_eq!(nb["cells"][2]["source"], json!("appended"));
    assert_eq!(nb["cells"][2]["cell_type"], json!("markdown"));
    // markdown cells must not have an outputs key
    assert!(nb["cells"][2].get("outputs").is_none());

    // ReadFileState should be updated after the write.
    assert!(context.read_file_state.get(&path).is_some());
}

#[tokio::test]
async fn insert_at_end_with_enoent_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/__notebook_edit_nonexistent_insert_4a7c.ipynb",
                "edit_mode": "insert",
                "new_source": "hi",
                "cell_type": "code"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "does not exist");
}

#[tokio::test]
async fn insert_before_index_with_read_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, INSERT_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_id": "1",
                "new_source": "inserted before index 1",
                "cell_type": "code"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "inserted before");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    let cells = nb["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 3);
    assert_eq!(cells[1]["source"], json!("inserted before index 1"));
    assert_eq!(cells[1]["cell_type"], json!("code"));
    assert_eq!(cells[1]["outputs"], json!([]));
    // original cell-b should have shifted to index 2
    assert_eq!(cells[2]["id"], json!("cell-b"));
}

#[tokio::test]
async fn insert_before_cell_id_string_with_read_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, INSERT_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_id": "cell-b",
                "new_source": "before cell-b",
                "cell_type": "markdown"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out.clone(), "inserted before");
    assert_success_text(out, "cell-b");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    let cells = nb["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 3);
    assert_eq!(cells[1]["source"], json!("before cell-b"));
    assert_eq!(cells[1]["cell_type"], json!("markdown"));
    assert!(cells[1].get("outputs").is_none());
    assert_eq!(cells[2]["id"], json!("cell-b"));
}

#[tokio::test]
async fn insert_before_unknown_cell_id_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, INSERT_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_id": "no-such-cell",
                "new_source": "hi",
                "cell_type": "code"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "no-such-cell");
}

#[tokio::test]
async fn insert_with_present_cell_id_without_read_state_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    tokio::fs::write(&path, INSERT_NB).await.unwrap();

    // No ReadFileState seeded — sub-mode (a) must reject.
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_id": "cell-a",
                "new_source": "hi",
                "cell_type": "code"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "You must Read");
}

#[tokio::test]
async fn insert_missing_cell_type_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "insert",
                "new_source": "hi"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "insert mode requires new_source and cell_type");
}

#[tokio::test]
async fn insert_missing_new_source_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "insert",
                "cell_type": "code"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "insert mode requires new_source and cell_type");
}

#[tokio::test]
async fn insert_code_cell_shape_assertion() {
    // Code cells must have outputs: [] and execution_count: null.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    tokio::fs::write(&path, INSERT_NB).await.unwrap();

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "new_source": "y = 2",
                "cell_type": "code"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_success_text(out, "inserted into");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    let new_cell = &nb["cells"][2];
    assert_eq!(new_cell["cell_type"], json!("code"));
    assert_eq!(new_cell["outputs"], json!([]));
    assert_eq!(new_cell["execution_count"], json!(null));
    assert_eq!(new_cell["metadata"], json!({}));
}

#[tokio::test]
async fn insert_markdown_cell_shape_assertion() {
    // Markdown cells must NOT have an outputs key.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    tokio::fs::write(&path, INSERT_NB).await.unwrap();

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "insert",
                "new_source": "## Section",
                "cell_type": "markdown"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_success_text(out, "inserted into");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    let new_cell = &nb["cells"][2];
    assert_eq!(new_cell["cell_type"], json!("markdown"));
    assert!(
        new_cell.get("outputs").is_none(),
        "markdown cell must not have outputs key"
    );
    assert_eq!(new_cell["metadata"], json!({}));
}

// ---------------------------------------------------------------------------
// Delete mode tests
// ---------------------------------------------------------------------------

/// Two-cell notebook for delete tests.
const DELETE_NB: &str = r##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "markdown",
   "id": "first-cell",
   "metadata": {},
   "source": "# First"
  },
  {
   "cell_type": "code",
   "id": "second-cell",
   "metadata": {},
   "outputs": [],
   "execution_count": null,
   "source": "x = 1"
  }
 ]
}"##;

#[tokio::test]
async fn delete_happy_path_by_index() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, DELETE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "0"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "deleted from");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    let cells = nb["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0]["id"], json!("second-cell"));

    // ReadFileState must be updated with the post-delete content.
    let entry = context
        .read_file_state
        .get(&path)
        .expect("state must be recorded");
    assert_eq!(entry.content, written);
}

#[tokio::test]
async fn delete_happy_path_by_cell_id_string() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, DELETE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "second-cell"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "second-cell");
    assert_success_text(
        NotebookEdit
            .invoke(
                json!({
                    "notebook_path": path.to_str().unwrap(),
                    "edit_mode": "delete",
                    "cell_id": "0"
                }),
                &context,
            )
            .await
            .unwrap(),
        "deleted from",
    );
    // After two deletes the file should have zero cells.
    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(nb["cells"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_without_read_state_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    tokio::fs::write(&path, DELETE_NB).await.unwrap();

    // No ReadFileState seeded — must reject.
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "0"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "You must Read");
}

#[tokio::test]
async fn delete_with_stale_mtime_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, DELETE_NB).await;

    // Externally change the file with a future mtime.
    tokio::fs::write(
        &path,
        r#"{"cells":[],"nbformat":4,"nbformat_minor":5,"metadata":{}}"#,
    )
    .await
    .unwrap();
    let future = FileTime::from_unix_time(FileTime::now().unix_seconds() + 60, 0);
    set_file_mtime(&path, future).unwrap();

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "0"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "modified since it was last read");
}

#[tokio::test]
async fn delete_unknown_cell_id_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, DELETE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "no-such-cell"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "no-such-cell");
}

#[tokio::test]
async fn delete_index_out_of_bounds_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, DELETE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "99"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "out of bounds");
}

#[tokio::test]
async fn delete_with_new_source_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "delete",
                "cell_id": "0",
                "new_source": "forbidden"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "delete mode forbids new_source and cell_type");
}

#[tokio::test]
async fn delete_with_cell_type_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "delete",
                "cell_id": "0",
                "cell_type": "code"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "delete mode forbids new_source and cell_type");
}

#[tokio::test]
async fn delete_only_cell_yields_empty_cells_array() {
    // Single-cell notebook.
    let single_cell_nb = r##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "code",
   "id": "only",
   "metadata": {},
   "outputs": [],
   "execution_count": null,
   "source": "x = 1"
  }
 ]
}"##;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.ipynb");
    let context = ctx();

    seed_notebook(&context, &path, single_cell_nb).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "delete",
                "cell_id": "0"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "deleted from");

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    let nb: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        nb["cells"].as_array().unwrap().len(),
        0,
        "cells array must be empty after deleting the only cell"
    );
}

// ---------------------------------------------------------------------------
// Cross-mode validation hardening tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_relative_path_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "relative/notebook.ipynb",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "x"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "must be absolute");
}

#[tokio::test]
async fn validate_non_ipynb_extension_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/notebook.py",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "x"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, ".ipynb");
}

#[tokio::test]
async fn validate_uppercase_extension_accepted() {
    // .IPYNB (uppercase) must be accepted — extension check is case-insensitive.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("notebook.IPYNB");
    let context = ctx();

    seed_notebook(&context, &path, REPLACE_NB).await;

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "code-cell",
                "new_source": "x = 99"
            }),
            &context,
        )
        .await
        .unwrap();

    assert_success_text(out, "updated successfully");
}

#[tokio::test]
async fn validate_enoent_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/__validate_enoent_nonexistent_a1b2.ipynb",
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "x"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "does not exist");
}

#[tokio::test]
async fn validate_size_cap_rejected() {
    // Create a sparse file just over the 100 MB cap.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("big.ipynb");

    let f = tokio::fs::File::create(&path).await.unwrap();
    f.set_len(100 * 1024 * 1024 + 1).await.unwrap();
    drop(f);

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "x"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "100 MB");
}

#[tokio::test]
async fn validate_utf16_bom_rejected_at_validate_layer() {
    // The BOM check must fire in validate_common — before the ReadFileState gate.
    // We prove this by NOT seeding ReadFileState: if the gate ran first, the
    // error would be "You must Read" rather than the BOM message.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bom.ipynb");

    // UTF-16 BE BOM followed by dummy bytes.
    let bom_bytes: &[u8] = &[0xFE, 0xFF, b'{', b'}'];
    tokio::fs::write(&path, bom_bytes).await.unwrap();

    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": path.to_str().unwrap(),
                "edit_mode": "replace",
                "cell_id": "0",
                "new_source": "x"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "UTF-16 BOM");
}

#[tokio::test]
async fn validate_input_missing_edit_mode_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb"
                // edit_mode intentionally omitted
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "Invalid input:");
}

#[tokio::test]
async fn validate_input_unknown_edit_mode_rejected() {
    let out = NotebookEdit
        .invoke(
            json!({
                "notebook_path": "/tmp/test.ipynb",
                "edit_mode": "frob"
            }),
            &ctx(),
        )
        .await
        .unwrap();

    assert_recoverable_error(out, "Invalid input:");
}
