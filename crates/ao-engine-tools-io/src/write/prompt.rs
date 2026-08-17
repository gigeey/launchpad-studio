/// Verbatim description returned by [`Write::description`](super::Write::description).
pub const DESCRIPTION: &str = "Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, you MUST use the `Read` tool first to read the \
file's contents. This tool will fail if you did not read the file first.
- Prefer the `Edit` tool for modifying existing files — it only sends the diff. \
Only use this tool to create new files or for complete rewrites.
- This tool cannot write Jupyter notebooks (.ipynb); use the `NotebookEdit` tool \
instead.
- Files larger than 1 GiB are refused to prevent out-of-memory errors.
- File content is written verbatim — no line-ending conversion is performed.
- Do not create documentation files (e.g. `*.md`) or README files unless the \
user has explicitly asked for one.
- Only include emojis in file content when the user has explicitly requested \
them; otherwise write emoji-free text.";

/// JSON Schema for the `Write` tool's input parameters.
pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "file_path": {
      "type": "string",
      "description": "Absolute path to the file to write."
    },
    "content": {
      "type": "string",
      "description": "The content to write to the file."
    }
  },
  "required": ["file_path", "content"],
  "additionalProperties": false
}"#;
