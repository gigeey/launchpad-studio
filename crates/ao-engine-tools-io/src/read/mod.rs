//! Read tool — reads a file from the local filesystem and returns it `cat -n`
//! formatted (`\t<lineno>\t<line>` per the PRD). Defaults: `offset = 1`,
//! `limit = 2000` lines. Rejects relative paths and directories with recoverable errors,
//! sniffs binary content (NUL byte in the first 8 KiB) and rejects it,
//! truncates individual lines longer than `MAX_LINE_LENGTH` with a marker,
//! and returns the empty-file system-reminder Text for zero-byte files.
//!
//! Images (PNG/JPEG/GIF/WebP) and PDFs are recognised by extension and returned
//! as base64 media blocks ([`ToolOutput::image`] / [`ToolOutput::document`])
//! rather than text. That detection happens before the text-path size cap and
//! binary sniff, which would otherwise reject these (necessarily binary) files;
//! each media kind has its own larger size cap ([`MAX_IMAGE_BYTES`],
//! [`MAX_PDF_BYTES`]).
//!
//! Cancellation: `RunnerContext::cancel` is polled between line batches.
//! The branch's `AoError` enum has no `Cancelled` variant yet, so the
//! cancellation surface is `AoError::Internal("cancelled")` — see
//! [`CANCELLED_MESSAGE`]. When `Cancelled` lands on main this should switch
//! to `AoError::Cancelled`.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, ReadEntry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};

mod notebook;
pub mod prompt;

/// Default line limit when `limit` is not supplied by the caller.
pub const DEFAULT_LIMIT: usize = 2000;

/// Maximum length of a single line (characters) before it is truncated and
/// a marker is appended. Matches `DEFAULT_LIMIT` per the PRD.
pub const MAX_LINE_LENGTH: usize = 2000;

/// Number of lines processed per cancellation poll. Cheap to bump; the
/// 100 ms cancellation budget in the PRD test is comfortable at 64.
const CANCEL_POLL_BATCH: usize = 64;

/// Bytes scanned at the head of a file to decide binary vs text.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// String surfaced through `AoError::Internal` when a Read invocation is
/// cancelled mid-flight. Promoted to `AoError::Cancelled` once that variant
/// lands on main.
pub const CANCELLED_MESSAGE: &str = "cancelled";

/// Device and pseudo-file paths that must never be opened. Reading /dev/zero
/// or /dev/urandom loops forever / exhausts memory; the others expose process
/// I/O or have no meaningful text content. The /dev/fd/ prefix covers all
/// numbered file-descriptor entries (/dev/fd/0, /dev/fd/1, …).
const BLOCKED_DEVICE_PATHS: &[&str] = &[
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/null",
    "/dev/tty",
];

/// Maximum file size that can be read whole. Files larger than this require
/// the caller to supply `offset`/`limit` to read a slice.
pub const MAX_FILE_SIZE_BYTES: usize = 256 * 1024;

/// Maximum size of an image file returned as a base64 media block. Images are
/// delivered whole (offset/limit don't apply), so the cap bounds the base64
/// payload sent to the model. Chosen to comfortably cover screenshots and
/// typical raster assets without risking an oversized request.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Maximum size of a PDF returned as a base64 document block. Higher than the
/// image cap because documents are legitimately larger; still bounded so a
/// single read can't blow the request budget.
pub const MAX_PDF_BYTES: usize = 16 * 1024 * 1024;

/// Maximum estimated token count for content returned in a single call.
/// Token count is estimated as `chars / 4` — cheap heuristic, no tokenizer
/// dependency. Applies to the formatted slice actually returned (after
/// offset/limit), not to the raw file bytes.
pub const MAX_TOKENS: usize = 25_000;

/// System-reminder text returned for empty files, matching the placeholder
/// the CLI surfaces when a read yields no content.
pub const EMPTY_FILE_REMINDER: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";

/// Returned instead of the full file content when the file has not changed
/// since the last read and the requested view window is identical. Saves
/// prompt-cache tokens on re-reads of stable files within a session.
pub const FILE_UNCHANGED_STUB: &str =
    "<system-reminder>File unchanged since last read — prior content still applies.</system-reminder>";

/// Marker appended to lines longer than [`MAX_LINE_LENGTH`].
const LINE_TRUNCATION_MARKER: &str = "... [line truncated]";

/// Reads a file from the local filesystem.
///
/// Read-side IO tool. `is_concurrency_safe()` is true — the tool is read-only
/// and may be fanned out within a single assistant turn.
pub struct Read;

#[async_trait]
impl IoTool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "Line number to start reading from (1-based, matches the line numbers shown in output)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "default": DEFAULT_LIMIT,
                    "description": "Maximum number of lines to read."
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let file_path = match input.get("file_path").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "file_path is required and must be a string",
                    true,
                ));
            }
        };

        // Preserve whether the caller explicitly supplied offset/limit — None
        // means "caller omitted the field" and maps to is_full_read() == true
        // in ReadFileState.  The local usize values drive the actual slicing.
        let offset_caller: Option<usize> = coerce_usize(input.get("offset"));
        let limit_caller: Option<usize> = coerce_usize(input.get("limit"));
        let offset = offset_caller.unwrap_or(1);
        let limit = limit_caller.unwrap_or(DEFAULT_LIMIT);

        // Normalize: trim surrounding whitespace, then expand a leading `~` or
        // `~/` to the user's home directory.  `~user` forms are left unchanged
        // so the absolute-path check below rejects them.
        let file_path = file_path.trim().to_string();
        let file_path = expand_tilde(file_path);

        let path = Path::new(&file_path);

        if !path.is_absolute() {
            return Ok(ToolOutput::error(
                format!(
                    "file_path must be an absolute path, got relative path: {}",
                    file_path
                ),
                true,
            ));
        }

        // Reject device/pseudo paths before any I/O. Exact matches cover the
        // common devices; the /dev/fd/ prefix check covers numbered fd entries.
        let path_str = path.to_string_lossy();
        let is_blocked =
            BLOCKED_DEVICE_PATHS.contains(&path_str.as_ref()) || path_str.starts_with("/dev/fd/");
        if is_blocked {
            return Ok(ToolOutput::error(
                format!(
                    "reading device or pseudo file is not allowed: {}",
                    file_path
                ),
                true,
            ));
        }

        // Stat first so we can distinguish missing / dir / file before any
        // read syscalls.
        let metadata = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut msg = format!("file does not exist: {}", file_path);
                let suggestions = similar_names(path, 3).await;
                if !suggestions.is_empty() {
                    let names: Vec<String> = suggestions
                        .iter()
                        .filter_map(|p| p.to_str())
                        .map(|s| s.to_string())
                        .collect();
                    msg.push_str(&format!("\n\ndid you mean: {}?", names.join(", ")));
                }
                return Ok(ToolOutput::error(msg, true));
            }
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to stat {}: {}", file_path, err),
                    true,
                ));
            }
        };

        if metadata.is_dir() {
            return Ok(ToolOutput::error(
                format!(
                    "{} is a directory, not a file. Use the Glob tool to enumerate directory contents.",
                    file_path
                ),
                true,
            ));
        }

        let mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());

        // Dedup: if the same file was read in this session with the same view
        // window and mtime hasn't advanced, return a lightweight stub instead
        // of re-sending the full content.
        if let Some(prev) = ctx.read_file_state.get(path) {
            // Only suppress content the model has already seen as Read output.
            // A snapshot left behind by an edit (surfaced_by_read == false) holds
            // the post-edit content the model has not yet seen, so it must fall
            // through and surface the real content rather than the stub.
            if prev.surfaced_by_read
                && prev.mtime == mtime
                && prev.offset == offset_caller
                && prev.limit == limit_caller
            {
                return Ok(ToolOutput::text(FILE_UNCHANGED_STUB));
            }
        }

        if metadata.len() == 0 {
            ctx.read_file_state.record(
                PathBuf::from(path),
                ReadEntry {
                    content: String::new(),
                    mtime,
                    offset: None,
                    limit: None,
                    surfaced_by_read: true,
                },
            );
            return Ok(ToolOutput::text(EMPTY_FILE_REMINDER));
        }

        // Images and PDFs are returned to the model as base64 media blocks, not
        // text. They must be detected by extension *before* the text-path size
        // cap and binary sniff below — both of which would otherwise reject the
        // file. Each media kind has its own (larger) size cap.
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let file_size = metadata.len() as usize;

        if let Some(media_type) = image_media_type(extension) {
            if file_size > MAX_IMAGE_BYTES {
                return Ok(ToolOutput::error(
                    format!(
                        "{} is too large to read as an image ({} bytes; cap is {} bytes).",
                        file_path, file_size, MAX_IMAGE_BYTES
                    ),
                    true,
                ));
            }
            if ctx.cancel.is_cancelled() {
                return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
            }
            let bytes = tokio::fs::read(path).await.map_err(AoError::from)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            ctx.read_file_state.record(
                PathBuf::from(path),
                ReadEntry {
                    content: String::new(),
                    mtime,
                    offset: offset_caller,
                    limit: limit_caller,
                    surfaced_by_read: true,
                },
            );
            return Ok(ToolOutput::image(media_type, encoded));
        }

        if extension.eq_ignore_ascii_case("pdf") {
            if file_size > MAX_PDF_BYTES {
                return Ok(ToolOutput::error(
                    format!(
                        "{} is too large to read as a PDF ({} bytes; cap is {} bytes).",
                        file_path, file_size, MAX_PDF_BYTES
                    ),
                    true,
                ));
            }
            if ctx.cancel.is_cancelled() {
                return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
            }
            let bytes = tokio::fs::read(path).await.map_err(AoError::from)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let title = path.file_name().and_then(|n| n.to_str()).map(String::from);
            let summary = format!(
                "Read PDF {} ({} bytes).",
                title.as_deref().unwrap_or("file"),
                file_size
            );
            ctx.read_file_state.record(
                PathBuf::from(path),
                ReadEntry {
                    content: String::new(),
                    mtime,
                    offset: offset_caller,
                    limit: limit_caller,
                    surfaced_by_read: true,
                },
            );
            return Ok(ToolOutput::document(
                "application/pdf",
                encoded,
                title,
                Some(summary),
            ));
        }

        if file_size > MAX_FILE_SIZE_BYTES {
            return Ok(ToolOutput::error(
                format!(
                    "{} is too large to read whole ({} bytes; cap is {} bytes). \
                     Use offset and limit to read a slice.",
                    file_path, file_size, MAX_FILE_SIZE_BYTES
                ),
                true,
            ));
        }

        if ctx.cancel.is_cancelled() {
            return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
        }

        // Open the file. Read the first BINARY_SNIFF_BYTES to detect binary
        // content without loading the whole file, then seek back to the start.
        let mut file = tokio::fs::File::open(path).await.map_err(AoError::from)?;
        {
            let sniff_size = BINARY_SNIFF_BYTES.min(file_size);
            let mut head = vec![0u8; sniff_size];
            let n = file.read(&mut head).await.map_err(AoError::from)?;
            head.truncate(n);
            if is_binary(&head) {
                return Ok(ToolOutput::error(
                    format!(
                        "{} appears to be a binary file and cannot be read as text",
                        file_path
                    ),
                    true,
                ));
            }
        }
        file.seek(SeekFrom::Start(0)).await.map_err(AoError::from)?;

        // Jupyter notebooks: parse JSON and emit a structured cell-by-cell view.
        // `extension` was bound above for the image/PDF media checks.
        if extension.eq_ignore_ascii_case("ipynb") {
            let mut bytes = Vec::with_capacity(file_size);
            file.read_to_end(&mut bytes).await.map_err(AoError::from)?;
            match notebook::render(&bytes) {
                Ok(rendered) => {
                    ctx.read_file_state.record(
                        PathBuf::from(path),
                        ReadEntry {
                            content: rendered.clone(),
                            mtime,
                            offset: offset_caller,
                            limit: limit_caller,
                            surfaced_by_read: true,
                        },
                    );
                    return Ok(ToolOutput::text(rendered));
                }
                Err(err_msg) => {
                    return Ok(ToolOutput::error(
                        format!("failed to parse {file_path} as a Jupyter notebook: {err_msg}"),
                        true,
                    ));
                }
            }
        }

        // Stream lines with BufReader — only lines in [offset, offset+limit) are
        // materialized. Lossy UTF-8 decoding is applied per line so stray
        // non-UTF-8 bytes (e.g. latin-1 in comments) don't fail the read.
        // Convert 1-based caller offset to a 0-based skip count.
        let start_idx = offset.saturating_sub(1);
        let mut reader = BufReader::new(file);
        let mut out = String::new();
        let mut raw_content_bytes: Vec<u8> = Vec::new();
        let mut line_buf: Vec<u8> = Vec::new();
        let mut idx = 0usize;
        let mut emitted = 0usize;
        let mut wrote_any = false;

        loop {
            if idx % CANCEL_POLL_BATCH == 0 && ctx.cancel.is_cancelled() {
                return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
            }

            line_buf.clear();
            let n = reader
                .read_until(b'\n', &mut line_buf)
                .await
                .map_err(AoError::from)?;
            if n == 0 {
                break; // EOF
            }

            // Accumulate raw bytes only for lines in the requested slice.
            if idx >= start_idx && emitted < limit {
                raw_content_bytes.extend_from_slice(&line_buf);
            }

            if idx < start_idx {
                idx += 1;
                continue;
            }
            if emitted >= limit {
                break;
            }

            // Strip trailing newline (or CRLF) for display — matches
            // split_terminator('\n'): a trailing newline on the last line
            // does not produce a phantom empty line.
            let display_bytes = if line_buf.ends_with(b"\r\n") {
                &line_buf[..line_buf.len() - 2]
            } else if line_buf.ends_with(b"\n") {
                &line_buf[..line_buf.len() - 1]
            } else {
                &line_buf[..]
            };

            let raw_line = String::from_utf8_lossy(display_bytes);
            let line_no = idx + 1;
            let line = if raw_line.chars().count() > MAX_LINE_LENGTH {
                let truncated: String = raw_line.chars().take(MAX_LINE_LENGTH).collect();
                format!("{}{}", truncated, LINE_TRUNCATION_MARKER)
            } else {
                raw_line.into_owned()
            };

            if wrote_any {
                out.push('\n');
            }
            out.push('\t');
            out.push_str(&line_no.to_string());
            out.push('\t');
            out.push_str(&line);

            wrote_any = true;
            emitted += 1;
            idx += 1;
        }

        // Token-count guard (post-format): estimate tokens as chars/4 — cheap
        // heuristic that avoids a tokenizer dependency. Applied to the
        // formatted slice being returned, so offset/limit already narrowed it.
        let estimated_tokens = out.chars().count() / 4;
        if estimated_tokens > MAX_TOKENS {
            return Ok(ToolOutput::error(
                format!(
                    "the returned content is too large (~{} estimated tokens; cap is {} tokens). \
                     Use offset and limit to read a smaller slice.",
                    estimated_tokens, MAX_TOKENS
                ),
                true,
            ));
        }

        ctx.read_file_state.record(
            PathBuf::from(path),
            ReadEntry {
                content: String::from_utf8_lossy(&raw_content_bytes).into_owned(),
                mtime,
                offset: offset_caller,
                limit: limit_caller,
                surfaced_by_read: true,
            },
        );

        Ok(ToolOutput::text(out))
    }
}

/// Expands a leading `~` or `~/` to the user's home directory.
///
/// Only the bare-tilde forms are expanded; `~user` is left as-is so the
/// absolute-path check downstream rejects it with a clear error.
fn expand_tilde(path: String) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if path == "~" {
                return home;
            }
            return format!("{}{}", home, &path[1..]);
        }
    }
    path
}

/// Coerces a JSON value to `usize`, accepting both JSON numbers and
/// string-encoded numbers (e.g. `"50"` → `50`). Returns `None` when the
/// value is absent or unparseable — "caller omitted" semantics are preserved.
fn coerce_usize(v: Option<&Value>) -> Option<usize> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n as usize);
    }
    v.as_str()?.trim().parse::<u64>().ok().map(|n| n as usize)
}

/// Maps a file extension to an image MIME type, or `None` if the extension is
/// not a supported raster image. Matching is case-insensitive so `.PNG` and
/// `.png` behave identically. The supported set mirrors what the downstream
/// providers accept as inline image media.
fn image_media_type(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// True if `bytes` looks like a binary file. We use the same heuristic as
/// most editors / `file(1)`: a NUL byte in the first 8 KiB is a strong
/// signal that the file is not text.
fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    head.contains(&0)
}

/// Max directory entries scanned when looking for similar filenames.
const DID_YOU_MEAN_SCAN_LIMIT: usize = 300;

/// Scan `missing_path`'s parent directory for filenames close to the missing
/// name. Returns up to `max` absolute paths, ordered best-match first.
///
/// Skips the scan silently if the parent is absent, unreadable, or has more
/// than [`DID_YOU_MEAN_SCAN_LIMIT`] entries. Uses case-insensitive equality
/// (score 0) then Levenshtein edit distance (score = distance).
async fn similar_names(missing_path: &Path, max: usize) -> Vec<PathBuf> {
    let parent = match missing_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return vec![],
    };
    let missing_name = match missing_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_ascii_lowercase(),
        None => return vec![],
    };

    let mut read_dir = match tokio::fs::read_dir(parent).await {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let threshold = (missing_name.len() / 3).max(1).min(4);
    let mut candidates: Vec<(usize, PathBuf)> = Vec::new();
    let mut count = 0usize;

    loop {
        let entry = match read_dir.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => break,
        };
        count += 1;
        if count > DID_YOU_MEAN_SCAN_LIMIT {
            return vec![];
        }
        let raw = entry.file_name();
        let name = match raw.to_str() {
            Some(n) => n,
            None => continue,
        };
        let lower = name.to_ascii_lowercase();
        let score = if lower == missing_name {
            0
        } else {
            let d = levenshtein(&lower, &missing_name);
            if d > threshold {
                continue;
            }
            d
        };
        candidates.push((score, entry.path()));
    }

    candidates.sort_by_key(|(score, _)| *score);
    candidates.truncate(max);
    candidates.into_iter().map(|(_, p)| p).collect()
}

/// Levenshtein edit distance between two strings (char-level).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut row: Vec<usize> = (0..=m).collect();
    for i in 1..=n {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=m {
            let old = row[j];
            row[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(row[j]).min(row[j - 1])
            };
            prev = old;
        }
    }
    row[m]
}

#[cfg(test)]
mod tests;
