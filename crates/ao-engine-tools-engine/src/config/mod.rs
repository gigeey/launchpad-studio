mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_protocol::{data_root::resolve_data_root, error::AoError};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct Config;

#[async_trait]
impl EngineTool for Config {
    fn name(&self) -> &str {
        "Config"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn mutates_for_input(&self, input: &Value) -> bool {
        input.get("action").and_then(|v| v.as_str()) == Some("set")
    }

    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return Ok(ToolOutput::error("missing required field: action", true)),
        };

        let settings_path = match resolve_data_root() {
            Ok(root) => root.join("settings.json"),
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("failed to resolve data root: {e}"),
                    true,
                ))
            }
        };

        match action {
            "get" => {
                let key = match input.get("key").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => return Ok(ToolOutput::error("get requires a key", true)),
                };
                match load_settings(&settings_path) {
                    Ok(settings) => {
                        let value = settings.get(key).cloned().unwrap_or(Value::Null);
                        Ok(ToolOutput::structured(value))
                    }
                    Err(out) => Ok(out),
                }
            }
            "set" => {
                let key = match input.get("key").and_then(|v| v.as_str()) {
                    Some(k) => k.to_string(),
                    None => return Ok(ToolOutput::error("set requires a key", true)),
                };
                let value = match input.get("value") {
                    Some(v) => v.clone(),
                    None => return Ok(ToolOutput::error("set requires a value", true)),
                };
                let mut settings = match load_settings(&settings_path) {
                    Ok(s) => s,
                    Err(out) => return Ok(out),
                };
                if let Value::Object(ref mut map) = settings {
                    map.insert(key, value);
                }
                write_settings_atomic(&settings_path, &settings)
            }
            "list" => match load_settings(&settings_path) {
                Ok(settings) => {
                    let mut keys: Vec<String> = if let Value::Object(ref map) = settings {
                        map.keys().cloned().collect()
                    } else {
                        vec![]
                    };
                    keys.sort();
                    Ok(ToolOutput::structured(json!({ "keys": keys })))
                }
                Err(out) => Ok(out),
            },
            _ => Ok(ToolOutput::error(
                format!("unknown action '{action}': must be get, set, or list"),
                true,
            )),
        }
    }
}

fn load_settings(path: &std::path::Path) -> Result<Value, ToolOutput> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ToolOutput::error(format!("malformed settings.json: {e}"), true))?;
            if !v.is_object() {
                return Err(ToolOutput::error("settings.json must be a JSON object", true));
            }
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(ToolOutput::error(
            format!("failed to read settings.json: {e}"),
            true,
        )),
    }
}

fn write_settings_atomic(
    path: &std::path::Path,
    settings: &Value,
) -> Result<ToolOutput, AoError> {
    let serialized = match serde_json::to_string_pretty(settings) {
        Ok(s) => s,
        Err(e) => {
            return Ok(ToolOutput::error(
                format!("failed to serialize settings: {e}"),
                true,
            ))
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Ok(ToolOutput::error(
                format!("failed to create settings directory: {e}"),
                true,
            ));
        }
    }
    let tmp_path = path.with_file_name("settings.json.tmp");
    if let Err(e) = std::fs::write(&tmp_path, serialized.as_bytes()) {
        return Ok(ToolOutput::error(
            format!("failed to write settings: {e}"),
            true,
        ));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        return Ok(ToolOutput::error(
            format!("failed to commit settings: {e}"),
            true,
        ));
    }
    Ok(ToolOutput::text("ok"))
}
