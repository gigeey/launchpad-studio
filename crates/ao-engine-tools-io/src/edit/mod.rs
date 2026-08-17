//! Edit tool — exact string replacement with read-before-write enforcement.
//!
//! Refuses to mutate files that were not read first (via the `Read` tool) or
//! that have changed on disk since the read. `is_concurrency_safe()` returns
//! false so the bounded executor serialises all Edit/Write calls for a given
//! turn, eliminating the staleness-vs-write race.

use std::sync::Arc;
use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

use crate::path::expand_path;

pub mod prompt;
mod quote;
mod sanitize;

/// Maximum file size Edit will read into memory (1 GiB). Files larger than
/// this are rejected before the read so a model call cannot OOM the runner.
pub const MAX_EDIT_FILE_SIZE: u64 = 1 << 30;

#[cfg(test)]
mod tests;

/// Performs exact string replacements on a previously-read file.
///
/// Write-side IO tool. `is_concurrency_safe()` is false — concurrent
/// writes to the same file would race; the bounded executor serialises them.
pub struct Edit;

/// Register the [`Edit`] tool into `registry`.
pub fn register_edit(registry: &mut Registry) {
    registry.register_io(Arc::new(Edit));
}

#[async_trait]
impl IoTool for Edit {
    fn name(&self) -> &str {
        "Edit"
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
        let old_string = match input.get("old_string").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "old_string is required and must be a string",
                    true,
                ));
            }
        };
        let new_string = match input.get("new_string").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "new_string is required and must be a string",
                    true,
                ));
            }
        };
        // Coerce replace_all defensively: accept bool, "true"/"false"/"1"/"0", and 0/1.
        let replace_all = coerce_bool(input.get("replace_all"));

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

        // 4. old_string and new_string must differ.
        if old_string == new_string {
            return Ok(ToolOutput::error(
                "No changes to make: old_string and new_string are identical",
                true,
            ));
        }

        // 5. Cancellation check.
        if ctx.cancel.is_cancelled() {
            return Err(AoError::Internal("cancelled".to_string()));
        }

        // 6. Stat the file.
        let metadata = match tokio::fs::metadata(&abs_path).await {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if old_string.is_empty() {
                    // Create the file (and any missing parent directories).
                    if let Some(parent) = abs_path.parent() {
                        if let Err(e) = tokio::fs::create_dir_all(parent).await {
                            return Ok(ToolOutput::error(
                                format!("Failed to create parent directory: {}", e),
                                true,
                            ));
                        }
                    }
                    tokio::fs::write(&abs_path, new_string.as_bytes())
                        .await
                        .map_err(AoError::from)?;
                    let new_mtime = tokio::fs::metadata(&abs_path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or_else(SystemTime::now);
                    ctx.read_file_state.record(
                        abs_path.clone(),
                        ReadEntry {
                            content: new_string.clone(),
                            mtime: new_mtime,
                            offset: None,
                            limit: None,
                            surfaced_by_read: false,
                        },
                    );
                    return Ok(ToolOutput::text(format!(
                        "File created successfully at: {}",
                        abs_path.display()
                    )));
                }
                return Ok(ToolOutput::error(
                    format!("File does not exist: {}", abs_path.display()),
                    true,
                ));
            }
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to stat {}: {}", abs_path.display(), err),
                    true,
                ));
            }
        };

        let current_mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());

        // Size cap: refuse before reading the file so a >1 GiB file cannot OOM the runner.
        if metadata.len() > MAX_EDIT_FILE_SIZE {
            return Ok(ToolOutput::error(
                format!(
                    "File is too large to edit (size: {} bytes; max: {} bytes). Use a different tool or split the file.",
                    metadata.len(),
                    MAX_EDIT_FILE_SIZE
                ),
                true,
            ));
        }

        // .ipynb redirect — refuse before reading the file into memory.
        if abs_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ipynb"))
            .unwrap_or(false)
        {
            return Ok(ToolOutput::error(
                "Editing Jupyter notebooks via Edit is not supported. The NotebookEdit tool will land in a follow-up bucket.",
                true,
            ));
        }

        // Read raw bytes and decode.
        let bytes = tokio::fs::read(&abs_path).await.map_err(AoError::from)?;
        let decoded_raw = decode_content(&bytes);

        // Detect CRLF line endings so we can restore them after editing.
        // All matching and replacement happen in LF space; CRLF is restored on write.
        let had_crlf = decoded_raw.contains("\r\n");
        let content = if had_crlf {
            decoded_raw.replace("\r\n", "\n")
        } else {
            decoded_raw.clone()
        };

        // Handle empty old_string on an existing file.
        // An empty/whitespace-only file can be overwritten; a non-empty file cannot.
        if old_string.is_empty() {
            if !content.trim().is_empty() {
                return Ok(ToolOutput::error(
                    "Cannot create file: file already exists. Pass a non-empty old_string to edit existing content.",
                    true,
                ));
            }
            // Gate: file must have been read before we overwrite it.
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
                    "File was only partially read. Re-read the full file before editing.",
                    true,
                ));
            }
            if current_mtime > entry.mtime && decoded_raw != entry.content {
                return Ok(ToolOutput::error(
                    "File has been modified since it was last read. Re-read the file before editing.",
                    true,
                ));
            }
            let to_write = if had_crlf {
                new_string.replace('\n', "\r\n")
            } else {
                new_string.clone()
            };
            tokio::fs::write(&abs_path, to_write.as_bytes())
                .await
                .map_err(AoError::from)?;
            let new_mtime = tokio::fs::metadata(&abs_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or_else(SystemTime::now);
            ctx.read_file_state.record(
                abs_path.clone(),
                ReadEntry {
                    content: to_write,
                    mtime: new_mtime,
                    offset: None,
                    limit: None,
                    surfaced_by_read: false,
                },
            );
            return Ok(ToolOutput::text(format!(
                "The file {} has been updated.",
                abs_path.display()
            )));
        }

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
                "File was only partially read. Re-read the full file before editing.",
                true,
            ));
        }

        // Staleness check: mtime advanced AND bytes diverged from the snapshot.
        // Compares decoded_raw (raw bytes as string, possibly CRLF) against
        // entry.content, which is also raw — the Read tool stores raw bytes.
        // Fall-through is allowed when mtime advanced but bytes are identical —
        // cloud sync, antivirus, and format-on-save tools touch the mtime
        // without changing the file bytes.
        if current_mtime > entry.mtime && decoded_raw != entry.content {
            return Ok(ToolOutput::error(
                "File has been modified since it was last read. Re-read the file before editing.",
                true,
            ));
        }

        // Find old_string in the LF-normalised content. First try exact match
        // and curly-quote normalisation (quote.rs). If that fails, apply the
        // XML-token de-sanitization fallback (sanitize.rs) and retry.
        //
        // We track `effective_old` (the string that was actually used to find
        // `actual`) so that preserve_quote_style can build a correct char map.
        // Using the model's raw old_string in the desanitize path would build a
        // garbage map from the abbreviated tokens vs. the expanded file content.
        let (actual, effective_old, effective_new) =
            match quote::find_actual_string(&content, &old_string) {
                Some(a) => (a, old_string.clone(), new_string.clone()),
                None => {
                    let expanded_old = sanitize::expand_sanitized_tokens(&old_string);
                    if expanded_old != old_string {
                        match quote::find_actual_string(&content, &expanded_old) {
                            Some(a) => {
                                let expanded_new = sanitize::expand_sanitized_tokens(&new_string);
                                (a, expanded_old, expanded_new)
                            }
                            None => {
                                return Ok(ToolOutput::error(
                                    "String to replace not found in file.",
                                    true,
                                ));
                            }
                        }
                    } else {
                        return Ok(ToolOutput::error(
                            "String to replace not found in file.",
                            true,
                        ));
                    }
                }
            };

        // Preserve the file's quote typography in the replacement text.
        let styled_new = quote::preserve_quote_style(&effective_old, &actual, &effective_new);

        // Strip trailing whitespace per line in the replacement, unless the
        // target is a Markdown file (trailing double-space is a hard line break).
        let is_markdown = abs_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("mdx"))
            .unwrap_or(false);
        let styled_new = if is_markdown {
            styled_new
        } else {
            strip_trailing_whitespace(&styled_new)
        };

        // When the replacement is empty (a deletion), absorb the trailing newline
        // that follows the deleted text so no dangling blank line is left behind.
        let (match_target, replacement) = if new_string.is_empty() {
            let with_newline = format!("{}\n", actual);
            if content.contains(with_newline.as_str()) {
                (with_newline, String::new())
            } else {
                (actual.clone(), String::new())
            }
        } else {
            (actual.clone(), styled_new)
        };

        // Count occurrences of the effective match target.
        let count = content.matches(match_target.as_str()).count();
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(
                format!(
                    "Found {} matches of the string. Either include more surrounding context to make old_string unique, or pass replace_all: true to replace all occurrences.",
                    count
                ),
                true,
            ));
        }

        // Apply replacement in LF space.
        let new_content_lf = if replace_all {
            content.replace(match_target.as_str(), &replacement)
        } else {
            // Replace the first occurrence only.
            let mut parts = content.splitn(2, match_target.as_str());
            match (parts.next(), parts.next()) {
                (Some(before), Some(after)) => {
                    let mut s =
                        String::with_capacity(before.len() + replacement.len() + after.len());
                    s.push_str(before);
                    s.push_str(&replacement);
                    s.push_str(after);
                    s
                }
                // count > 0 guarantees at least one occurrence.
                _ => unreachable!(),
            }
        };

        // Restore the file's original line endings on write.
        let to_write = if had_crlf {
            new_content_lf.replace('\n', "\r\n")
        } else {
            new_content_lf
        };

        // Write the edited content back to disk.
        tokio::fs::write(&abs_path, to_write.as_bytes())
            .await
            .map_err(AoError::from)?;

        // Refresh the read-state. We store the raw on-disk content (same
        // representation the Read tool uses) so a follow-up staleness check
        // compares apples to apples.
        let new_mtime = tokio::fs::metadata(&abs_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or_else(SystemTime::now);
        ctx.read_file_state.record(
            abs_path.clone(),
            ReadEntry {
                content: to_write,
                mtime: new_mtime,
                offset: None,
                limit: None,
                surfaced_by_read: false,
            },
        );

        let suffix = if replace_all {
            " (all occurrences replaced)"
        } else {
            ""
        };
        Ok(ToolOutput::text(format!(
            "The file {} has been updated.{}",
            abs_path.display(),
            suffix
        )))
    }
}

/// Decode raw file bytes to a `String`.
///
/// Detects UTF-16-LE via the `0xFF 0xFE` BOM; falls back to lossily-decoded
/// UTF-8 for everything else (mirrors the Read tool's decode strategy).
fn decode_content(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let shorts: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        String::from_utf16_lossy(&shorts)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Coerce a JSON value to `bool` for the `replace_all` field.
///
/// Accepts a real JSON bool, the strings `"true"`/`"false"` and `"1"`/`"0"`
/// (case-insensitive), and the numbers `1`/`0`. Anything else returns `false`.
/// The INPUT_SCHEMA still declares `type: "boolean"` — this is a defensive
/// runtime coercion for providers that serialize booleans as strings.
pub(super) fn coerce_bool(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1"),
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// Strip trailing spaces and tabs from each line of `s`.
///
/// Preserves line count and trailing newlines. Only ASCII whitespace (space
/// and tab) at line ends is removed — line content is otherwise unchanged.
fn strip_trailing_whitespace(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    let trailing_newline = s.ends_with('\n');
    let mut result = s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    if trailing_newline {
        result.push('\n');
    }
    result
}
