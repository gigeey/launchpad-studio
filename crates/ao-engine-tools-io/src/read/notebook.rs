//! Jupyter notebook (.ipynb) rendering for the Read tool.
//!
//! Parses a notebook JSON document and produces a structured, human-readable
//! plain-text representation of its cells. Each cell is preceded by a header
//! showing its 1-based index and `cell_type`. Code cells include any text
//! outputs: stream text, `execute_result`/`display_data` `text/plain` values,
//! and error tracebacks with ANSI codes stripped. Image outputs are replaced by
//! the annotation `[image output omitted]` — multimodal handling is a separate
//! future task.

use serde_json::Value;

/// Parse and render a Jupyter notebook from raw bytes.
///
/// Returns the rendered text on success, or an error string describing why the
/// bytes could not be interpreted as a valid notebook.
pub fn render(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| format!("notebook bytes are not valid UTF-8: {e}"))?;
    let root: Value =
        serde_json::from_str(text).map_err(|e| format!("notebook JSON is malformed: {e}"))?;

    let cells = match root.get("cells").and_then(Value::as_array) {
        Some(c) => c,
        None => return Err("notebook JSON is missing the 'cells' array".to_string()),
    };

    let mut out = String::new();

    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("--- Cell {} [{}] ---\n", idx + 1, cell_type));

        let source = join_source(cell.get("source"));
        out.push_str(&source);
        if !source.is_empty() && !source.ends_with('\n') {
            out.push('\n');
        }

        if cell_type == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(Value::as_array) {
                for output in outputs {
                    render_output(output, &mut out);
                }
            }
        }
    }

    Ok(out)
}

/// Append the rendered form of one output entry to `buf`.
fn render_output(output: &Value, buf: &mut String) {
    let output_type = output
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("");

    match output_type {
        "stream" => {
            let text = join_source(output.get("text"));
            if !text.is_empty() {
                buf.push_str("\nOutput:\n");
                buf.push_str(&text);
                if !text.ends_with('\n') {
                    buf.push('\n');
                }
            }
        }
        "execute_result" | "display_data" => {
            if let Some(data) = output.get("data").and_then(Value::as_object) {
                if data.keys().any(|k| k.starts_with("image/")) {
                    buf.push_str("\n[image output omitted]\n");
                    return;
                }
                let plain = data
                    .get("text/plain")
                    .map(|v| join_source(Some(v)))
                    .unwrap_or_default();
                if !plain.is_empty() {
                    buf.push_str("\nOutput:\n");
                    buf.push_str(&plain);
                    if !plain.ends_with('\n') {
                        buf.push('\n');
                    }
                }
            }
        }
        "error" => {
            let ename = output
                .get("ename")
                .and_then(Value::as_str)
                .unwrap_or("Error");
            let evalue = output.get("evalue").and_then(Value::as_str).unwrap_or("");
            buf.push_str(&format!("\nError: {ename}: {evalue}\n"));
            if let Some(tb) = output.get("traceback").and_then(Value::as_array) {
                for line in tb {
                    let raw = line.as_str().unwrap_or("");
                    buf.push_str(&strip_ansi(raw));
                    buf.push('\n');
                }
            }
        }
        _ => {}
    }
}

/// Join a Jupyter `source` field: a JSON array of strings or a plain string.
fn join_source(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::String(s)) => s.clone(),
        Some(_) => String::new(),
    }
}

/// Remove ANSI CSI escape sequences from `s` (e.g. colour codes in error tracebacks).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("fixtures/sample.ipynb");

    #[test]
    fn renders_sample_notebook_to_readable_text() {
        let rendered = render(SAMPLE.as_bytes()).expect("sample notebook must parse");

        assert!(
            rendered.contains("--- Cell 1 [markdown] ---"),
            "cell 1 header missing"
        );
        assert!(
            rendered.contains("--- Cell 2 [code] ---"),
            "cell 2 header missing"
        );
        assert!(
            rendered.contains("--- Cell 3 [code] ---"),
            "cell 3 header missing"
        );
        assert!(
            rendered.contains("--- Cell 4 [code] ---"),
            "cell 4 header missing"
        );
        assert!(
            rendered.contains("--- Cell 5 [code] ---"),
            "cell 5 header missing"
        );

        assert!(
            rendered.contains("# Sample Notebook"),
            "markdown heading missing"
        );
        assert!(rendered.contains("x = 6 * 7"), "code source missing");
        assert!(rendered.contains("42"), "stream output value missing");
        assert!(rendered.contains("43"), "execute_result value missing");
        assert!(rendered.contains("ZeroDivisionError"), "error type missing");
        assert!(
            rendered.contains("division by zero"),
            "error message missing"
        );
        assert!(
            rendered.contains("[image output omitted]"),
            "image annotation missing"
        );

        assert!(
            !rendered.contains("\"cell_type\""),
            "raw JSON keys must not appear"
        );
    }

    #[test]
    fn malformed_json_returns_error() {
        let result = render(b"{ not json {{{");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("malformed"),
            "error must mention malformed: {msg:?}"
        );
    }

    #[test]
    fn missing_cells_array_returns_error() {
        let result = render(br#"{"nbformat": 4, "metadata": {}}"#);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("cells"), "error must mention 'cells': {msg:?}");
    }

    #[test]
    fn non_utf8_bytes_return_error() {
        let result = render(b"\xff\xfe not utf8");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("UTF-8"), "error must mention UTF-8: {msg:?}");
    }

    #[test]
    fn empty_cells_array_produces_empty_output() {
        let nb = serde_json::json!({"nbformat": 4, "cells": []});
        let rendered = render(nb.to_string().as_bytes()).expect("must parse");
        assert!(rendered.is_empty());
    }

    #[test]
    fn image_output_is_annotated_and_data_omitted() {
        let nb = serde_json::json!({
            "nbformat": 4,
            "cells": [{
                "cell_type": "code",
                "source": ["plt.plot([1,2,3])"],
                "outputs": [{
                    "output_type": "display_data",
                    "data": {
                        "image/png": "base64rawdata==",
                        "text/plain": ["<Figure>"]
                    },
                    "metadata": {}
                }]
            }]
        });
        let rendered = render(nb.to_string().as_bytes()).expect("must parse");
        assert!(
            rendered.contains("[image output omitted]"),
            "image must be annotated"
        );
        assert!(
            !rendered.contains("base64rawdata"),
            "raw image data must not appear"
        );
        assert!(
            !rendered.contains("<Figure>"),
            "text/plain must be suppressed when image present"
        );
    }

    #[test]
    fn execute_result_text_plain_is_shown() {
        let nb = serde_json::json!({
            "nbformat": 4,
            "cells": [{
                "cell_type": "code",
                "source": ["2 + 2"],
                "outputs": [{
                    "output_type": "execute_result",
                    "execution_count": 1,
                    "data": {"text/plain": ["4"]},
                    "metadata": {}
                }]
            }]
        });
        let rendered = render(nb.to_string().as_bytes()).expect("must parse");
        assert!(rendered.contains("Output:"), "output section missing");
        assert!(rendered.contains('4'), "result value missing");
    }

    #[test]
    fn source_as_single_string_is_handled() {
        let nb = serde_json::json!({
            "nbformat": 4,
            "cells": [{"cell_type": "markdown", "source": "# Title\n\nBody text."}]
        });
        let rendered = render(nb.to_string().as_bytes()).expect("must parse");
        assert!(rendered.contains("# Title"), "title missing");
        assert!(rendered.contains("Body text."), "body missing");
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        let coloured = "\x1b[0;31mZeroDivisionError\x1b[0m: division by zero";
        assert_eq!(strip_ansi(coloured), "ZeroDivisionError: division by zero");
    }

    #[test]
    fn strip_ansi_leaves_plain_text_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_handles_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }
}
