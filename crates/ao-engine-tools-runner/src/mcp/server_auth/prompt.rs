pub(super) const DESCRIPTION: &str = "Authorize a connection to an MCP server that requires OAuth authentication. \
Call this tool when you need access to a server whose tools are unavailable because authorization is required. \
Returns an authorization URL that the user must open to grant access. After the user completes authorization, \
the server's real tools will become available automatically on the next turn.";
