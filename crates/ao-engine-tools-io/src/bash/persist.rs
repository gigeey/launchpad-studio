//! Large-output disk persistence for the Bash tool.
//!
//! When combined stdout+stderr exceeds the persistence threshold, the full
//! untruncated output is written to a file under `{data_root}/bash-output/`
//! and the caller returns a compact `<persisted-output>` envelope instead of
//! the raw bytes. The model can read the complete output on demand using the
//! Read tool.

use std::path::PathBuf;

use ao_protocol::{data_root::resolve_data_root, error::AoError};

/// Number of lines included in the preview head and tail sections.
const PREVIEW_HEAD_LINES: usize = 20;
const PREVIEW_TAIL_LINES: usize = 20;

/// Result of persisting large bash output to disk.
pub struct PersistedOutput {
    /// Absolute path to the file containing the full output.
    pub path: PathBuf,
    /// Total byte size of the persisted file.
    pub size: u64,
    /// Total number of lines in the file.
    pub lines: u64,
    /// Ready-to-return envelope string for inline model consumption.
    pub envelope: String,
}

/// Write combined bash output to a unique file under `{data_root}/bash-output/`
/// and return a [`PersistedOutput`] with path, size, line count, and a compact
/// head/tail preview.
///
/// The on-disk format mirrors the inline text rendering: stdout verbatim, then
/// stderr lines prefixed with `stderr: `. This keeps the file human-readable
/// and consistent with what the model would have seen had the output been small
/// enough for inline delivery.
pub async fn write_output(stdout: &[u8], stderr: &[u8]) -> Result<PersistedOutput, AoError> {
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(stdout));
    for line in String::from_utf8_lossy(stderr).lines() {
        combined.push_str("stderr: ");
        combined.push_str(line);
        combined.push('\n');
    }

    let data_root = resolve_data_root()?;
    let output_dir = data_root.join("bash-output");
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(AoError::Io)?;

    let id = uuid::Uuid::new_v4();
    let path = output_dir.join(format!("bash-{id}.txt"));

    tokio::fs::write(&path, combined.as_bytes())
        .await
        .map_err(AoError::Io)?;

    let size = combined.len() as u64;
    let all_lines: Vec<&str> = combined.lines().collect();
    let lines = all_lines.len() as u64;

    let envelope = build_envelope(&path, size, lines, &all_lines);

    Ok(PersistedOutput {
        path,
        size,
        lines,
        envelope,
    })
}

fn build_envelope(path: &PathBuf, size: u64, lines: u64, all_lines: &[&str]) -> String {
    let mut preview = String::new();

    let head_count = PREVIEW_HEAD_LINES.min(all_lines.len());
    if head_count > 0 {
        preview.push_str(&format!("--- head ({head_count} lines) ---\n"));
        for line in &all_lines[..head_count] {
            preview.push_str(line);
            preview.push('\n');
        }
    }

    if all_lines.len() > PREVIEW_HEAD_LINES {
        let tail_start = if all_lines.len() > PREVIEW_HEAD_LINES + PREVIEW_TAIL_LINES {
            all_lines.len() - PREVIEW_TAIL_LINES
        } else {
            PREVIEW_HEAD_LINES
        };
        let tail_count = all_lines.len() - tail_start;
        if tail_count > 0 {
            preview.push_str(&format!("--- tail ({tail_count} lines) ---\n"));
            for line in &all_lines[tail_start..] {
                preview.push_str(line);
                preview.push('\n');
            }
        }
    }

    format!(
        "<persisted-output filepath=\"{}\" size=\"{} bytes\" lines=\"{}\">\n{}</persisted-output>",
        path.display(),
        size,
        lines,
        preview
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_contains_key_fields() {
        let path = PathBuf::from("/tmp/bash-test.txt");
        let lines_data = (1..=50u32).map(|i| format!("line{i}")).collect::<Vec<_>>();
        let all_lines: Vec<&str> = lines_data.iter().map(|s| s.as_str()).collect();
        let env = build_envelope(&path, 1000, 50, &all_lines);
        assert!(env.contains("/tmp/bash-test.txt"), "filepath missing");
        assert!(env.contains("1000 bytes"), "size missing");
        assert!(env.contains("lines=\"50\""), "line count missing");
        assert!(env.contains("<persisted-output"), "opening tag missing");
        assert!(env.contains("</persisted-output>"), "closing tag missing");
    }

    #[test]
    fn envelope_head_shows_first_lines() {
        let path = PathBuf::from("/tmp/x.txt");
        let lines_data = (1..=60u32).map(|i| format!("line{i}")).collect::<Vec<_>>();
        let all_lines: Vec<&str> = lines_data.iter().map(|s| s.as_str()).collect();
        let env = build_envelope(&path, 500, 60, &all_lines);
        assert!(env.contains("line1"), "first line missing from head");
        assert!(env.contains("line20"), "20th line missing from head");
        assert!(!env.contains("line21\n"), "line 21 should not appear in head");
    }

    #[test]
    fn envelope_tail_shows_last_lines() {
        let path = PathBuf::from("/tmp/x.txt");
        let lines_data = (1..=60u32).map(|i| format!("line{i}")).collect::<Vec<_>>();
        let all_lines: Vec<&str> = lines_data.iter().map(|s| s.as_str()).collect();
        let env = build_envelope(&path, 500, 60, &all_lines);
        assert!(env.contains("line60"), "last line missing from tail");
        assert!(env.contains("line41"), "41st line missing from tail");
    }

    #[test]
    fn envelope_short_output_no_tail_section() {
        let path = PathBuf::from("/tmp/x.txt");
        let lines_data = vec!["a", "b", "c"];
        let env = build_envelope(&path, 6, 3, &lines_data);
        assert!(!env.contains("--- tail"), "no tail for short output");
    }
}
