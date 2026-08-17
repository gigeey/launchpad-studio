//! Unit tests for the `settings.json` loader.
//!
//! Several tests mutate the process-wide `LAUNCHPAD_STUDIO_DATA_DIR`
//! env var to point the user-global settings source at a tempdir.
//! `cargo test` runs tests in parallel by default, so the crate-wide
//! [`crate::test_env::DataDirGuard`] serializes every test in this
//! binary that touches the variable and restores the prior value on
//! drop, even across a panic.

use std::fs;
use std::path::Path;

use ao_engine_tools_core::PermissionDecision;
use tempfile::tempdir;

use super::config::{
    DEFAULT_CONCURRENT_TOOL_CAP, DEFAULT_DENY_COUNT_THRESHOLD, DEFAULT_HOOK_TIMEOUT_MS,
    RawPermissionRule, SettingsError, load_runner_settings,
};
use crate::test_env::DataDirGuard as EnvGuard;

fn write_json(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

fn project_settings_path(cwd: &Path) -> std::path::PathBuf {
    cwd.join(".launchpad_studio").join("settings.json")
}

fn global_settings_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("settings.json")
}

#[test]
fn defaults_when_both_files_missing() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");
    // Nothing is written under either source.
    let settings = load_runner_settings(cwd.path()).expect("loads with defaults");

    assert_eq!(
        settings.permissions.concurrent_tool_cap,
        DEFAULT_CONCURRENT_TOOL_CAP
    );
    assert_eq!(
        settings.permissions.deny_count_threshold,
        DEFAULT_DENY_COUNT_THRESHOLD
    );
    assert!(settings.permissions.rules.is_empty());
    assert!(settings.hooks.pre_tool_use.is_empty());
    assert!(settings.hooks.post_tool_use.is_empty());

    drop(guard);
}

#[test]
fn project_local_only_flows_through() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let project = project_settings_path(cwd.path());
    write_json(
        &project,
        r#"{
            "permissions": {
                "concurrent_tool_cap": 4,
                "deny_count_threshold": 7,
                "rules": [
                    {"match": "Bash(rm -rf *)", "decision": "deny"}
                ]
            },
            "hooks": {
                "pre_tool_use": [
                    {"match": "Bash(git *)", "command": "echo pre"}
                ]
            }
        }"#,
    );

    let settings = load_runner_settings(cwd.path()).expect("loads project-only");

    assert_eq!(settings.permissions.concurrent_tool_cap, 4);
    assert_eq!(settings.permissions.deny_count_threshold, 7);
    assert_eq!(settings.permissions.rules.len(), 1);
    assert_eq!(settings.permissions.rules[0].r#match, "Bash(rm -rf *)");
    assert_eq!(settings.permissions.rules[0].decision, "deny");

    assert_eq!(settings.hooks.pre_tool_use.len(), 1);
    assert_eq!(settings.hooks.pre_tool_use[0].r#match, "Bash(git *)");
    assert_eq!(settings.hooks.pre_tool_use[0].command, "echo pre");
    // Default timeout applied when the entry omits the field.
    assert_eq!(
        settings.hooks.pre_tool_use[0].timeout_ms,
        DEFAULT_HOOK_TIMEOUT_MS
    );
    assert!(settings.hooks.post_tool_use.is_empty());

    drop(guard);
}

#[test]
fn user_global_only_flows_through() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let global = global_settings_path(guard.data_dir());
    write_json(
        &global,
        r#"{
            "permissions": {
                "concurrent_tool_cap": 6,
                "deny_count_threshold": 2,
                "rules": [
                    {"match": "Read", "decision": "allow"}
                ]
            },
            "hooks": {
                "post_tool_use": [
                    {"match": "Bash", "command": "echo post", "timeout_ms": 1000}
                ]
            }
        }"#,
    );

    let settings = load_runner_settings(cwd.path()).expect("loads global-only");

    assert_eq!(settings.permissions.concurrent_tool_cap, 6);
    assert_eq!(settings.permissions.deny_count_threshold, 2);
    assert_eq!(settings.permissions.rules.len(), 1);
    assert_eq!(settings.permissions.rules[0].decision, "allow");
    assert_eq!(settings.hooks.post_tool_use.len(), 1);
    assert_eq!(settings.hooks.post_tool_use[0].timeout_ms, 1000);

    drop(guard);
}

#[test]
fn project_scalar_overrides_global_and_vecs_concatenate_with_project_first() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let global = global_settings_path(guard.data_dir());
    write_json(
        &global,
        r#"{
            "permissions": {
                "concurrent_tool_cap": 2,
                "deny_count_threshold": 8,
                "rules": [
                    {"match": "GlobalRule", "decision": "ask"}
                ]
            },
            "hooks": {
                "pre_tool_use": [
                    {"match": "Bash(global *)", "command": "echo global-pre"}
                ],
                "post_tool_use": [
                    {"match": "Bash", "command": "echo global-post"}
                ]
            }
        }"#,
    );

    let project = project_settings_path(cwd.path());
    write_json(
        &project,
        r#"{
            "permissions": {
                "concurrent_tool_cap": 12,
                "rules": [
                    {"match": "ProjectRule", "decision": "deny"}
                ]
            },
            "hooks": {
                "pre_tool_use": [
                    {"match": "Bash(project *)", "command": "echo project-pre"}
                ]
            }
        }"#,
    );

    let settings = load_runner_settings(cwd.path()).expect("loads merged");

    // Project-set scalar wins.
    assert_eq!(settings.permissions.concurrent_tool_cap, 12);
    // Project absent → global value flows through (not the default).
    assert_eq!(settings.permissions.deny_count_threshold, 8);

    // Vec concatenation, project entries first.
    let rule_matches: Vec<&str> = settings
        .permissions
        .rules
        .iter()
        .map(|r| r.r#match.as_str())
        .collect();
    assert_eq!(rule_matches, vec!["ProjectRule", "GlobalRule"]);

    let pre_matches: Vec<&str> = settings
        .hooks
        .pre_tool_use
        .iter()
        .map(|h| h.r#match.as_str())
        .collect();
    assert_eq!(pre_matches, vec!["Bash(project *)", "Bash(global *)"]);

    // Project omitted post_tool_use entirely → global's still flows.
    let post_matches: Vec<&str> = settings
        .hooks
        .post_tool_use
        .iter()
        .map(|h| h.r#match.as_str())
        .collect();
    assert_eq!(post_matches, vec!["Bash"]);

    drop(guard);
}

#[test]
fn malformed_project_json_returns_parse_error_naming_path() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let project = project_settings_path(cwd.path());
    write_json(&project, "{ this is not json");

    let err = load_runner_settings(cwd.path()).expect_err("should fail to parse");
    match err {
        SettingsError::Parse { path, .. } => {
            assert_eq!(path, project, "Parse error should name the project path");
        }
        other => panic!("expected SettingsError::Parse, got {other:?}"),
    }

    drop(guard);
}

#[test]
fn malformed_global_json_returns_parse_error_naming_path() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let global = global_settings_path(guard.data_dir());
    write_json(&global, "{ malformed");

    let err = load_runner_settings(cwd.path()).expect_err("should fail to parse");
    match err {
        SettingsError::Parse { path, .. } => {
            assert_eq!(path, global, "Parse error should name the global path");
        }
        other => panic!("expected SettingsError::Parse, got {other:?}"),
    }

    drop(guard);
}

#[test]
fn unknown_decision_string_returns_unknown_decision_error() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    let project = project_settings_path(cwd.path());
    write_json(
        &project,
        r#"{
            "permissions": {
                "rules": [
                    {"match": "Bash", "decision": "maybe"}
                ]
            }
        }"#,
    );

    let err = load_runner_settings(cwd.path()).expect_err("should reject unknown decision");
    match err {
        SettingsError::UnknownDecision { rule, decision } => {
            assert_eq!(rule, "Bash");
            assert_eq!(decision, "maybe");
        }
        other => panic!("expected SettingsError::UnknownDecision, got {other:?}"),
    }

    drop(guard);
}

#[test]
fn raw_rule_to_decision_maps_each_recognised_string() {
    let cases = [
        ("allow", PermissionDecision::Allow),
        ("allow_once", PermissionDecision::AllowOnce),
        ("allow_session", PermissionDecision::AllowSession),
    ];
    for (s, expected) in cases {
        let raw = RawPermissionRule {
            r#match: "Tool".into(),
            decision: s.into(),
        };
        assert_eq!(raw.to_decision().expect("maps"), expected);
    }

    let deny_raw = RawPermissionRule {
        r#match: "Bash(rm -rf *)".into(),
        decision: "deny".into(),
    };
    match deny_raw.to_decision().expect("deny maps") {
        PermissionDecision::Deny { reason } => {
            assert!(reason.contains("Bash(rm -rf *)"));
        }
        other => panic!("expected Deny, got {other:?}"),
    }

    let ask_raw = RawPermissionRule {
        r#match: "Bash".into(),
        decision: "ask".into(),
    };
    match ask_raw.to_decision().expect("ask maps") {
        PermissionDecision::Ask { reason } => {
            assert!(reason.contains("Bash"));
        }
        other => panic!("expected Ask, got {other:?}"),
    }

    let bad_raw = RawPermissionRule {
        r#match: "Bash".into(),
        decision: "yolo".into(),
    };
    assert!(matches!(
        bad_raw.to_decision(),
        Err(SettingsError::UnknownDecision { .. })
    ));
}

#[test]
fn empty_json_object_yields_defaults() {
    let guard = EnvGuard::new();
    let cwd = tempdir().expect("project tempdir");

    write_json(&project_settings_path(cwd.path()), "{}");
    write_json(&global_settings_path(guard.data_dir()), "{}");

    let settings = load_runner_settings(cwd.path()).expect("loads empty objects");

    assert_eq!(
        settings.permissions.concurrent_tool_cap,
        DEFAULT_CONCURRENT_TOOL_CAP
    );
    assert_eq!(
        settings.permissions.deny_count_threshold,
        DEFAULT_DENY_COUNT_THRESHOLD
    );
    assert!(settings.permissions.rules.is_empty());
    assert!(settings.hooks.pre_tool_use.is_empty());
    assert!(settings.hooks.post_tool_use.is_empty());

    drop(guard);
}

// ---------------------------------------------------------------------
// Hook subprocess runner tests.
// ---------------------------------------------------------------------

mod runner {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::fmt::MakeWriter;

    use super::super::config::HookEntry;
    use super::super::{HookOutcome, HookRequest, run_post_hooks, run_pre_hooks};

    fn entry(command: impl Into<String>) -> HookEntry {
        HookEntry {
            r#match: "Bash".into(),
            command: command.into(),
            timeout_ms: 5_000,
        }
    }

    fn entry_with_timeout(command: impl Into<String>, timeout_ms: u64) -> HookEntry {
        HookEntry {
            r#match: "Bash".into(),
            command: command.into(),
            timeout_ms,
        }
    }

    fn request() -> HookRequest {
        HookRequest {
            tool_name: "Bash".into(),
            input: json!({"command": "git push origin main"}),
            agent_id: "agent-test".into(),
            session_id: "session-test".into(),
        }
    }

    /// Capturing writer used by tests that assert on tracing output.
    /// `set_default` returns a thread-scoped guard so two tests running
    /// in parallel on different threads never interfere; the
    /// `current_thread` runtime keeps the spawned task on the same
    /// thread as the guard.
    #[derive(Clone)]
    struct VecMakeWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for VecMakeWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            VecWriter(self.0.clone())
        }
    }

    struct VecWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn install_capturing_subscriber()
    -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = VecMakeWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (buf, guard)
    }

    #[tokio::test]
    async fn pre_hook_emitting_deny_returns_deny_outcome() {
        let e = entry(r#"echo '{"decision":"deny","reason":"nope"}'"#);
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        match outcome {
            HookOutcome::Deny { reason } => assert_eq!(reason, "nope"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pre_hook_emitting_mutate_returns_updated_input() {
        let e =
            entry(r#"echo '{"decision":"mutate","updated_input":{"file_path":"/tmp/x"}}'"#);
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        match outcome {
            HookOutcome::Mutate { updated_input } => {
                assert_eq!(updated_input, json!({"file_path": "/tmp/x"}));
            }
            other => panic!("expected Mutate, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_hook_that_sleeps_past_timeout_is_killed_with_warn_trace() {
        let (buf, guard) = install_capturing_subscriber();

        // 60ms timeout, command sleeps 5s. With kill_on_drop the child
        // is SIGKILLed when the wait future is dropped on timeout.
        let e = entry_with_timeout("sleep 5", 60);
        let start = Instant::now();
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        let elapsed = start.elapsed();

        assert!(matches!(outcome, HookOutcome::Continue));
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout did not promptly fire (elapsed = {elapsed:?})"
        );

        drop(guard);
        let captured = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
        assert!(
            captured.contains("timed out"),
            "expected 'timed out' in tracing output; got: {captured}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_hook_with_non_zero_exit_emits_stderr_via_tracing() {
        let (buf, guard) = install_capturing_subscriber();

        // Marker is a unique substring so we can assert it surfaced.
        let e = entry("echo specific-failure-marker >&2; exit 1");
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;

        // Empty stdout → outcome is Continue regardless of stderr.
        assert!(matches!(outcome, HookOutcome::Continue));

        drop(guard);
        let captured = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
        assert!(
            captured.contains("specific-failure-marker"),
            "stderr text was not captured by tracing; got: {captured}"
        );
    }

    #[tokio::test]
    async fn first_non_continue_outcome_short_circuits_remaining_pre_hooks() {
        let tmp = tempdir().expect("tempdir");
        let beacon = tmp.path().join("beacon");
        let beacon_str = beacon.to_string_lossy().replace('\'', "\\'");

        let first = entry(r#"echo '{"decision":"allow"}'"#);
        // Touches a beacon file IFF this hook runs. The test asserts
        // the file does NOT exist after the run, proving the second
        // hook never executed.
        let second_cmd = format!("touch '{}'; exit 99", beacon_str);
        let second = entry(second_cmd);

        let outcome =
            run_pre_hooks(&[&first, &second], &request(), CancellationToken::new()).await;

        assert!(matches!(outcome, HookOutcome::Allow));
        assert!(
            !beacon.exists(),
            "second hook should be skipped after first non-Continue"
        );
    }

    #[tokio::test]
    async fn cancellation_mid_run_kills_child_within_100ms() {
        // Long-running hook; the outer cancel fires after 20ms.
        let e = entry_with_timeout("sleep 10", 60_000);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_clone.cancel();
        });

        let start = Instant::now();
        let outcome = run_pre_hooks(&[&e], &request(), cancel).await;
        let elapsed = start.elapsed();

        assert!(matches!(outcome, HookOutcome::Continue));
        assert!(
            elapsed < Duration::from_millis(150),
            "cancellation did not promptly kill child (elapsed = {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn empty_stdout_falls_through_to_continue() {
        let e = entry("true");
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        assert!(matches!(outcome, HookOutcome::Continue));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_stdout_falls_through_to_continue_with_warn() {
        let (buf, guard) = install_capturing_subscriber();
        let e = entry("echo not-json-at-all");
        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        assert!(matches!(outcome, HookOutcome::Continue));

        drop(guard);
        let captured = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
        assert!(
            captured.contains("did not parse"),
            "expected parse-failure warn in tracing; got: {captured}"
        );
    }

    #[tokio::test]
    async fn post_hooks_all_run_and_outcomes_are_ignored() {
        let tmp = tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let a_str = a.to_string_lossy().replace('\'', "\\'");
        let b_str = b.to_string_lossy().replace('\'', "\\'");

        // Both hooks emit Deny — but post-hooks discard outcomes, so
        // BOTH must still run (proven by both beacon files existing).
        let first = entry(format!(
            "touch '{}'; echo '{{\"decision\":\"deny\",\"reason\":\"x\"}}'",
            a_str
        ));
        let second = entry(format!(
            "touch '{}'; echo '{{\"decision\":\"deny\",\"reason\":\"y\"}}'",
            b_str
        ));

        run_post_hooks(&[&first, &second], &request()).await;

        assert!(a.exists(), "first post-hook must run");
        assert!(
            b.exists(),
            "second post-hook must run even after first denied"
        );
    }

    #[tokio::test]
    async fn empty_pre_hook_list_returns_continue() {
        let outcome = run_pre_hooks(&[], &request(), CancellationToken::new()).await;
        assert!(matches!(outcome, HookOutcome::Continue));
    }

    #[tokio::test]
    async fn hook_sees_request_json_on_stdin() {
        let tmp = tempdir().expect("tempdir");
        let captured = tmp.path().join("captured.json");
        let captured_str = captured.to_string_lossy().replace('\'', "\\'");
        // Read stdin, save to a file, then emit allow on stdout.
        let cmd = format!(
            "cat > '{}'; echo '{{\"decision\":\"allow\"}}'",
            captured_str
        );
        let e = entry(cmd);

        let outcome = run_pre_hooks(&[&e], &request(), CancellationToken::new()).await;
        assert!(matches!(outcome, HookOutcome::Allow));

        let bytes = std::fs::read(&captured).expect("read captured stdin");
        let received: Value = serde_json::from_slice(&bytes).expect("parse stdin JSON");
        assert_eq!(received["tool_name"], "Bash");
        assert_eq!(received["agent_id"], "agent-test");
        assert_eq!(received["session_id"], "session-test");
        assert_eq!(received["input"]["command"], "git push origin main");
    }
}

// ---------------------------------------------------------------------------
// tool_load_overrides tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tool_load_overrides_tests {
    use ao_engine_tools_core::LoadPolicyOverride;
    use tempfile::tempdir;

    use super::{EnvGuard, global_settings_path, project_settings_path, write_json};
    use crate::hooks::config::load_runner_settings;

    #[test]
    fn empty_settings_yields_empty_overrides() {
        let guard = EnvGuard::new();
        let cwd = tempdir().expect("project tempdir");
        // Global settings has no tool_load_overrides key.
        write_json(
            &global_settings_path(guard.data_dir()),
            r#"{ "permissions": { "rules": [] }, "hooks": {} }"#,
        );
        let settings = load_runner_settings(cwd.path()).expect("load");
        assert!(settings.tool_load_overrides.is_empty());
        drop(guard);
    }

    #[test]
    fn project_local_wins_over_global_on_same_key() {
        let guard = EnvGuard::new();
        let cwd = tempdir().expect("project tempdir");

        write_json(
            &global_settings_path(guard.data_dir()),
            r#"{ "tool_load_overrides": { "TodoWrite": "always_load" } }"#,
        );
        write_json(
            &project_settings_path(cwd.path()),
            r#"{ "tool_load_overrides": { "TodoWrite": "deferred" } }"#,
        );

        let settings = load_runner_settings(cwd.path()).expect("load");
        assert_eq!(
            settings.tool_load_overrides.get("TodoWrite"),
            Some(&LoadPolicyOverride::ForceDeferred),
            "project-local should override global"
        );
        drop(guard);
    }

    #[test]
    fn global_fills_in_keys_absent_from_project() {
        let guard = EnvGuard::new();
        let cwd = tempdir().expect("project tempdir");

        write_json(
            &global_settings_path(guard.data_dir()),
            r#"{ "tool_load_overrides": { "AskUserQuestionWithForm": "always_load" } }"#,
        );
        write_json(
            &project_settings_path(cwd.path()),
            r#"{ "tool_load_overrides": { "TodoWrite": "deferred" } }"#,
        );

        let settings = load_runner_settings(cwd.path()).expect("load");
        assert_eq!(
            settings.tool_load_overrides.get("AskUserQuestionWithForm"),
            Some(&LoadPolicyOverride::ForceAlwaysLoad)
        );
        assert_eq!(
            settings.tool_load_overrides.get("TodoWrite"),
            Some(&LoadPolicyOverride::ForceDeferred)
        );
        drop(guard);
    }

    #[test]
    fn invalid_value_strings_are_skipped() {
        let guard = EnvGuard::new();
        let cwd = tempdir().expect("project tempdir");

        write_json(
            &global_settings_path(guard.data_dir()),
            r#"{ "tool_load_overrides": { "TodoWrite": "bogus_value", "AskUserQuestionWithForm": "always_load" } }"#,
        );

        let settings = load_runner_settings(cwd.path()).expect("load");
        assert!(!settings.tool_load_overrides.contains_key("TodoWrite"));
        assert_eq!(
            settings.tool_load_overrides.get("AskUserQuestionWithForm"),
            Some(&LoadPolicyOverride::ForceAlwaysLoad)
        );
        drop(guard);
    }

    #[test]
    fn absent_key_contributes_empty_map() {
        let guard = EnvGuard::new();
        let cwd = tempdir().expect("project tempdir");
        // Global file exists but has no tool_load_overrides key.
        write_json(&global_settings_path(guard.data_dir()), r#"{}"#);
        let settings = load_runner_settings(cwd.path()).expect("load");
        assert!(settings.tool_load_overrides.is_empty());
        drop(guard);
    }
}
