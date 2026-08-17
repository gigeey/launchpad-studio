/// Verbatim description returned by [`NotebookEdit::description`](super::NotebookEdit::description).
pub const DESCRIPTION: &str =
    "Edits Jupyter notebook (.ipynb) files via three modes: replace, insert, and delete.

Usage:
- This tool only edits files ending in .ipynb. For all other file types use the Edit tool.
- Before editing you must Read the notebook first (except when using insert mode without \
cell_id to append at the end — that sub-mode bypasses the read gate).
- edit_mode values and requirements:
  - 'replace': requires both cell_id and new_source. Overwrites the source of the \
identified cell. Optionally supply cell_type to change the cell's type at the same time.
  - 'insert': requires new_source and cell_type. If cell_id is supplied the new cell is \
inserted *before* that cell (requires a prior Read). If cell_id is omitted the new cell \
is appended to the end of the notebook (Read gate bypassed, but the file must exist).
  - 'delete': requires cell_id only. Forbids new_source and cell_type. Requires a prior Read.
- cell_id accepts either a 0-based numeric index (e.g. '0', '2') or a cell-id string \
matching the 'id' field stored in the notebook JSON.
- Do not use Write or Edit on .ipynb paths — those tools corrupt the notebook JSON. \
Use NotebookEdit instead.";

/// JSON Schema for the `NotebookEdit` tool's input parameters.
pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "notebook_path": {
      "type": "string",
      "description": "Absolute path to the .ipynb notebook file to edit."
    },
    "cell_id": {
      "type": "string",
      "description": "Target cell: a 0-based numeric index or the cell's id string. Optional for insert mode (omit to append at end); required for replace and delete modes."
    },
    "new_source": {
      "type": "string",
      "description": "New source text for the cell. Required for replace and insert modes; forbidden for delete mode."
    },
    "cell_type": {
      "type": "string",
      "enum": ["code", "markdown"],
      "description": "Cell type. Required for insert mode. Optional for replace mode (supply only when changing the cell type)."
    },
    "edit_mode": {
      "type": "string",
      "enum": ["replace", "insert", "delete"],
      "description": "The edit operation to perform."
    }
  },
  "required": ["notebook_path", "edit_mode"],
  "additionalProperties": false
}"#;
