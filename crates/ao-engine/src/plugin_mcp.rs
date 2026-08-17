//! Load and convert plugin-bundled MCP server definitions.
//!
//! Plugins may declare MCP servers in two places:
//!
//! 1. A `.mcp.json` file at the plugin root directory:
//!    ```json
//!    { "mcpServers": { "<name>": { "command": "...", "args": [...], "env": {...} } } }
//!    ```
//!    Remote (HTTP) servers use `{ "type": "http", "url": "..." }`.
//!
//! 2. An `mcpServers` field in the plugin manifest, with the same shape.
//!
//! The manifest field takes precedence; `.mcp.json` is the fallback.
//!
//! The resulting `McpServerEntry` values use `<plugin-name>:<server-name>` as
//! their name, ensuring they are distinct from user-configured entries (which
//! are restricted to `[a-z][a-z0-9_]*`) and cannot collide across plugins.

use std::collections::HashMap;
use std::path::Path;

use ao_engine_tools_provider_config::mcp_servers::{
    McpLoadingPolicy, McpServerEntry, McpTransportType,
};

/// Raw JSON shape for a single plugin-bundled MCP server definition.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginMcpServerDef {
    /// `"http"` for an HTTP server. Absent for stdio servers.
    #[serde(rename = "type", default)]
    pub transport_type: Option<String>,

    /// Command to launch for stdio servers.
    pub command: Option<String>,

    /// Arguments for the stdio command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables for the stdio command.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Endpoint URL for HTTP servers.
    pub url: Option<String>,
}

/// Top-level shape of a `.mcp.json` file.
#[derive(Debug, serde::Deserialize)]
struct McpFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: HashMap<String, PluginMcpServerDef>,
}

/// Replace `${VAR}` references in `s` with process environment values.
///
/// Unset variables are left unexpanded.
fn expand_env_vars(s: &str) -> String {
    let mut result = s.to_string();
    let mut pos = 0;
    while let Some(start) = result[pos..].find("${") {
        let abs_start = pos + start;
        if let Some(rel_end) = result[abs_start + 2..].find('}') {
            let abs_end = abs_start + 2 + rel_end;
            let var_name = &result[abs_start + 2..abs_end];
            if let Ok(val) = std::env::var(var_name) {
                result.replace_range(abs_start..=abs_end, &val);
                // Stay at abs_start in case the replacement itself contains `${`.
            } else {
                pos = abs_end + 1;
            }
        } else {
            break;
        }
    }
    result
}

fn expand_env_map(env: HashMap<String, String>) -> HashMap<String, String> {
    env.into_iter().map(|(k, v)| (k, expand_env_vars(&v))).collect()
}

/// Convert a `PluginMcpServerDef` to an `McpServerEntry`.
///
/// The entry name is `<plugin_name>:<server_name>`. The colon prefix ensures
/// plugin-sourced entries are distinct from user-configured entries (which
/// follow the `[a-z][a-z0-9_]*` constraint) and cannot collide across plugins.
///
/// Returns `None` when the definition is malformed or uses an unrecognized
/// transport type.
pub fn def_to_entry(
    plugin_name: &str,
    server_name: &str,
    def: PluginMcpServerDef,
) -> Option<McpServerEntry> {
    let qualified_name = format!("{plugin_name}:{server_name}");

    match def.transport_type.as_deref() {
        Some("http") => {
            let url = def.url.map(|u| expand_env_vars(&u))?;
            Some(McpServerEntry {
                name: qualified_name,
                command: None,
                args: vec![],
                env: HashMap::new(),
                loading: McpLoadingPolicy::Always,
                transport: McpTransportType::Http,
                url: Some(url),
                auth: None,
            })
        }
        None => {
            let command = match def.command {
                Some(c) => expand_env_vars(&c),
                None => {
                    tracing::warn!(
                        plugin = %plugin_name,
                        server = %server_name,
                        "stdio MCP server definition is missing 'command'; skipping"
                    );
                    return None;
                }
            };
            let args = def.args.into_iter().map(|a| expand_env_vars(&a)).collect();
            let env = expand_env_map(def.env);
            Some(McpServerEntry {
                name: qualified_name,
                command: Some(command),
                args,
                env,
                loading: McpLoadingPolicy::Always,
                transport: McpTransportType::Stdio,
                url: None,
                auth: None,
            })
        }
        Some(unknown) => {
            tracing::warn!(
                plugin = %plugin_name,
                server = %server_name,
                transport = %unknown,
                "unrecognized MCP transport type; skipping server definition"
            );
            None
        }
    }
}

/// Convert a raw `mcpServers` map (from the manifest or `.mcp.json`) to a
/// list of `McpServerEntry` values with collision-safe names.
pub fn convert_mcp_map(
    plugin_name: &str,
    servers: HashMap<String, PluginMcpServerDef>,
) -> Vec<McpServerEntry> {
    servers
        .into_iter()
        .filter_map(|(name, def)| def_to_entry(plugin_name, &name, def))
        .collect()
}

/// Load plugin MCP server entries from the installed plugin directory.
///
/// Checks `manifest_mcp` first (from the parsed plugin manifest); falls back
/// to reading `.mcp.json` from `plugin_dir`. Returns an empty list when
/// neither source defines any servers.
///
/// Parse errors are logged at `warn` and treated as empty, so a malformed
/// `.mcp.json` never prevents the plugin from loading.
pub fn load_plugin_mcp_entries(
    plugin_name: &str,
    plugin_dir: &Path,
    manifest_mcp: Option<HashMap<String, PluginMcpServerDef>>,
) -> Vec<McpServerEntry> {
    if let Some(servers) = manifest_mcp {
        if !servers.is_empty() {
            return convert_mcp_map(plugin_name, servers);
        }
    }

    let mcp_file = plugin_dir.join(".mcp.json");
    if !mcp_file.is_file() {
        return vec![];
    }

    let raw = match std::fs::read_to_string(&mcp_file) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(plugin = %plugin_name, "failed to read .mcp.json: {e}");
            return vec![];
        }
    };

    let parsed: McpFile = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(plugin = %plugin_name, "failed to parse .mcp.json: {e}");
            return vec![];
        }
    };

    convert_mcp_map(plugin_name, parsed.mcp_servers)
}

/// Collect MCP server entries for all installed plugins.
///
/// Reads the plugin registry, then for each plugin reads `.mcp.json` from its
/// installed directory. Returns `(plugin_name, entry)` pairs so the caller can
/// set the correct `source` label on the manager.
pub fn collect_all_plugin_mcp_entries() -> Vec<(String, McpServerEntry)> {
    let root = match crate::plugin_paths::plugins_root() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("cannot resolve plugins root for MCP loading: {e}");
            return vec![];
        }
    };

    let registry = match crate::plugin_registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("cannot load plugin registry for MCP loading: {e}");
            return vec![];
        }
    };

    let mut out = Vec::new();
    for plugin in &registry.entries {
        let plugin_dir = root.join(&plugin.name);
        let entries = load_plugin_mcp_entries(&plugin.name, &plugin_dir, None);
        for entry in entries {
            out.push((plugin.name.clone(), entry));
        }
    }
    out
}

/// Serialize an `mcpServers` map into the JSON string for a `.mcp.json` file.
///
/// Returns `None` when the map is empty (no file should be written).
pub fn serialize_mcp_json(
    servers: &HashMap<String, serde_json::Value>,
) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let wrapper = serde_json::json!({ "mcpServers": servers });
    serde_json::to_string_pretty(&wrapper).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_def_converts_with_plugin_prefix() {
        let mut env = HashMap::new();
        env.insert("TOKEN".to_string(), "secret".to_string());

        let def = PluginMcpServerDef {
            transport_type: None,
            command: Some("my-mcp-server".to_string()),
            args: vec!["--port".to_string(), "9000".to_string()],
            env,
            url: None,
        };

        let entry = def_to_entry("my-plugin", "my-server", def).unwrap();
        assert_eq!(entry.name, "my-plugin:my-server");
        assert_eq!(entry.command.as_deref(), Some("my-mcp-server"));
        assert_eq!(entry.args, vec!["--port", "9000"]);
        assert_eq!(entry.env.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(entry.transport, McpTransportType::Stdio);
        assert!(entry.url.is_none());
    }

    #[test]
    fn http_def_converts_with_plugin_prefix() {
        let def = PluginMcpServerDef {
            transport_type: Some("http".to_string()),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("https://example.com/mcp".to_string()),
        };

        let entry = def_to_entry("tools-plugin", "remote", def).unwrap();
        assert_eq!(entry.name, "tools-plugin:remote");
        assert_eq!(entry.transport, McpTransportType::Http);
        assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
        assert!(entry.command.is_none());
    }

    #[test]
    fn http_def_missing_url_returns_none() {
        let def = PluginMcpServerDef {
            transport_type: Some("http".to_string()),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
        };
        assert!(def_to_entry("p", "s", def).is_none());
    }

    #[test]
    fn stdio_def_missing_command_returns_none() {
        let def = PluginMcpServerDef {
            transport_type: None,
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
        };
        assert!(def_to_entry("p", "s", def).is_none());
    }

    #[test]
    fn unknown_transport_returns_none() {
        let def = PluginMcpServerDef {
            transport_type: Some("grpc".to_string()),
            command: Some("cmd".to_string()),
            args: vec![],
            env: HashMap::new(),
            url: None,
        };
        assert!(def_to_entry("p", "s", def).is_none());
    }

    #[test]
    fn load_plugin_mcp_entries_reads_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_content = serde_json::json!({
            "mcpServers": {
                "search": { "command": "search-mcp", "args": ["--fast"] },
                "remote": { "type": "http", "url": "https://api.example.com/mcp" }
            }
        });
        std::fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string(&mcp_content).unwrap(),
        )
        .unwrap();

        let entries = load_plugin_mcp_entries("awesome-plugin", dir.path(), None);
        assert_eq!(entries.len(), 2);

        let stdio = entries.iter().find(|e| e.name == "awesome-plugin:search");
        assert!(stdio.is_some(), "should have stdio server entry");
        let stdio = stdio.unwrap();
        assert_eq!(stdio.command.as_deref(), Some("search-mcp"));
        assert_eq!(stdio.args, vec!["--fast"]);

        let http = entries.iter().find(|e| e.name == "awesome-plugin:remote");
        assert!(http.is_some(), "should have http server entry");
        let http = http.unwrap();
        assert_eq!(http.transport, McpTransportType::Http);
        assert_eq!(http.url.as_deref(), Some("https://api.example.com/mcp"));
    }

    #[test]
    fn load_plugin_mcp_entries_manifest_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();

        // .mcp.json has "disk-server"
        let disk_content = serde_json::json!({
            "mcpServers": { "disk-server": { "command": "disk-mcp" } }
        });
        std::fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string(&disk_content).unwrap(),
        )
        .unwrap();

        // Manifest map has "manifest-server" (different)
        let mut manifest_servers = HashMap::new();
        manifest_servers.insert(
            "manifest-server".to_string(),
            PluginMcpServerDef {
                transport_type: None,
                command: Some("manifest-mcp".to_string()),
                args: vec![],
                env: HashMap::new(),
                url: None,
            },
        );

        let entries = load_plugin_mcp_entries("my-plugin", dir.path(), Some(manifest_servers));
        assert_eq!(entries.len(), 1, "manifest takes precedence over .mcp.json");
        assert_eq!(entries[0].name, "my-plugin:manifest-server");
        assert_eq!(entries[0].command.as_deref(), Some("manifest-mcp"));
    }

    #[test]
    fn load_plugin_mcp_entries_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let entries = load_plugin_mcp_entries("no-mcp-plugin", dir.path(), None);
        assert!(entries.is_empty());
    }

    #[test]
    fn env_var_expansion_replaces_set_vars() {
        std::env::set_var("TEST_PLUGIN_MCP_VAR", "expanded_value");
        let result = expand_env_vars("prefix_${TEST_PLUGIN_MCP_VAR}_suffix");
        assert_eq!(result, "prefix_expanded_value_suffix");
        std::env::remove_var("TEST_PLUGIN_MCP_VAR");
    }

    #[test]
    fn env_var_expansion_leaves_unset_vars_intact() {
        std::env::remove_var("DEFINITELY_NOT_SET_PLUGIN_MCP_XYZ");
        let result = expand_env_vars("value_${DEFINITELY_NOT_SET_PLUGIN_MCP_XYZ}_end");
        assert_eq!(result, "value_${DEFINITELY_NOT_SET_PLUGIN_MCP_XYZ}_end");
    }

    #[test]
    fn name_prefix_uses_colon_separator() {
        let def = PluginMcpServerDef {
            transport_type: None,
            command: Some("cmd".to_string()),
            args: vec![],
            env: HashMap::new(),
            url: None,
        };
        let entry = def_to_entry("my-plugin", "github", def).unwrap();
        assert_eq!(entry.name, "my-plugin:github");
        assert!(
            entry.name.contains(':'),
            "name must contain colon as separator"
        );
    }
}
