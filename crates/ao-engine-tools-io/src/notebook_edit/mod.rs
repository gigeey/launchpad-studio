//! NotebookEdit tool — structured editing of Jupyter .ipynb notebooks.
//!
//! Supports three edit modes (replace / insert / delete) and enforces a
//! read-before-write contract via `ReadFileState`. `is_concurrency_safe()`
//! returns false so the bounded executor serialises invocations against
//! itself and the rest of the file-mutation set.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::path::expand_path;

pub mod ipynb;
pub mod prompt;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Input deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NotebookEditInput {
    notebook_path: String,
    edit_mode: EditMode,
    cell_id: Option<String>,
    new_source: Option<String>,
    cell_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum EditMode {
    Replace,
    Insert,
    Delete,
}

// ---------------------------------------------------------------------------
// Tool struct and registration
// ---------------------------------------------------------------------------

/// Edits Jupyter notebook files via replace, insert, or delete operations.
pub struct NotebookEdit;

/// Register the [`NotebookEdit`] tool into `registry`.
pub fn register_notebook_edit(registry: &mut Registry) {
    registry.register_io(Arc::new(NotebookEdit));
}

// ---------------------------------------------------------------------------
// Common validation
// ---------------------------------------------------------------------------

/// Validates path, extension, cancellation, stat (ENOENT + 100 MB cap), and
/// UTF-16 BOM. Returns the canonicalised absolute path on success.
async fn validate_common(
    input: &NotebookEditInput,
    ctx: &RunnerContext,
) -> Result<PathBuf, ToolOutput> {
    // 1. Must be absolute.
    if !std::path::Path::new(&input.notebook_path).is_absolute() {
        return Err(ToolOutput::error(
            format!(
                "notebook_path must be absolute (got: {})",
                input.notebook_path
            ),
            true,
        ));
    }

    // 2. Must end in .ipynb (case-insensitive).
    if !std::path::Path::new(&input.notebook_path)
        .extension()
        .map(|e| e.to_ascii_lowercase() == "ipynb")
        .unwrap_or(false)
    {
        return Err(ToolOutput::error(
            format!(
                "notebook_path must end in .ipynb (got: {})",
                input.notebook_path
            ),
            true,
        ));
    }

    // 3. Expand path.
    let cwd = ctx.cwd.read().unwrap().clone();
    let abs_path = expand_path(&input.notebook_path, &cwd);

    // 4. Cancellation check.
    if ctx.cancel.is_cancelled() {
        return Err(ToolOutput::error("NotebookEdit was cancelled", true));
    }

    // 5. Stat: ENOENT + 100 MB size cap.
    let metadata = match tokio::fs::metadata(&abs_path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutput::error(
                format!("File does not exist: {}", abs_path.display()),
                true,
            ));
        }
        Err(e) => {
            return Err(ToolOutput::error(
                format!("Failed to stat {}: {}", abs_path.display(), e),
                true,
            ));
        }
    };

    if metadata.len() > 100 * 1024 * 1024 {
        return Err(ToolOutput::error(
            "Notebook is larger than the 100 MB cap",
            true,
        ));
    }

    // 6. BOM check — read first 2 bytes only.
    if let Ok(mut f) = tokio::fs::File::open(&abs_path).await {
        let mut bom_buf = [0u8; 2];
        if let Ok(2) = f.read(&mut bom_buf).await {
            if (bom_buf[0] == 0xFE && bom_buf[1] == 0xFF)
                || (bom_buf[0] == 0xFF && bom_buf[1] == 0xFE)
            {
                return Err(ToolOutput::error(
                    "Notebook file is not UTF-8 (UTF-16 BOM detected)",
                    true,
                ));
            }
        }
    }

    Ok(abs_path)
}

// ---------------------------------------------------------------------------
// IoTool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl IoTool for NotebookEdit {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::from_str(prompt::INPUT_SCHEMA).expect("prompt::INPUT_SCHEMA is valid JSON")
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // Deserialize with clear field-level error messages.
        let input: NotebookEditInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid input: {e}"), true)),
        };

        match input.edit_mode {
            // ------------------------------------------------------------------
            // Replace mode
            // ------------------------------------------------------------------
            EditMode::Replace => {
                if input.cell_id.is_none() || input.new_source.is_none() {
                    return Ok(ToolOutput::error(
                        "replace mode requires both cell_id and new_source",
                        true,
                    ));
                }

                let abs_path = match validate_common(&input, ctx).await {
                    Ok(p) => p,
                    Err(out) => return Ok(out),
                };

                let current_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);

                let cell_id_str = input.cell_id.unwrap();
                let new_source_str = input.new_source.unwrap();
                let cell_type = input.cell_type;

                // ReadFileState gate.
                let entry = match ctx.read_file_state.get(&abs_path) {
                    None => {
                        return Ok(ToolOutput::error(
                            "You must Read the notebook before editing it",
                            true,
                        ))
                    }
                    Some(e) => e,
                };

                if entry.is_partial_view() {
                    return Ok(ToolOutput::error(
                        "Notebook was only partially read. Re-read the full notebook before editing.",
                        true,
                    ));
                }

                // Read file bytes for staleness check and parse.
                let bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;
                let current_content = String::from_utf8_lossy(&bytes).into_owned();
                if current_mtime > entry.mtime && current_content != entry.content {
                    return Ok(ToolOutput::error(
                        "Notebook has been modified since it was last read. Re-read before editing.",
                        true,
                    ));
                }

                // Parse notebook.
                let mut notebook = match ipynb::Notebook::parse(&bytes) {
                    Ok(nb) => nb,
                    Err(ipynb::IpynbError::ParseJson(e)) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to parse notebook JSON: {e}"),
                            true,
                        ));
                    }
                    Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                };

                // Resolve cell index.
                let idx = match notebook.resolve_cell_id(&cell_id_str) {
                    Ok(i) => i,
                    Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                };

                // Apply replace: update source, optionally mutate cell_type.
                {
                    let cells = match notebook.cells_mut() {
                        Ok(c) => c,
                        Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                    };

                    cells[idx]["source"] = json!(new_source_str);

                    if let Some(ref new_type) = cell_type {
                        let existing_type =
                            cells[idx]["cell_type"].as_str().unwrap_or("").to_string();
                        if new_type != &existing_type {
                            cells[idx]["cell_type"] = json!(new_type);
                            match (existing_type.as_str(), new_type.as_str()) {
                                ("code", "markdown") => {
                                    cells[idx].as_object_mut().unwrap().remove("outputs");
                                }
                                ("markdown", "code") => {
                                    cells[idx]["outputs"] = json!([]);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Serialise and write.
                let serialised = notebook.serialise();
                if let Err(e) = tokio::fs::write(&abs_path, serialised.as_bytes()).await {
                    return Ok(ToolOutput::error(
                        format!("Failed to write notebook: {e}"),
                        true,
                    ));
                }

                let new_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);
                ctx.read_file_state.record(
                    abs_path.clone(),
                    ReadEntry {
                        content: serialised,
                        mtime: new_mtime,
                        offset: None,
                        limit: None,
                        surfaced_by_read: false,
                    },
                );

                Ok(ToolOutput::text(format!(
                    "Cell {} in {} updated successfully.",
                    cell_id_str,
                    abs_path.display()
                )))
            }

            // ------------------------------------------------------------------
            // Insert mode
            // ------------------------------------------------------------------
            EditMode::Insert => {
                if input.new_source.is_none() || input.cell_type.is_none() {
                    return Ok(ToolOutput::error(
                        "insert mode requires new_source and cell_type",
                        true,
                    ));
                }

                let abs_path = match validate_common(&input, ctx).await {
                    Ok(p) => p,
                    Err(out) => return Ok(out),
                };

                let current_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);

                self.invoke_insert(
                    abs_path,
                    input.cell_id,
                    input.new_source.unwrap(),
                    input.cell_type.unwrap(),
                    current_mtime,
                    ctx,
                )
                .await
            }

            // ------------------------------------------------------------------
            // Delete mode
            // ------------------------------------------------------------------
            EditMode::Delete => {
                if input.cell_id.is_none() {
                    return Ok(ToolOutput::error("delete mode requires cell_id", true));
                }
                if input.new_source.is_some() || input.cell_type.is_some() {
                    return Ok(ToolOutput::error(
                        "delete mode forbids new_source and cell_type",
                        true,
                    ));
                }

                let abs_path = match validate_common(&input, ctx).await {
                    Ok(p) => p,
                    Err(out) => return Ok(out),
                };

                let current_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);

                self.invoke_delete(abs_path, input.cell_id.unwrap(), current_mtime, ctx)
                    .await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mode helpers
// ---------------------------------------------------------------------------

impl NotebookEdit {
    /// Handles the insert edit mode — inserts a new cell before the given
    /// `cell_id` (sub-mode a) or appends it to the end (sub-mode b when
    /// `cell_id` is `None`). The ReadFileState gate is only enforced for
    /// sub-mode a. BOM is already verified by `validate_common`.
    async fn invoke_insert(
        &self,
        abs_path: PathBuf,
        cell_id: Option<String>,
        new_source: String,
        cell_type: String,
        current_mtime: SystemTime,
        ctx: &RunnerContext,
    ) -> Result<ToolOutput, AoError> {
        let new_cell = if cell_type == "code" {
            json!({
                "cell_type": "code",
                "source": new_source,
                "metadata": {},
                "outputs": [],
                "execution_count": null
            })
        } else {
            json!({
                "cell_type": "markdown",
                "source": new_source,
                "metadata": {}
            })
        };

        match cell_id {
            Some(cell_id_str) => {
                // Sub-mode (a): insert before cell_id — full ReadFileState gate.
                let entry = match ctx.read_file_state.get(&abs_path) {
                    None => {
                        return Ok(ToolOutput::error(
                            "You must Read the notebook before editing it",
                            true,
                        ))
                    }
                    Some(e) => e,
                };

                if entry.is_partial_view() {
                    return Ok(ToolOutput::error(
                        "Notebook was only partially read. Re-read the full notebook before editing.",
                        true,
                    ));
                }

                let bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;

                let current_content = String::from_utf8_lossy(&bytes).into_owned();
                if current_mtime > entry.mtime && current_content != entry.content {
                    return Ok(ToolOutput::error(
                        "Notebook has been modified since it was last read. Re-read before editing.",
                        true,
                    ));
                }

                let mut notebook = match ipynb::Notebook::parse(&bytes) {
                    Ok(nb) => nb,
                    Err(ipynb::IpynbError::ParseJson(e)) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to parse notebook JSON: {e}"),
                            true,
                        ));
                    }
                    Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                };

                let idx = match notebook.resolve_cell_id(&cell_id_str) {
                    Ok(i) => i,
                    Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                };

                {
                    let cells = match notebook.cells_mut() {
                        Ok(c) => c,
                        Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                    };
                    cells.insert(idx, new_cell);
                }

                let serialised = notebook.serialise();
                if let Err(e) = tokio::fs::write(&abs_path, serialised.as_bytes()).await {
                    return Ok(ToolOutput::error(
                        format!("Failed to write notebook: {e}"),
                        true,
                    ));
                }

                let new_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);
                ctx.read_file_state.record(
                    abs_path.clone(),
                    ReadEntry {
                        content: serialised,
                        mtime: new_mtime,
                        offset: None,
                        limit: None,
                        surfaced_by_read: false,
                    },
                );

                Ok(ToolOutput::text(format!(
                    "Cell inserted before {} in {} successfully.",
                    cell_id_str,
                    abs_path.display()
                )))
            }
            None => {
                // Sub-mode (b): end-append — ReadFileState gate is bypassed.
                let bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;

                let mut notebook = match ipynb::Notebook::parse(&bytes) {
                    Ok(nb) => nb,
                    Err(ipynb::IpynbError::ParseJson(e)) => {
                        return Ok(ToolOutput::error(
                            format!("Failed to parse notebook JSON: {e}"),
                            true,
                        ));
                    }
                    Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                };

                {
                    let cells = match notebook.cells_mut() {
                        Ok(c) => c,
                        Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
                    };
                    cells.push(new_cell);
                }

                let serialised = notebook.serialise();
                if let Err(e) = tokio::fs::write(&abs_path, serialised.as_bytes()).await {
                    return Ok(ToolOutput::error(
                        format!("Failed to write notebook: {e}"),
                        true,
                    ));
                }

                let new_mtime = tokio::fs::metadata(&abs_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now);
                ctx.read_file_state.record(
                    abs_path.clone(),
                    ReadEntry {
                        content: serialised,
                        mtime: new_mtime,
                        offset: None,
                        limit: None,
                        surfaced_by_read: false,
                    },
                );

                Ok(ToolOutput::text(format!(
                    "Cell inserted into {} successfully.",
                    abs_path.display()
                )))
            }
        }
    }

    /// Handles the delete edit mode — removes the cell resolved by `cell_id`.
    /// The ReadFileState gate is mandatory (no bypass).
    /// BOM is already verified by `validate_common`.
    async fn invoke_delete(
        &self,
        abs_path: PathBuf,
        cell_id_str: String,
        current_mtime: SystemTime,
        ctx: &RunnerContext,
    ) -> Result<ToolOutput, AoError> {
        // ReadFileState gate — mandatory, no bypass.
        let entry = match ctx.read_file_state.get(&abs_path) {
            None => {
                return Ok(ToolOutput::error(
                    "You must Read the notebook before editing it",
                    true,
                ))
            }
            Some(e) => e,
        };

        if entry.is_partial_view() {
            return Ok(ToolOutput::error(
                "Notebook was only partially read. Re-read the full notebook before editing.",
                true,
            ));
        }

        let bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;

        // Staleness check.
        let current_content = String::from_utf8_lossy(&bytes).into_owned();
        if current_mtime > entry.mtime && current_content != entry.content {
            return Ok(ToolOutput::error(
                "Notebook has been modified since it was last read. Re-read before editing.",
                true,
            ));
        }

        // Parse notebook.
        let mut notebook = match ipynb::Notebook::parse(&bytes) {
            Ok(nb) => nb,
            Err(ipynb::IpynbError::ParseJson(e)) => {
                return Ok(ToolOutput::error(
                    format!("Failed to parse notebook JSON: {e}"),
                    true,
                ));
            }
            Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
        };

        // Resolve cell index.
        let idx = match notebook.resolve_cell_id(&cell_id_str) {
            Ok(i) => i,
            Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
        };

        // Remove the cell.
        {
            let cells = match notebook.cells_mut() {
                Ok(c) => c,
                Err(e) => return Ok(ToolOutput::error(e.to_string(), true)),
            };
            cells.remove(idx);
        }

        // Serialise and write.
        let serialised = notebook.serialise();
        if let Err(e) = tokio::fs::write(&abs_path, serialised.as_bytes()).await {
            return Ok(ToolOutput::error(
                format!("Failed to write notebook: {e}"),
                true,
            ));
        }

        // Update ReadFileState with post-delete content and fresh mtime.
        let new_mtime = tokio::fs::metadata(&abs_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or_else(SystemTime::now);
        ctx.read_file_state.record(
            abs_path.clone(),
            ReadEntry {
                content: serialised,
                mtime: new_mtime,
                offset: None,
                limit: None,
                surfaced_by_read: false,
            },
        );

        Ok(ToolOutput::text(format!(
            "Cell {} deleted from {} successfully.",
            cell_id_str,
            abs_path.display()
        )))
    }
}
