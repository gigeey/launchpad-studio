//! Write tool — create or overwrite a file with full content.
//!
//! Refuses to overwrite files that were not read first (via the `Read` tool)
//! or that have changed on disk since the read. `is_concurrency_safe()` returns
//! false so the bounded executor serialises all Edit/Write calls for a given
//! turn, eliminating the staleness-vs-write race.

use std::sync::Arc;
use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

use crate::edit::MAX_EDIT_FILE_SIZE;
use crate::path::expand_path;

pub mod prompt;

#[cfg(test)]
mod tests;

/// Creates or overwrites a file with the full supplied content.
///
/// Write-side IO tool. `is_concurrency_safe()` is false — concurrent
/// writes to the same file would race; the bounded executor serialises them.
///
/// Write-through symlinks: if `file_path` resolves through a symlink,
/// `tokio::fs::write` writes to the symlink target (the standard Unix behaviour).
pub struct Write;

/// Register the [`Write`] tool into `registry`.
pub fn register_write(registry: &mut Registry) {
    registry.register_io(Arc::new(Write));
}

#[async_trait]
impl IoTool for Write {
    fn name(&self) -> &str {
        "Write"
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
        // 1. Deserialize required input fields.
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "file_path is required and must be a string",
                    true,
                ));
            }
        };
        let content = match input.get("content").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "content is required and must be a string",
                    true,
                ));
            }
        };

        // 2. file_path must be absolute.
        if !std::path::Path::new(&file_path).is_absolute() {
            return Ok(ToolOutput::error(
                format!(
                    "file_path must be an absolute path, got relative path: {}",
                    file_path
                ),
                true,
            ));
        }

        // 3. Expand path (~ expansion; absolute paths pass through unchanged).
        let cwd = ctx.cwd.read().unwrap().clone();
        let abs_path = expand_path(&file_path, &cwd);

        // 4. Cancellation check.
        if ctx.cancel.is_cancelled() {
            return Err(AoError::Internal("cancelled".to_string()));
        }

        // 5. .ipynb redirect — refuse before any file I/O.
        if abs_path
            .extension()
            .map(|e| e.to_ascii_lowercase() == "ipynb")
            .unwrap_or(false)
        {
            return Ok(ToolOutput::error(
                "Writing Jupyter notebooks via Write is not supported. The NotebookEdit tool will land in a follow-up bucket.",
                true,
            ));
        }

        // 6. Stat the file.
        let metadata = match tokio::fs::metadata(&abs_path).await {
            Ok(m) => Some(m),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to stat {}: {}", abs_path.display(), err),
                    true,
                ));
            }
        };

        if let Some(ref meta) = metadata {
            // Existing-file branch (update).

            // 7. Size cap: refuse before reading the file into memory.
            if meta.len() > MAX_EDIT_FILE_SIZE {
                return Ok(ToolOutput::error(
                    format!(
                        "File is too large to write (size: {} bytes; max: {} bytes). Use a different tool or split the file.",
                        meta.len(),
                        MAX_EDIT_FILE_SIZE
                    ),
                    true,
                ));
            }

            let current_mtime = meta.modified().unwrap_or_else(|_| SystemTime::now());

            // Read-before-write check.
            let entry = match ctx.read_file_state.get(&abs_path) {
                None => {
                    return Ok(ToolOutput::error(
                        "File has not been read yet. Use the Read tool first.",
                        true,
                    ));
                }
                Some(e) => e,
            };

            if entry.is_partial_view() {
                return Ok(ToolOutput::error(
                    "File was only partially read. Re-read the full file before writing.",
                    true,
                ));
            }

            // Staleness check: mtime advanced AND content diverged from snapshot.
            // Fall-through when mtime advanced but bytes identical (cloud sync, antivirus).
            if current_mtime > entry.mtime {
                let on_disk_bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;
                let on_disk_content = String::from_utf8_lossy(&on_disk_bytes).into_owned();
                if on_disk_content != entry.content {
                    return Ok(ToolOutput::error(
                        "File has been modified since it was last read. Re-read before writing.",
                        true,
                    ));
                }
            }

            // 8. Write content verbatim — no LF/CRLF rewriting.
            tokio::fs::write(&abs_path, content.as_bytes())
                .await
                .map_err(AoError::from)?;

            // Refresh read-state so a follow-up write/edit in the same turn
            // does not trip the staleness check against the now-stale snapshot.
            let new_mtime = tokio::fs::metadata(&abs_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or_else(SystemTime::now);
            ctx.read_file_state.record(
                abs_path.clone(),
                ReadEntry {
                    content: content.clone(),
                    mtime: new_mtime,
                    offset: None,
                    limit: None,
                    surfaced_by_read: false,
                },
            );

            Ok(ToolOutput::text(format!(
                "The file {} has been updated successfully.",
                abs_path.display()
            )))
        } else {
            // ENOENT branch (create): skip read-state and staleness checks.

            // Create any missing parent directories before writing.
            if let Some(parent) = abs_path.parent() {
                if let Err(err) = tokio::fs::create_dir_all(parent).await {
                    return Ok(ToolOutput::error(
                        format!("Failed to create parent directory: {err}"),
                        true,
                    ));
                }
            }

            // 8. Write content verbatim — no LF/CRLF rewriting.
            tokio::fs::write(&abs_path, content.as_bytes())
                .await
                .map_err(AoError::from)?;

            // Refresh read-state with the new file's content.
            let new_mtime = tokio::fs::metadata(&abs_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or_else(SystemTime::now);
            ctx.read_file_state.record(
                abs_path.clone(),
                ReadEntry {
                    content: content.clone(),
                    mtime: new_mtime,
                    offset: None,
                    limit: None,
                    surfaced_by_read: false,
                },
            );

            Ok(ToolOutput::text(format!(
                "File created successfully at: {}",
                abs_path.display()
            )))
        }
    }
}
