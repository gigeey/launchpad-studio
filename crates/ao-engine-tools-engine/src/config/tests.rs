use super::*;
use ao_engine_tools_core::RunnerContext;
use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use serde_json::json;
use std::path::PathBuf;

// Use the crate-wide mutex to serialise all tests that mutate the
// process-global LAUNCHPAD_STUDIO_DATA_DIR env var (shared with skill tests).
use crate::lock_env_var;

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

// Sets DATA_DIR_ENV_VAR to `dir` for the duration of the returned guard.
// The caller must hold `ENV_MUTEX` before calling this.
#[allow(deprecated)]
fn set_data_dir(dir: &std::path::Path) {
    std::env::set_var(DATA_DIR_ENV_VAR, dir);
}

#[allow(deprecated)]
fn clear_data_dir() {
    std::env::remove_var(DATA_DIR_ENV_VAR);
}

#[tokio::test]
async fn get_missing_file_returns_null_not_error() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "get", "key": "foo"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v, Value::Null),
        other => panic!("expected Structured(Null), got {other:?}"),
    }
}

#[tokio::test]
async fn get_existing_key_returns_value() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    // Pre-populate settings.json
    let settings_path = dir.path().join("settings.json");
    std::fs::write(&settings_path, r#"{"theme":"dark","width":1280}"#).unwrap();

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "get", "key": "theme"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v, json!("dark")),
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn get_absent_key_returns_null() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let settings_path = dir.path().join("settings.json");
    std::fs::write(&settings_path, r#"{"a":1}"#).unwrap();

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "get", "key": "missing"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v, Value::Null),
        other => panic!("expected Structured(Null), got {other:?}"),
    }
}

#[tokio::test]
async fn set_writes_atomically_and_tmp_is_removed() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "set", "key": "foo", "value": 42}), &ctx)
        .await
        .unwrap();

    let settings_path = dir.path().join("settings.json");
    let tmp_path = dir.path().join("settings.json.tmp");
    clear_data_dir();

    match out {
        ToolOutput::Text(s) => assert_eq!(s, "ok"),
        other => panic!("expected Text(ok), got {other:?}"),
    }

    let contents = std::fs::read_to_string(&settings_path).unwrap();
    let v: Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(v["foo"], json!(42));
    assert!(!tmp_path.exists(), "tmp file should have been removed");
}

#[tokio::test]
async fn set_then_get_round_trips() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    Config
        .invoke(json!({"action": "set", "key": "theme", "value": "dark"}), &ctx)
        .await
        .unwrap();
    let out = Config
        .invoke(json!({"action": "get", "key": "theme"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v, json!("dark")),
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn list_returns_sorted_keys() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let settings_path = dir.path().join("settings.json");
    std::fs::write(&settings_path, r#"{"zebra":1,"apple":2,"mango":3}"#).unwrap();

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["keys"], json!(["apple", "mango", "zebra"]));
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn list_empty_file_returns_empty_list() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let settings_path = dir.path().join("settings.json");
    std::fs::write(&settings_path, "{}").unwrap();

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v["keys"], json!([])),
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn list_missing_file_returns_empty_list() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Structured(v) => assert_eq!(v["keys"], json!([])),
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_action_returns_error() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "delete", "key": "x"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn get_missing_key_field_returns_error_no_disk_write() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "get"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(!dir.path().join("settings.json").exists());
}

#[tokio::test]
async fn set_missing_key_field_returns_error_no_disk_write() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "set", "value": 1}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(!dir.path().join("settings.json").exists());
}

#[tokio::test]
async fn set_missing_value_field_returns_error_no_disk_write() {
    let _guard = lock_env_var();
    let dir = tempfile::tempdir().unwrap();
    set_data_dir(dir.path());

    let ctx = make_ctx();
    let out = Config
        .invoke(json!({"action": "set", "key": "foo"}), &ctx)
        .await
        .unwrap();
    clear_data_dir();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(!dir.path().join("settings.json").exists());
}

#[test]
fn tool_name_is_config() {
    assert_eq!(Config.name(), "Config");
}

#[test]
fn is_not_concurrency_safe() {
    assert!(!Config.is_concurrency_safe());
}

#[test]
fn mutates_for_input_true_only_for_set_action() {
    assert!(Config.mutates_for_input(&json!({"action": "set", "key": "x", "value": 1})));
    assert!(!Config.mutates_for_input(&json!({"action": "get", "key": "x"})));
    assert!(!Config.mutates_for_input(&json!({"action": "list"})));
    assert!(!Config.mutates_for_input(&json!({})));
    assert!(!Config.mutates_for_input(&json!({"action": "unknown"})));
}

#[test]
fn lookup_through_registry() {
    use ao_engine_tools_core::Registry;
    use std::sync::Arc;
    let mut r = Registry::new();
    r.register_engine(Arc::new(Config));
    assert!(r.lookup_engine("Config").is_some());
}
