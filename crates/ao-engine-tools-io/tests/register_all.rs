//! Integration test for the caller pattern.
//!
//! Builds a [`Registry`], calls [`ao_engine_tools_io::register_all`], looks
//! each IO tool up by name, invokes it against a tempfile fixture,
//! and asserts the [`ToolOutput`] shape — the contract that external
//! callers (engine session bootstrap, integration tests on `main`) depend
//! on.
//!
//! Also asserts the description-equals-`prompt::DESCRIPTION` invariant per
//! tool to prevent drift between `mod.rs` and `prompt.rs`.

use std::sync::Arc;

use ao_engine_tools_core::{Registry, RunnerContext, ToolOutput};
use ao_engine_tools_io::{bash, bash_kill, bash_status, edit, glob, grep, notebook_edit, read, register_all, write};
use jsonschema::Validator;
use serde_json::json;
use tempfile::TempDir;

fn ctx_with_registry(registry: Registry) -> RunnerContext {
    RunnerContext::new("session-id", "agent-id")
        .unwrap()
        .with_registry(Arc::new(registry))
}

fn registry_with_all_io() -> Registry {
    let mut r = Registry::new();
    register_all(&mut r);
    r
}

#[test]
fn register_all_installs_read_glob_grep_by_name() {
    let r = registry_with_all_io();
    assert!(r.lookup_io("Read").is_some());
    assert!(r.lookup_io("Glob").is_some());
    assert!(r.lookup_io("Grep").is_some());
    assert!(r.lookup_io("Edit").is_some());
    assert!(r.lookup_io("Write").is_some());
    assert!(r.lookup_io("NotebookEdit").is_some());
    assert!(r.lookup_io("Bash").is_some());
    assert!(r.lookup_io("BashStatus").is_some());
    assert!(r.lookup_io("BashKill").is_some());
    assert_eq!(r.len(), 9);
}

#[test]
fn each_tool_input_schema_validates() {
    let r = registry_with_all_io();
    for name in [
        "Read",
        "Glob",
        "Grep",
        "Edit",
        "Write",
        "NotebookEdit",
        "Bash",
        "BashStatus",
        "BashKill",
    ] {
        let tool = r
            .lookup_io(name)
            .unwrap_or_else(|| panic!("{name} should be registered"));
        let schema = tool.input_schema();
        Validator::new(&schema).unwrap_or_else(|e| panic!("{name} schema must compile: {e}"));
    }
}

#[test]
fn each_tool_description_matches_prompt_constant() {
    let r = registry_with_all_io();

    let read_tool = r.lookup_io("Read").unwrap();
    assert_eq!(read_tool.description(), read::prompt::DESCRIPTION);
    assert!(!read_tool.description().is_empty());

    let glob_tool = r.lookup_io("Glob").unwrap();
    assert_eq!(glob_tool.description(), glob::prompt::DESCRIPTION);
    assert!(!glob_tool.description().is_empty());

    let grep_tool = r.lookup_io("Grep").unwrap();
    assert_eq!(grep_tool.description(), grep::prompt::DESCRIPTION);
    assert!(!grep_tool.description().is_empty());

    let edit_tool = r.lookup_io("Edit").unwrap();
    assert_eq!(edit_tool.description(), edit::prompt::DESCRIPTION);
    assert!(!edit_tool.description().is_empty());

    let write_tool = r.lookup_io("Write").unwrap();
    assert_eq!(write_tool.description(), write::prompt::DESCRIPTION);
    assert!(!write_tool.description().is_empty());

    let notebook_edit_tool = r.lookup_io("NotebookEdit").unwrap();
    assert_eq!(
        notebook_edit_tool.description(),
        notebook_edit::prompt::DESCRIPTION
    );
    assert!(!notebook_edit_tool.description().is_empty());

    let bash_tool = r.lookup_io("Bash").unwrap();
    assert_eq!(bash_tool.description(), bash::prompt::DESCRIPTION);
    assert!(!bash_tool.description().is_empty());

    let bash_status_tool = r.lookup_io("BashStatus").unwrap();
    assert_eq!(bash_status_tool.description(), bash_status::prompt::DESCRIPTION);
    assert!(!bash_status_tool.description().is_empty());

    let bash_kill_tool = r.lookup_io("BashKill").unwrap();
    assert_eq!(bash_kill_tool.description(), bash_kill::prompt::DESCRIPTION);
    assert!(!bash_kill_tool.description().is_empty());
}

#[tokio::test]
async fn read_dispatches_via_registry_against_tempfile() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("hello.txt");
    tokio::fs::write(&file, "alpha\nbeta\n").await.unwrap();

    let registry = registry_with_all_io();
    let tool = registry.lookup_io("Read").expect("Read registered");
    let ctx = ctx_with_registry(registry_with_all_io());
    let out = tool
        .invoke(json!({"file_path": file.to_str().unwrap()}), &ctx)
        .await
        .expect("invoke should succeed");

    match out {
        ToolOutput::Text(s) => {
            assert!(
                s.contains("alpha"),
                "expected output to contain 'alpha': {s}"
            );
            assert!(s.contains("beta"), "expected output to contain 'beta': {s}");
        }
        other => panic!("expected ToolOutput::Text from Read, got {other:?}"),
    }
}

#[tokio::test]
async fn glob_dispatches_via_registry_against_tempdir() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "fn main() {}")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("b.rs"), "fn other() {}")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("c.txt"), "ignore me")
        .await
        .unwrap();

    let registry = registry_with_all_io();
    let tool = registry.lookup_io("Glob").expect("Glob registered");
    let ctx = ctx_with_registry(registry_with_all_io());
    let out = tool
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx,
        )
        .await
        .expect("invoke should succeed");

    // Glob now emits ToolOutput::Structured with a text_fallback field that
    // is byte-identical to the old Text rendering.
    let text = match out {
        ToolOutput::Structured(ref v) => v["text_fallback"]
            .as_str()
            .expect("text_fallback must be a string")
            .to_string(),
        ToolOutput::Text(s) => s,
        other => panic!("expected ToolOutput::Structured from Glob, got {other:?}"),
    };
    assert!(text.contains("a.rs"), "output should contain a.rs: {text}");
    assert!(text.contains("b.rs"), "output should contain b.rs: {text}");
    assert!(
        !text.contains("c.txt"),
        "output should not contain c.txt: {text}"
    );
}

#[tokio::test]
async fn grep_dispatches_via_registry_against_tempdir() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "alpha\nfoo bar\nbeta\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("b.txt"), "no match here\n")
        .await
        .unwrap();

    let registry = registry_with_all_io();
    let tool = registry.lookup_io("Grep").expect("Grep registered");
    let ctx = ctx_with_registry(registry_with_all_io());
    let out = tool
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "files_with_matches",
            }),
            &ctx,
        )
        .await
        .expect("invoke should succeed");

    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("a.txt"), "expected match for a.txt: {s}");
            assert!(!s.contains("b.txt"), "b.txt should not match: {s}");
        }
        other => panic!("expected ToolOutput::Text from Grep, got {other:?}"),
    }
}

// ── Background lifecycle end-to-end ──────────────────────────────────────────

/// Full lifecycle: spawn a short-lived background command, poll BashStatus while
/// it is still running, confirm it exits with code 0, and verify the disk output
/// file holds the complete output produced by the command.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn background_lifecycle_full_cycle() {
    let tmp = TempDir::new().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = ctx_with_registry(registry_with_all_io());
    let bash = ctx.registry.lookup_io("Bash").unwrap();
    let status_tool = ctx.registry.lookup_io("BashStatus").unwrap();

    // Command emits three distinct lines then sleeps before exiting so the first
    // BashStatus poll reliably observes the Running state. The sleep is 2 s rather
    // than a few hundred ms because two later assertions race it: the spawn-latency
    // bound below, and the first poll having to land while the command still runs.
    let t0 = std::time::Instant::now();
    let invoke_result = bash
        .invoke(
            json!({
                "command": "printf 'alpha\\nbeta\\ngamma\\n'; sleep 2",
                "run_in_background": true,
            }),
            &ctx,
        )
        .await
        .unwrap();
    let spawn_elapsed = t0.elapsed();

    // A spawn that wrongly blocks returns when the command completes, at ~2 s. The
    // bound is set against that, not against the ~1 ms a non-blocking spawn takes.
    assert!(
        spawn_elapsed.as_millis() < 1_000,
        "Bash background must return without blocking; took {spawn_elapsed:?}"
    );

    let payload = match &invoke_result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured from Bash background, got: {other:?}"),
    };

    let process_id = payload["process_id"].as_str().expect("process_id string").to_string();
    let output_path_str = payload["output_path"].as_str().expect("output_path string").to_string();

    assert!(
        process_id.starts_with("bash_"),
        "process_id format unexpected: {process_id}"
    );
    assert!(
        output_path_str.ends_with(".log"),
        "output_path must be a .log file: {output_path_str}"
    );

    // Release the env var — paths are already resolved in handles we hold.
    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    // First BashStatus poll while the command is still sleeping.
    let first_out = status_tool
        .invoke(json!({"process_id": &process_id}), &ctx)
        .await
        .unwrap();
    let first_status = match &first_out {
        ToolOutput::Structured(v) => v["status"].as_str().unwrap_or("").to_string(),
        other => panic!("expected Structured from BashStatus, got: {other:?}"),
    };
    assert_eq!(first_status, "running", "first poll must report running");

    // Poll until the command exits (up to ~6 s, against a 2 s sleep).
    let mut final_status = first_status;
    let mut final_output = String::new();
    for _ in 0..60 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let out = status_tool
            .invoke(json!({"process_id": &process_id}), &ctx)
            .await
            .unwrap();
        if let ToolOutput::Structured(v) = out {
            final_status = v["status"].as_str().unwrap_or("").to_string();
            final_output = v["output"].as_str().unwrap_or("").to_string();
            if final_status.starts_with("exited") {
                break;
            }
        }
    }

    assert_eq!(
        final_status, "exited:0",
        "command must exit with code 0; got: {final_status}"
    );
    for marker in ["alpha", "beta", "gamma"] {
        assert!(
            final_output.contains(marker),
            "BashStatus output must contain '{marker}': {final_output:?}"
        );
    }

    // Read the disk output file directly and confirm it holds the complete output.
    let disk = tokio::fs::read_to_string(&output_path_str)
        .await
        .unwrap_or_else(|e| panic!("disk output file must exist at {output_path_str}: {e}"));
    for marker in ["alpha", "beta", "gamma"] {
        assert!(
            disk.contains(marker),
            "disk file must contain '{marker}': {disk:?}"
        );
    }
}

/// Kill a long-running background command via BashKill, then confirm BashStatus
/// reports killed and no second kill is possible (proves the process is not leaked).
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn background_kill_lifecycle() {
    use ao_engine_tools_core::BackgroundCommandStatus;

    let tmp = TempDir::new().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = ctx_with_registry(registry_with_all_io());
    let bash = ctx.registry.lookup_io("Bash").unwrap();
    let kill_tool = ctx.registry.lookup_io("BashKill").unwrap();
    let status_tool = ctx.registry.lookup_io("BashStatus").unwrap();

    // Spawn a command that will not exit on its own within the test window.
    let invoke_result = bash
        .invoke(
            json!({ "command": "sleep 30", "run_in_background": true }),
            &ctx,
        )
        .await
        .unwrap();
    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let process_id = match &invoke_result {
        ToolOutput::Structured(v) => v["process_id"].as_str().expect("process_id").to_string(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // Kill it through the BashKill tool.
    let kill_out = kill_tool
        .invoke(json!({"process_id": &process_id}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(kill_out, ToolOutput::Structured(_)),
        "BashKill must return Structured on success, got: {kill_out:?}"
    );

    // BashStatus must report killed immediately — status is set atomically by
    // BashKill before it signals the drain task.
    let status_out = status_tool
        .invoke(json!({"process_id": &process_id}), &ctx)
        .await
        .unwrap();
    if let ToolOutput::Structured(v) = &status_out {
        assert_eq!(
            v["status"].as_str(),
            Some("killed"),
            "BashStatus must report killed immediately after BashKill"
        );
    } else {
        panic!("expected Structured from BashStatus, got: {status_out:?}");
    }

    // Allow the drain task time to send SIGKILL and reap the child.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Verify via the handle that the child has been reaped.
    let ids = ctx.background_commands.list().await;
    let handle = ctx.background_commands.get(&ids[0]).await.unwrap();
    let st = handle.status.lock().unwrap().clone();
    assert_eq!(
        st,
        BackgroundCommandStatus::Killed,
        "handle status must be Killed after drain completes"
    );

    // A second BashKill on the same id must fail — proves the process is not
    // still running and accepting kill signals.
    let second_kill = kill_tool
        .invoke(json!({"process_id": &process_id}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(second_kill, ToolOutput::Error { .. }),
        "second kill must return an error, got: {second_kill:?}"
    );
}

/// BashStatus and BashKill on an unknown id must return clean errors that name
/// the missing id, making them diagnosable and retryable by the caller.
#[cfg(unix)]
#[tokio::test]
async fn background_unknown_id_returns_clean_errors() {
    let ctx = ctx_with_registry(registry_with_all_io());
    let status_tool = ctx.registry.lookup_io("BashStatus").unwrap();
    let kill_tool = ctx.registry.lookup_io("BashKill").unwrap();

    let bogus_id = "bash_00000";

    // BashStatus on unknown id must return a validation error naming the id.
    let status_result = status_tool
        .invoke(json!({"process_id": bogus_id}), &ctx)
        .await;
    assert!(
        status_result.is_err(),
        "BashStatus on unknown id must return Err"
    );
    let err = status_result.unwrap_err().to_string();
    assert!(
        err.contains(bogus_id),
        "BashStatus error must name the unknown id: {err}"
    );

    // BashKill on unknown id must also return a validation error.
    let kill_result = kill_tool
        .invoke(json!({"process_id": bogus_id}), &ctx)
        .await;
    assert!(
        kill_result.is_err(),
        "BashKill on unknown id must return Err"
    );
    let err = kill_result.unwrap_err().to_string();
    assert!(
        err.contains(bogus_id),
        "BashKill error must name the unknown id: {err}"
    );
}

/// BashStatus and BashKill are registered in the same `register_all` assembly
/// point as Bash — exercised end-to-end via the registry the lifecycle tests use.
#[test]
fn bash_status_and_kill_coregistered_with_bash() {
    let r = registry_with_all_io();
    assert!(r.lookup_io("Bash").is_some(), "Bash must be registered");
    assert!(r.lookup_io("BashStatus").is_some(), "BashStatus must be registered");
    assert!(r.lookup_io("BashKill").is_some(), "BashKill must be registered");
}
