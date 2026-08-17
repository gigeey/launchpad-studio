use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub preferred_name: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default = "default_language")]
    pub language: Option<String>,
    #[serde(default = "default_locale")]
    pub locale: Option<String>,
    /// How many hours before a scheduled task the sleep guard should activate.
    /// Default is 4.0 hours. Set to None to disable the sleep guard entirely.
    ///
    /// Deliberately NOT `skip_serializing_if`: combined with the non-`None`
    /// `default` below, skipping `None` on serialize would make an explicit
    /// "disabled" choice indistinguishable on disk from "field absent" —
    /// deserializing it back would silently re-apply the `Some(4.0)` default
    /// and re-enable the guard the user just turned off.
    #[serde(default = "default_max_sleep_guard_hours")]
    pub max_sleep_guard_hours: Option<f64>,
    /// Whether to prevent system sleep while any workflow task is active
    /// (running, ready, or in backoff). Defaults to true.
    #[serde(default = "default_prevent_sleep_during_workflows")]
    pub prevent_sleep_during_workflows: bool,
    /// Whether to prevent system sleep while any agent run is in flight
    /// (across all agents, including team and synthetic phase agents).
    /// Defaults to true.
    #[serde(default = "default_prevent_sleep_during_agent_runs")]
    pub prevent_sleep_during_agent_runs: bool,
    /// Whether to prevent system sleep while any tasklist task is queued or
    /// in-flight in a `TasklistQueueManager`. Defaults to true.
    #[serde(default = "default_prevent_sleep_during_tasklists")]
    pub prevent_sleep_during_tasklists: bool,
    /// Whether an active sleep guard should also keep the display (screen)
    /// on, rather than only preventing system/CPU sleep. Defaults to false —
    /// the screen may still turn off while a guarded task keeps running to
    /// completion. This only changes the assertion type used while a guard
    /// above is held; it does not decide whether a guard is held at all.
    #[serde(default = "default_keep_display_awake")]
    pub keep_display_awake: bool,
    /// Filename patterns matched (case-insensitively) against files at the
    /// root of an agent's home to discover instruction files. Defaults to
    /// `["CLAUDE.md"]`; users can add `Cursor.md`, `alwaysoninstructions.md`,
    /// etc. from the Instructions tab.
    #[serde(default = "default_instruction_filenames")]
    pub instruction_filenames: Vec<String>,
    /// Which [`AgentProfile`](crate::agent::AgentProfile) the reflection pass
    /// should drive when proposing candidate memories/skills —
    /// lets the user point distillation at a cheaper/simpler agent than the
    /// one that actually ran the turn. `None` (the default) falls back to
    /// the thread's own agent profile. Exposed as-is through
    /// `GET`/`PUT /preferences` since those routes round-trip the whole
    /// struct.
    // TODO(settings-ui): add a Settings control that binds to this field —
    // an agent picker, defaulting to "use the thread's own agent".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflection_agent_id: Option<AgentId>,
}

fn default_language() -> Option<String> {
    Some("en".to_string())
}

fn default_locale() -> Option<String> {
    Some("en-US".to_string())
}

fn default_max_sleep_guard_hours() -> Option<f64> {
    Some(4.0)
}

fn default_prevent_sleep_during_workflows() -> bool {
    true
}

fn default_prevent_sleep_during_agent_runs() -> bool {
    true
}

fn default_prevent_sleep_during_tasklists() -> bool {
    true
}

fn default_keep_display_awake() -> bool {
    false
}

fn default_instruction_filenames() -> Vec<String> {
    vec!["CLAUDE.md".to_string()]
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            full_name: None,
            preferred_name: None,
            timezone: None,
            language: default_language(),
            locale: default_locale(),
            max_sleep_guard_hours: default_max_sleep_guard_hours(),
            prevent_sleep_during_workflows: default_prevent_sleep_during_workflows(),
            prevent_sleep_during_agent_runs: default_prevent_sleep_during_agent_runs(),
            prevent_sleep_during_tasklists: default_prevent_sleep_during_tasklists(),
            keep_display_awake: default_keep_display_awake(),
            instruction_filenames: default_instruction_filenames(),
            reflection_agent_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_filenames_defaults_when_missing() {
        let json_missing = r#"{"full_name":"Test"}"#;
        let parsed: UserPreferences = serde_json::from_str(json_missing).unwrap();
        assert_eq!(parsed.instruction_filenames, vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn test_instruction_filenames_round_trip() {
        let prefs = UserPreferences {
            instruction_filenames: vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()],
            ..Default::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: UserPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.instruction_filenames,
            vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]
        );
    }

    #[test]
    fn test_default_prefs_has_claude_md() {
        let prefs = UserPreferences::default();
        assert_eq!(prefs.instruction_filenames, vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn test_reflection_agent_id_defaults_to_none_when_missing() {
        let json_missing = r#"{"full_name":"Test"}"#;
        let parsed: UserPreferences = serde_json::from_str(json_missing).unwrap();
        assert_eq!(parsed.reflection_agent_id, None);
        assert_eq!(UserPreferences::default().reflection_agent_id, None);
    }

    #[test]
    fn test_reflection_agent_id_round_trips_when_set() {
        let prefs = UserPreferences {
            reflection_agent_id: Some("cheap-reflector".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains("cheap-reflector"));
        let parsed: UserPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reflection_agent_id, Some("cheap-reflector".to_string()));

        // None is omitted from the wire format (skip_serializing_if).
        let json_none = serde_json::to_string(&UserPreferences::default()).unwrap();
        assert!(!json_none.contains("reflection_agent_id"));
    }
}
