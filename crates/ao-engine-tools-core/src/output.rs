use serde::{Deserialize, Serialize};

/// A single typed content block a tool can return as part of a
/// [`ToolOutput::Blocks`] response.
///
/// This is the provider-neutral substrate for multimodal tool results. A tool
/// constructs these; the runner maps them to provider-specific wire shapes
/// (Anthropic `image`/`document` blocks, OpenAI content parts, Gemini
/// `inlineData`). Tools never build provider-specific JSON themselves.
///
/// Only the subset of media a tool can meaningfully produce today is modelled:
/// plain text, a base64-encoded image, and a base64-encoded document (PDF).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum ToolBlock {
    /// Plain text content.
    Text { text: String },
    /// A base64-encoded image. `media_type` is an image MIME type such as
    /// `image/png`, `image/jpeg`, `image/gif`, or `image/webp`.
    Image { media_type: String, data: String },
    /// A base64-encoded document. `media_type` is `application/pdf` for the
    /// only document kind supported today. `title` is an optional human label
    /// some providers surface in their UI; others ignore it.
    Document {
        media_type: String,
        data: String,
        title: Option<String>,
    },
}

impl ToolBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn document(
        media_type: impl Into<String>,
        data: impl Into<String>,
        title: Option<String>,
    ) -> Self {
        Self::Document {
            media_type: media_type.into(),
            data: data.into(),
            title,
        }
    }

    /// A short textual summary of the block, used for logging and for the
    /// text-only dialect adapter. Binary payloads are described, not inlined.
    fn summary(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Image { media_type, data } => {
                format!("[image: {media_type}, {} base64 chars]", data.len())
            }
            Self::Document {
                media_type,
                data,
                title,
            } => {
                let label = title.as_deref().unwrap_or("untitled");
                format!(
                    "[document: {media_type}, {label}, {} base64 chars]",
                    data.len()
                )
            }
        }
    }
}

/// Result of invoking a tool. The dispatcher serialises this back to the
/// model — `Text`, `Structured`, and `Blocks` are success-shaped, `Error` is
/// the recoverable error channel (the tool ran and decided "no").
///
/// Hard failures (panics, cancellation, schema-violating input) propagate as
/// `Result::Err` from `invoke` and are not represented here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolOutput {
    Text(String),
    Structured(serde_json::Value),
    Error {
        message: String,
        /// `true` if the model can reasonably retry with different input.
        /// `false` for permission denials, validation, or "the world said no".
        recoverable: bool,
    },
    /// One or more mixed content blocks (text + image + document). Use when the
    /// result is not purely textual — e.g. an image read returns a single
    /// [`ToolBlock::Image`], and a PDF read returns a text summary followed by
    /// a [`ToolBlock::Document`]. The runner maps each block to the active
    /// provider's wire shape.
    Blocks(Vec<ToolBlock>),
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    pub fn structured(v: serde_json::Value) -> Self {
        Self::Structured(v)
    }

    pub fn error(message: impl Into<String>, recoverable: bool) -> Self {
        Self::Error {
            message: message.into(),
            recoverable,
        }
    }

    /// One or more content blocks (the multimodal channel).
    pub fn blocks(blocks: Vec<ToolBlock>) -> Self {
        Self::Blocks(blocks)
    }

    /// A single base64-encoded image result.
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Blocks(vec![ToolBlock::image(media_type, data)])
    }

    /// A base64-encoded document result, optionally preceded by a text summary.
    pub fn document(
        media_type: impl Into<String>,
        data: impl Into<String>,
        title: Option<String>,
        summary: Option<String>,
    ) -> Self {
        let mut blocks = Vec::with_capacity(2);
        if let Some(s) = summary {
            blocks.push(ToolBlock::text(s));
        }
        blocks.push(ToolBlock::document(media_type, data, title));
        Self::Blocks(blocks)
    }

    /// Render a [`Self::Structured`] payload to the plain string the model (or
    /// a transcript) should read.
    ///
    /// A structured payload may carry a `text_fallback` string field holding a
    /// compact, token-efficient rendering of itself (Glob does this: its
    /// `text_fallback` is byte-identical to the plain file list a text-only
    /// caller would receive). When present, that field is what reaches the
    /// model, so it does not have to parse a JSON blob to act on the result.
    /// Payloads without `text_fallback` (e.g. structured Task or Config
    /// returns) fall back to their compact JSON serialization.
    ///
    /// Every transport that turns a structured tool result into model-facing
    /// text routes through here so the rendering cannot drift between the
    /// native, XML, MCP, and CLI paths.
    pub fn structured_to_text(v: &serde_json::Value) -> String {
        match v.get("text_fallback").and_then(|f| f.as_str()) {
            Some(s) => s.to_string(),
            None => v.to_string(),
        }
    }

    /// Render the output as a plain string suitable for inlining into a
    /// transcript or for the XML/CLI dialect adapter.
    /// For [`Self::Blocks`], binary payloads are summarised, not inlined.
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Structured(v) => Self::structured_to_text(v),
            Self::Error { message, .. } => format!("error: {message}"),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(ToolBlock::summary)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let out = ToolOutput::text("hello");
        let s = serde_json::to_string(&out).unwrap();
        let back: ToolOutput = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ToolOutput::Text(t) if t == "hello"));
    }

    #[test]
    fn as_text_handles_each_variant() {
        assert_eq!(ToolOutput::text("x").as_text(), "x");
        assert_eq!(
            ToolOutput::structured(serde_json::json!({"a": 1})).as_text(),
            r#"{"a":1}"#
        );
        assert_eq!(
            ToolOutput::error("nope", false).as_text(),
            "error: nope"
        );
    }

    #[test]
    fn structured_to_text_prefers_text_fallback_field() {
        let v = serde_json::json!({
            "matches": [{ "path": "src/a.rs", "mtime_unix": 1 }],
            "text_fallback": "src/a.rs\nsrc/b.rs",
        });
        // The model sees the compact list, not the JSON blob.
        assert_eq!(ToolOutput::structured_to_text(&v), "src/a.rs\nsrc/b.rs");
        assert_eq!(ToolOutput::structured(v).as_text(), "src/a.rs\nsrc/b.rs");
    }

    #[test]
    fn structured_to_text_falls_back_to_json_without_text_fallback() {
        let v = serde_json::json!({ "background_agent_id": "abc", "status": "running" });
        // No text_fallback field → compact JSON, unchanged behavior.
        assert_eq!(ToolOutput::structured_to_text(&v), v.to_string());
    }

    #[test]
    fn structured_to_text_ignores_non_string_text_fallback() {
        // A non-string text_fallback is not a valid rendering; fall back to JSON.
        let v = serde_json::json!({ "text_fallback": 42, "x": 1 });
        assert_eq!(ToolOutput::structured_to_text(&v), v.to_string());
    }

    #[test]
    fn image_helper_builds_single_image_block() {
        let out = ToolOutput::image("image/png", "AAAA");
        match out {
            ToolOutput::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(
                    blocks[0],
                    ToolBlock::Image {
                        media_type: "image/png".into(),
                        data: "AAAA".into()
                    }
                );
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn document_helper_prepends_summary_then_document() {
        let out = ToolOutput::document(
            "application/pdf",
            "JVBER",
            Some("report".into()),
            Some("PDF read: report.pdf".into()),
        );
        match out {
            ToolOutput::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(&blocks[0], ToolBlock::Text { text } if text == "PDF read: report.pdf"));
                assert!(matches!(
                    &blocks[1],
                    ToolBlock::Document { media_type, title, .. }
                        if media_type == "application/pdf" && title.as_deref() == Some("report")
                ));
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn as_text_summarises_blocks_without_inlining_payload() {
        let out = ToolOutput::Blocks(vec![
            ToolBlock::text("here is the screenshot"),
            ToolBlock::image("image/png", "QUJDRA=="),
        ]);
        let rendered = out.as_text();
        assert!(rendered.contains("here is the screenshot"));
        assert!(rendered.contains("[image: image/png"));
        // The raw base64 payload must not be inlined into the text rendering.
        assert!(!rendered.contains("QUJDRA=="));
    }

    #[test]
    fn blocks_round_trip_through_json() {
        let out = ToolOutput::Blocks(vec![
            ToolBlock::text("caption"),
            ToolBlock::image("image/jpeg", "Zm9v"),
            ToolBlock::document("application/pdf", "YmFy", Some("t".into())),
        ]);
        let s = serde_json::to_string(&out).unwrap();
        let back: ToolOutput = serde_json::from_str(&s).unwrap();
        match back {
            ToolOutput::Blocks(blocks) => assert_eq!(blocks.len(), 3),
            other => panic!("expected Blocks, got {other:?}"),
        }
    }
}
