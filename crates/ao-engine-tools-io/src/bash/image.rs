//! Image-output detection for the Bash tool.
//!
//! When a command writes a base64-encoded image to stdout, the model receives an
//! image content block rather than raw base64 text. Two forms are detected:
//!
//! 1. **Data-URI prefix** — `data:image/<type>;base64,<data>` (Chrome DevTools,
//!    ImageMagick `-format` output, etc.)
//! 2. **Raw base64** — whitespace is stripped, the bytes are decoded, and the
//!    first few decoded bytes are matched against known image magic numbers.

use base64::Engine as _;

/// Maximum stdout byte length for which image detection is attempted.
/// Base64 of 5 MiB ≈ 6.7 MiB; 8 MiB gives comfortable headroom without risking
/// a costly decode of arbitrarily large non-image output.
const MAX_DETECTION_BYTES: usize = 8 * 1024 * 1024;

/// Data-URI prefixes for each supported image type.
const DATA_URI_PREFIXES: &[(&str, &str)] = &[
    ("data:image/png;base64,", "image/png"),
    ("data:image/jpeg;base64,", "image/jpeg"),
    ("data:image/gif;base64,", "image/gif"),
    ("data:image/webp;base64,", "image/webp"),
];

/// Check whether `stdout` is a base64-encoded image.
///
/// Returns `(media_type, base64_data)` on a match or `None` otherwise.
/// `base64_data` is whitespace-normalised and ready to embed in an image block.
///
/// Detection order:
/// 1. Data-URI prefix form.
/// 2. Raw base64 whose decoded bytes carry a recognised image magic number.
pub fn detect_image(stdout: &[u8]) -> Option<(&'static str, String)> {
    if stdout.len() > MAX_DETECTION_BYTES {
        return None;
    }

    let s = std::str::from_utf8(stdout).ok()?;
    let trimmed = s.trim();

    // 1. Data-URI form.
    for &(prefix, media_type) in DATA_URI_PREFIXES {
        if let Some(data) = trimmed.strip_prefix(prefix) {
            let normalised: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            if !normalised.is_empty() {
                return Some((media_type, normalised));
            }
        }
    }

    // 2. Raw base64: strip all whitespace, decode, sniff magic bytes.
    let compact: String = trimmed.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if !compact.is_empty() {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) {
            if let Some(media_type) = image_media_type_from_magic(&decoded) {
                return Some((media_type, compact));
            }
        }
    }

    None
}

/// Identify an image MIME type from its binary magic-byte header.
fn image_media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal 1×1 white PNG (valid, recognised by all image parsers).
    const TINY_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    #[test]
    fn detect_raw_png_base64() {
        let (media_type, data) = detect_image(TINY_PNG_B64.as_bytes()).expect("should detect PNG");
        assert_eq!(media_type, "image/png");
        assert_eq!(data, TINY_PNG_B64);
    }

    #[test]
    fn detect_png_with_trailing_newline() {
        let with_nl = format!("{TINY_PNG_B64}\n");
        let (media_type, _) = detect_image(with_nl.as_bytes()).expect("trailing newline ok");
        assert_eq!(media_type, "image/png");
    }

    #[test]
    fn detect_png_wrapped_at_76_chars() {
        // MIME-style line-wrapped base64 (ImageMagick `-compress none` output style).
        let wrapped: String = TINY_PNG_B64
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let (media_type, data) = detect_image(wrapped.as_bytes()).expect("wrapped base64 ok");
        assert_eq!(media_type, "image/png");
        // Whitespace is stripped in the returned data.
        assert!(!data.contains('\n'));
    }

    #[test]
    fn detect_data_uri_png() {
        let uri = format!("data:image/png;base64,{TINY_PNG_B64}");
        let (media_type, data) = detect_image(uri.as_bytes()).expect("data-URI PNG");
        assert_eq!(media_type, "image/png");
        assert_eq!(data, TINY_PNG_B64);
    }

    #[test]
    fn detect_data_uri_jpeg() {
        // Build a minimal JPEG-magic data-URI (3-byte JPEG header).
        let jpeg_bytes = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes);
        let uri = format!("data:image/jpeg;base64,{encoded}");
        let (media_type, _) = detect_image(uri.as_bytes()).expect("data-URI JPEG");
        assert_eq!(media_type, "image/jpeg");
    }

    #[test]
    fn detect_data_uri_gif() {
        let gif_bytes = b"GIF89a\x01\x00\x01\x00";
        let encoded = base64::engine::general_purpose::STANDARD.encode(gif_bytes);
        let uri = format!("data:image/gif;base64,{encoded}");
        let (media_type, _) = detect_image(uri.as_bytes()).expect("data-URI GIF");
        assert_eq!(media_type, "image/gif");
    }

    #[test]
    fn detect_data_uri_webp() {
        let mut webp = *b"RIFF    WEBP";
        webp[4] = 0;
        webp[5] = 0;
        webp[6] = 0;
        webp[7] = 0;
        let encoded = base64::engine::general_purpose::STANDARD.encode(webp);
        let uri = format!("data:image/webp;base64,{encoded}");
        let (media_type, _) = detect_image(uri.as_bytes()).expect("data-URI WebP");
        assert_eq!(media_type, "image/webp");
    }

    #[test]
    fn plain_text_returns_none() {
        assert!(detect_image(b"hello world\n").is_none());
    }

    #[test]
    fn non_image_base64_returns_none() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"this is just text data");
        assert!(detect_image(encoded.as_bytes()).is_none());
    }

    #[test]
    fn oversized_stdout_returns_none() {
        // MAX_DETECTION_BYTES + 1 must be rejected immediately.
        let big = vec![b'A'; MAX_DETECTION_BYTES + 1];
        assert!(detect_image(&big).is_none());
    }

    #[test]
    fn decoded_bytes_round_trip_correctly() {
        let (_, data) = detect_image(TINY_PNG_B64.as_bytes()).expect("detect");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .expect("valid base64");
        assert!(decoded.starts_with(b"\x89PNG\r\n\x1a\n"), "decoded must start with PNG magic");
    }

    #[test]
    fn magic_sniff_jpeg() {
        let bytes = [0xff, 0xd8, 0xff, 0xe0];
        assert_eq!(image_media_type_from_magic(&bytes), Some("image/jpeg"));
    }

    #[test]
    fn magic_sniff_gif87() {
        assert_eq!(
            image_media_type_from_magic(b"GIF87a\x01\x00"),
            Some("image/gif")
        );
    }

    #[test]
    fn magic_sniff_gif89() {
        assert_eq!(
            image_media_type_from_magic(b"GIF89a\x01\x00"),
            Some("image/gif")
        );
    }

    #[test]
    fn magic_sniff_webp() {
        let bytes = b"RIFF\x00\x00\x00\x00WEBP";
        assert_eq!(image_media_type_from_magic(bytes), Some("image/webp"));
    }

    #[test]
    fn magic_sniff_unknown_returns_none() {
        assert!(image_media_type_from_magic(b"BM\x00\x00").is_none()); // BMP not supported
    }
}
