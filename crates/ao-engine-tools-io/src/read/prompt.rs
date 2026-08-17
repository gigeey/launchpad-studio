//! Model-facing description text for the Read tool.
//!
//! Kept in its own module so the prompt string can be tuned independently of
//! tool behavior. This constant is the single source of truth for the tool's
//! `description()`, and a drift-guard test in `tests.rs` asserts
//! `Read::description() == DESCRIPTION`. Every capability claimed here must be
//! backed by the current `mod.rs` implementation.

pub const DESCRIPTION: &str = "Reads a file from the local filesystem. You can access any file directly by using this tool.
Assume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.

Usage:
- The file_path parameter must be an absolute path, not a relative path
- By default, it reads up to 2000 lines starting from the beginning of the file
- You can optionally specify a line offset and limit (especially handy for long files). offset is a 1-based line number matching the line numbers shown in the output; omitting it starts from line 1. It's recommended to read the whole file by not providing these parameters
- Results are returned using cat -n format, with line numbers starting at 1
- This tool can read images (PNG, JPEG, GIF, WebP). The image is returned to you as visual content you can interpret directly; offset and limit do not apply.
- This tool can read PDF files. Each PDF is returned as a document you can read, preceded by a short summary line.
- This tool reads Jupyter notebooks (.ipynb files) as a structured, cell-by-cell text view: each cell's source followed by any text outputs (stream text, execution results, and error tracebacks). Image outputs are noted as omitted rather than rendered.
- This tool can only read files, not directories. To enumerate the contents of a directory, use the Glob tool.
- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents.";
