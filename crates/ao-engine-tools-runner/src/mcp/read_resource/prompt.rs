//! Model-facing description string for the ReadMcpResource tool.
//!
//! Kept separate from `mod.rs` so the prompt text can be tuned independently
//! of tool behavior.  Every capability described here must be backed by the
//! current `mod.rs` implementation.

/// Description surfaced to the model as the tool's `description` field.
pub const DESCRIPTION: &str = "Fetch the content of a resource from a specific MCP server by URI.

MCP servers can expose file-like resources — documents, configuration files, logs, datasets — that the model can read directly. Use ListMcpResources first to discover available URIs, then call this tool to retrieve the actual content.

Parameters:
- server (required): the name of the MCP server that owns the resource.
- uri (required): the resource URI exactly as returned by ListMcpResources.

Output is a JSON string representing the resource contents array. Each element carries 'uri' and 'text'. For text-based resources, 'text' contains the content directly. For binary resources (PDFs, images, archives), 'text' contains a note describing where the file was saved on disk — the binary data is decoded from base64 and written to the local data directory so the model can reference it by path.

Error handling:
- Unknown server: returns an error listing the configured server names.
- Server lacks resource support: returns an error when the server did not advertise the 'resources' capability during initialization.
- Connection lost: attempts one reconnect before surfacing a 'not connected' error.

Output cap: results are limited to approximately 100 000 characters. An explicit truncation note is appended when the cap is reached.";
