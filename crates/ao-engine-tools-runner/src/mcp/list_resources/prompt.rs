//! Model-facing description string for the ListMcpResources tool.
//!
//! Kept separate from `mod.rs` so the prompt text can be tuned independently
//! of tool behavior.  Every capability described here must be backed by the
//! current `mod.rs` implementation.

/// Description surfaced to the model as the tool's `description` field.
pub const DESCRIPTION: &str = "List the resources available on one or all configured MCP servers.

MCP servers can expose file-like resources — documents, datasets, configuration files, logs — alongside their tools. Use this tool to discover what resources are available before reading their contents with ReadMcpResource.

Parameters:
- server (optional): restrict the listing to a single named MCP server. When omitted, every server that advertised resource support during initialization is queried and their results are returned together, each entry tagged with its originating server name.

Output is a JSON array where each entry contains at minimum 'uri' and 'server'. Optional fields 'name', 'description', and 'mimeType' are included when the server provided them. When no resources are found, a plain-text explanation is returned instead.

Notes:
- Only servers that declared the 'resources' capability during their initial handshake are queried; servers without that capability are silently skipped when no server filter is given.
- Results may be truncated for servers with very large resource catalogs; an explicit truncation note is added when the cap is reached.
- Use ReadMcpResource to fetch the actual content of a specific URI.";
