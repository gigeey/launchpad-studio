/// Verbatim description returned by [`Edit::description`](super::Edit::description).
pub const DESCRIPTION: &str = "Performs exact string replacements in files.

Usage:
- You must use the `Read` tool at least once before editing a file. This tool \
will error if the file has not been read in the current session.
- Prefer editing existing files over creating new ones. Only use the \
new-file path (empty `old_string` on a non-existent path) when genuinely \
adding a file that does not yet exist.
- The `old_string` and `new_string` values must contain ONLY real file \
content. Never include the line-number prefixes (e.g. `\\t1\\t`) that the \
Read tool prepends to its output — those are display annotations, not part \
of the file.
- Provide enough surrounding context in `old_string` to make it unique in the \
file. The edit fails if `old_string` appears more than once unless `replace_all` \
is set to true.
- Use `replace_all: true` to replace every occurrence when the same string \
appears multiple times.
- Pass `old_string: \"\"` on a non-existent path to create a new file with \
`new_string` as its content.
- This tool cannot edit Jupyter notebooks (.ipynb); use the `NotebookEdit` tool \
instead.
- Files larger than 1 GiB are refused to prevent out-of-memory errors.";

/// JSON Schema for the `Edit` tool's input parameters.
pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "file_path": {
      "type": "string",
      "description": "Absolute path to the file to edit."
    },
    "old_string": {
      "type": "string",
      "description": "The exact text to find and replace. Pass an empty string with a non-existent file_path to create a new file."
    },
    "new_string": {
      "type": "string",
      "description": "The replacement text."
    },
    "replace_all": {
      "type": "boolean",
      "default": false,
      "description": "Replace all occurrences of old_string. When false (default) the edit fails if old_string appears more than once."
    }
  },
  "required": ["file_path", "old_string", "new_string"],
  "additionalProperties": false
}"#;
