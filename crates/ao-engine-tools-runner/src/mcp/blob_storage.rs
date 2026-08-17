//! Persistence helpers for MCP binary content (images, audio, blobs).
//!
//! MCP tool results can include binary payloads — screenshots from browser
//! automation servers, audio from speech tools, PDF documents, and arbitrary
//! binary blobs.  This module decodes the base64-encoded payloads and writes
//! them to a dedicated directory under the user's data root so the model can
//! reference them by local path.
//!
//! The output directory is always resolved through
//! [`ao_protocol::data_root::resolve_data_root`], which honours the
//! `LAUNCHPAD_STUDIO_DATA_DIR` environment variable.  Nothing in this module
//! hardcodes a specific path.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose, Engine as _};
use uuid::Uuid;

// ── MIME type helpers ─────────────────────────────────────────────────────────

/// Map a MIME type string to a file extension (without the leading dot).
///
/// Unknown or absent types fall back to `"bin"`.  The input may include
/// MIME parameters separated by `;` (e.g. `"text/plain; charset=utf-8"`) —
/// only the base type is considered.
pub fn extension_for_mime(mime: &str) -> &'static str {
    let base = mime.split(';').next().unwrap_or("").trim();
    match base {
        // Images
        "image/apng"                        => "apng",
        "image/avif"                        => "avif",
        "image/bmp"                         => "bmp",
        "image/gif"                         => "gif",
        "image/ico"
        | "image/x-icon"
        | "image/vnd.microsoft.icon"        => "ico",
        "image/jpeg" | "image/jpg"          => "jpg",
        "image/png"                         => "png",
        "image/svg+xml"                     => "svg",
        "image/tiff"                        => "tiff",
        "image/webp"                        => "webp",
        // Audio
        "audio/aac"                         => "aac",
        "audio/flac"                        => "flac",
        "audio/mp3" | "audio/mpeg"          => "mp3",
        "audio/mp4"                         => "m4a",
        "audio/ogg"                         => "ogg",
        "audio/opus"                        => "opus",
        "audio/wav" | "audio/wave"
        | "audio/x-wav"                     => "wav",
        "audio/webm"                        => "weba",
        // Video
        "video/mp4"                         => "mp4",
        "video/ogg"                         => "ogv",
        "video/webm"                        => "webm",
        // Documents / data
        "application/csv" | "text/csv"      => "csv",
        "application/gzip"
        | "application/x-gzip"              => "gz",
        "application/json"                  => "json",
        "application/octet-stream"          => "bin",
        "application/pdf"                   => "pdf",
        "application/xml" | "text/xml"      => "xml",
        "application/zip"                   => "zip",
        "text/html"                         => "html",
        "text/markdown"
        | "text/x-markdown"                 => "md",
        "text/plain"                        => "txt",
        _                                   => "bin",
    }
}

/// Returns `true` if the MIME type describes an image that can be passed
/// inline to a model (e.g. `image/png`, `image/jpeg`, `image/webp`).
///
/// Only the base type (before any `;` separator) is inspected.
pub fn is_image_mime(mime: &str) -> bool {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .starts_with("image/")
}

// ── Base64 decoding ───────────────────────────────────────────────────────────

/// Decode a base64-encoded string into raw bytes.
///
/// Tries standard base64 (with padding) first, then URL-safe base64 as a
/// fallback.  Returns `None` if neither variant decodes successfully.
pub fn decode_base64(data: &str) -> Option<Vec<u8>> {
    general_purpose::STANDARD
        .decode(data)
        .ok()
        .or_else(|| general_purpose::URL_SAFE.decode(data).ok())
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Write `bytes` to `output_dir/<unique>.<ext>` and return the path.
///
/// Creates `output_dir` and any missing parent directories when they do not
/// exist.  The file extension is derived from `mime_type` via
/// [`extension_for_mime`].  Filenames include a UUID so concurrent writes
/// from multiple MCP tool calls never collide.
///
/// Returns an error string on I/O failure.  Callers should convert the error
/// into a graceful text fallback rather than propagating it to the model.
pub fn persist_blob_to_dir(
    bytes: &[u8],
    mime_type: &str,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(output_dir).map_err(|e| {
        format!(
            "could not create output directory '{}': {e}",
            output_dir.display()
        )
    })?;

    let ext = extension_for_mime(mime_type);
    let path = output_dir.join(format!("mcp-{}.{}", Uuid::new_v4(), ext));

    std::fs::write(&path, bytes)
        .map_err(|e| format!("could not write to '{}': {e}", path.display()))?;

    Ok(path)
}

/// Write `bytes` under `<data_root>/mcp-output/` and return the path.
///
/// The data root is resolved via
/// [`ao_protocol::data_root::resolve_data_root`].  Returns an error string
/// when the root cannot be determined or the write fails.
pub fn persist_blob(bytes: &[u8], mime_type: &str) -> Result<PathBuf, String> {
    let data_root = ao_protocol::data_root::resolve_data_root()
        .map_err(|e| format!("could not resolve data root: {e}"))?;
    persist_blob_to_dir(bytes, mime_type, &data_root.join("mcp-output"))
}

// ── Human-readable notes ──────────────────────────────────────────────────────

/// Build a concise note describing where a blob was persisted.
///
/// Format: `"Saved to <path> (<size>, <mime>)"`
pub fn saved_note(path: &Path, byte_count: usize, mime_type: &str) -> String {
    format!(
        "Saved to {} ({}, {})",
        path.display(),
        human_size(byte_count),
        mime_type,
    )
}

/// Format a byte count as a human-readable size string (B / KB / MB).
fn human_size(bytes: usize) -> String {
    if bytes < 1_024 {
        format!("{bytes} B")
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1_024.0 * 1_024.0))
    }
}

// ── Combined helper ───────────────────────────────────────────────────────────

/// Decode a base64 payload and persist it under the data root.
///
/// Returns a human-readable note (beginning with `"Saved to "`) describing
/// where the file was written.  If decoding or the write fails, the returned
/// string describes the failure so the model still receives useful diagnostic
/// text rather than a silent omission.
///
/// This is the primary entry point for audio blobs, non-image resource blobs,
/// and any binary MCP content the model cannot consume inline.
pub fn decode_and_persist(data: &str, mime_type: &str) -> String {
    let Some(bytes) = decode_base64(data) else {
        return format!(
            "Binary content (type: {mime_type}) could not be decoded — malformed base64 payload"
        );
    };
    let byte_count = bytes.len();
    match persist_blob(&bytes, mime_type) {
        Ok(path) => saved_note(&path, byte_count, mime_type),
        Err(err) => format!(
            "Binary content (type: {mime_type}, {}) could not be saved: {err}",
            human_size(byte_count),
        ),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Extension map ────────────────────────────────────────────────────────

    #[test]
    fn extension_common_image_types() {
        assert_eq!(extension_for_mime("image/png"), "png");
        assert_eq!(extension_for_mime("image/jpeg"), "jpg");
        assert_eq!(extension_for_mime("image/jpg"), "jpg");
        assert_eq!(extension_for_mime("image/gif"), "gif");
        assert_eq!(extension_for_mime("image/webp"), "webp");
        assert_eq!(extension_for_mime("image/svg+xml"), "svg");
        assert_eq!(extension_for_mime("image/bmp"), "bmp");
    }

    #[test]
    fn extension_common_audio_types() {
        assert_eq!(extension_for_mime("audio/mpeg"), "mp3");
        assert_eq!(extension_for_mime("audio/mp3"), "mp3");
        assert_eq!(extension_for_mime("audio/wav"), "wav");
        assert_eq!(extension_for_mime("audio/ogg"), "ogg");
        assert_eq!(extension_for_mime("audio/aac"), "aac");
        assert_eq!(extension_for_mime("audio/flac"), "flac");
    }

    #[test]
    fn extension_common_document_types() {
        assert_eq!(extension_for_mime("application/pdf"), "pdf");
        assert_eq!(extension_for_mime("application/json"), "json");
        assert_eq!(extension_for_mime("text/plain"), "txt");
        assert_eq!(extension_for_mime("text/html"), "html");
        assert_eq!(extension_for_mime("application/zip"), "zip");
    }

    #[test]
    fn extension_strips_mime_parameters() {
        assert_eq!(extension_for_mime("text/plain; charset=utf-8"), "txt");
        assert_eq!(extension_for_mime("image/jpeg; q=0.9"), "jpg");
    }

    #[test]
    fn extension_unknown_types_fall_back_to_bin() {
        assert_eq!(extension_for_mime("application/x-custom-binary"), "bin");
        assert_eq!(extension_for_mime(""), "bin");
        assert_eq!(extension_for_mime("totally/unknown"), "bin");
    }

    // ── is_image_mime ────────────────────────────────────────────────────────

    #[test]
    fn is_image_mime_true_for_image_types() {
        assert!(is_image_mime("image/png"));
        assert!(is_image_mime("image/jpeg"));
        assert!(is_image_mime("image/webp"));
        assert!(is_image_mime("image/gif"));
    }

    #[test]
    fn is_image_mime_false_for_non_image_types() {
        assert!(!is_image_mime("audio/mpeg"));
        assert!(!is_image_mime("application/pdf"));
        assert!(!is_image_mime("text/plain"));
        assert!(!is_image_mime("video/mp4"));
        assert!(!is_image_mime(""));
    }

    // ── Base64 decoding ──────────────────────────────────────────────────────

    #[test]
    fn decode_base64_standard_padded() {
        // "hello" → "aGVsbG8="
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn decode_base64_empty_input_returns_empty_bytes() {
        assert_eq!(decode_base64("").unwrap(), b"");
    }

    #[test]
    fn decode_base64_invalid_returns_none() {
        assert!(decode_base64("!!! not valid base64 !!!").is_none());
    }

    // ── persist_blob_to_dir ──────────────────────────────────────────────────

    #[test]
    fn persist_blob_to_dir_creates_file_with_mime_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = persist_blob_to_dir(b"png data", "image/png", tmp.path()).unwrap();
        assert!(path.exists(), "file should exist at {}", path.display());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&path).unwrap(), b"png data");
    }

    #[test]
    fn persist_blob_to_dir_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("level1").join("level2");
        let path = persist_blob_to_dir(b"x", "audio/mpeg", &deep).unwrap();
        assert!(path.exists());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp3"));
    }

    #[test]
    fn persist_blob_to_dir_unique_names_no_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = persist_blob_to_dir(b"a", "image/png", tmp.path()).unwrap();
        let p2 = persist_blob_to_dir(b"b", "image/png", tmp.path()).unwrap();
        assert_ne!(p1, p2, "each blob must receive a unique filename");
    }

    // ── saved_note ───────────────────────────────────────────────────────────

    #[test]
    fn saved_note_contains_path_size_and_mime() {
        let note = saved_note(
            Path::new("/data/mcp-output/mcp-abc.png"),
            2_048,
            "image/png",
        );
        assert!(
            note.contains("/data/mcp-output/mcp-abc.png"),
            "note: {note}"
        );
        assert!(note.contains("image/png"), "note: {note}");
        assert!(note.contains("2.0 KB"), "note: {note}");
    }

    #[test]
    fn human_size_boundaries() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1_023), "1023 B");
        assert_eq!(human_size(1_024), "1.0 KB");
        assert_eq!(human_size(1_024 * 1_024), "1.0 MB");
    }

    // ── decode_and_persist ───────────────────────────────────────────────────

    #[test]
    fn decode_and_persist_invalid_base64_returns_descriptive_error() {
        let result = decode_and_persist("!!! not base64 !!!", "image/png");
        assert!(
            result.contains("could not be decoded"),
            "result should describe the decode failure: {result}"
        );
    }

    // ── Full data-root integration ────────────────────────────────────────────

    #[test]
    fn persist_blob_lands_under_data_root_mcp_output() {
        // Pin the data root to a tempdir; the guard serializes against every
        // other test in this binary that touches the same process-global var.
        let guard = crate::test_env::DataDirGuard::new();

        let bytes = b"audio content";
        let result = persist_blob(bytes, "audio/mpeg");

        let path = result.expect("persist_blob should succeed with a writable data root");

        assert!(
            path.starts_with(guard.data_dir().join("mcp-output")),
            "file should be under data_root/mcp-output, got: {}",
            path.display()
        );
        assert!(path.exists(), "file should exist at {}", path.display());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp3"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // The saved note should embed the path.
        let note = saved_note(&path, bytes.len(), "audio/mpeg");
        assert!(
            note.contains(path.to_str().unwrap()),
            "saved note should embed the path: {note}"
        );
        assert!(note.contains("audio/mpeg"), "saved note should include mime: {note}");
    }
}
