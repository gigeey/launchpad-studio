// ---------------------------------------------------------------------------
// TypeScript types for the plugin Tauri command bridge.
//
// These mirror the DTOs in `frontend/src-tauri/src/plugin_commands.rs`.
// They are hand-maintained — when the Rust side changes, update here too.
// ---------------------------------------------------------------------------

/** Where a plugin lives. The `value` matches the original Rust enum's payload:
 *  a URL string for `github_url`, or an absolute filesystem path for `local_path`. */
export type PluginSource =
  | { type: "github_url"; value: string }
  | { type: "local_path"; value: string };

/** Argument shape for `install_plugin`. Mirrors `PluginSource` — installing
 *  always starts from one of these two source kinds. */
export type PluginInstallSource = PluginSource;

/** Which manifest convention located the plugin's metadata at install time. */
export type ManifestLocationKind =
  | "launchpad_native"
  | "override"
  | "claude_code"
  | "auto_discovered";

/** A single registered plugin (returned by `list_plugins`). */
export interface PluginEntry {
  name: string;
  version: string;
  source: PluginSource;
  /** RFC3339 timestamp. */
  installed_at: string;
  /** RFC3339 timestamp; updated by `refresh_plugin` and the auto-update tick. */
  last_updated_at: string;
  auto_update_enabled: boolean;
  manifest_location: ManifestLocationKind;
}

/** Returned by `install_plugin`. */
export interface InstallOutcome {
  name: string;
  version: string;
  /** Absolute path to the new plugin directory under the global plugin store. */
  plugin_dir: string;
  manifest_location: ManifestLocationKind;
  skills_installed: number;
  rules_installed: number;
}

/** Returned by `uninstall_plugin`. Both bools may be false on a no-op
 *  uninstall (plugin already gone from both registry and disk). */
export interface UninstallOutcome {
  directory_removed: boolean;
  registry_entry_removed: boolean;
}

/** Returned by `refresh_plugin`. */
export interface RefreshOutcome {
  name: string;
  version: string;
  plugin_dir: string;
  manifest_location: ManifestLocationKind;
  skills_installed: number;
  rules_installed: number;
  /** RFC3339 timestamp; same value as the entry's `last_updated_at` after refresh. */
  last_updated_at: string;
}

/** A skill from the global plugin catalog (returned by `list_global_skills`). */
export interface PluginSkill {
  /** Prefix-namespaced id: `<plugin>/<skill>` (e.g. `superpowers/tdd`). */
  id: string;
  plugin_name: string;
  skill_name: string;
  /** Absolute path to the skill folder on disk. */
  skill_dir: string;
  /** Absolute path to the skill's `SKILL.md` file. */
  skill_md: string;
}

/** A rule from the global plugin catalog (returned by `list_global_rules`). */
export interface PluginRule {
  /** Prefix-namespaced id: `<plugin>/<rule>` (extension stripped). */
  id: string;
  plugin_name: string;
  rule_name: string;
  /** Absolute path to the `.md` rule file on disk. */
  rule_file: string;
}

/** Tagged error variant returned by every plugin command on failure.
 *  Tauri serializes `Err(...)` directly, so awaited `invoke(...)` calls reject
 *  with one of these objects. The UI should `switch (err.type)` to discriminate. */
export type PluginCommandError =
  | { type: "conflict"; detail: string }
  | { type: "not_found"; detail: string }
  | { type: "manifest_invalid"; detail: string }
  | { type: "nothing_to_install" }
  | { type: "manifest_missing" }
  | { type: "unsafe_name"; detail: string }
  | { type: "source_missing"; detail: string }
  | { type: "network_error"; detail: { url: string; detail: string } }
  | { type: "git_unavailable" }
  | { type: "agent_not_found"; detail: string }
  | { type: "internal"; detail: string };
