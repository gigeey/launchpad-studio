//! Unit tests for the BashKill tool.

use ao_engine_tools_core::{BackgroundCommandStatus, IoTool, Registry, RunnerContext, ToolOutput};
use serde_json::json;

use super::{prompt, BashKill};
use crate::bash::background::spawn_and_register;
use crate::bash_status::BashStatus;

fn test_ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

// ── schema / registration ────────────────────────────────────────────────────

#[test]
fn description_matches_prompt_constant() {
    assert_eq!(BashKill::default().description(), prompt::DESCRIPTION);
}

#[test]
fn register_bash_kill_lookup_succeeds() {
    let mut r = Registry::new();
    super::register_bash_kill(&mut r);
    assert!(r.lookup_io("BashKill").is_some());
}

#[test]
fn input_schema_is_valid_json() {
    let schema: serde_json::Value =
        serde_json::from_str(prompt::INPUT_SCHEMA).expect("INPUT_SCHEMA is valid JSON");
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["process_id"].is_object());
}

// ── unknown id ───────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn unknown_id_returns_validation_error() {
    let ctx = test_ctx();
    let tool = BashKill;
    let result = tool
        .invoke(json!({"process_id": "bash_99998"}), &ctx)
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("bash_99998"), "error should name the id: {err}");
}

// ── kill a running command ───────────────────────────────────────────────────

/// Covers the bookkeeping only: that a kill leaves the handle and BashStatus
/// agreeing on `killed`. It does NOT observe the OS process — every value it
/// reads was written inside this crate. The name used to claim it terminated
/// the command, which it never checked;
/// `kill_terminates_the_os_process_group_not_just_the_shell` below is the test
/// that actually holds a PID and asks the kernel.
#[cfg(unix)]
#[tokio::test]
async fn kill_marks_handle_and_bash_status_killed() {
    let ctx = test_ctx();

    // Spawn a command that runs indefinitely.
    let (id, _path) = spawn_and_register("sleep 30", &ctx).await.unwrap();

    // Confirm it's Running.
    {
        let handle = ctx.background_commands.get(&id).await.unwrap();
        let st = handle.status.lock().unwrap().clone();
        assert_eq!(st, BackgroundCommandStatus::Running);
    }

    // Kill it.
    let kill_tool = BashKill;
    let kill_out = kill_tool
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    assert!(
        matches!(kill_out, ToolOutput::Structured(_)),
        "kill should return structured success"
    );

    // BashKill returns only after the drain task confirms the child was
    // reaped, so the terminal status is already recorded here.
    {
        let handle = ctx.background_commands.get(&id).await.unwrap();
        let st = handle.status.lock().unwrap().clone();
        assert_eq!(st, BackgroundCommandStatus::Killed);
    }

    // BashStatus should also reflect killed.
    let status_tool = BashStatus;
    // Give drain task a moment to clean up.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let status_out = status_tool
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    if let ToolOutput::Structured(v) = status_out {
        assert_eq!(v["status"], "killed");
    } else {
        panic!("expected structured output from BashStatus");
    }
}

// ── the kill reaches the OS, and reaches the whole process group ─────────────

/// Does `pid` still exist? Signal 0 performs permission/existence checking
/// only — it delivers nothing. `ESRCH` (no such process) is the "gone" answer.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill() with signal 0 has no side effects on the target.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll until `pid` disappears, up to roughly `budget`.
#[cfg(unix)]
async fn wait_until_gone(pid: i32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    !process_alive(pid)
}

/// BashKill must terminate the actual OS processes, not merely update
/// bookkeeping — and it must reach the grandchild, not just the shell.
///
/// This is the assertion the suite was missing. Every other test here reads
/// state that `BashKill` itself wrote a moment earlier, so they pass whether
/// or not a signal is ever delivered. This one holds real PIDs and asks the
/// kernel.
///
/// The command is shaped to put the real work in a grandchild: the shell
/// (`$$`, the process-group leader) backgrounds `sleep` (`$!`) and blocks in
/// `wait`. That is the same shape as `sh -c 'cargo test …'`. Signalling only
/// the direct child would reap the shell and leave `sleep` orphaned and
/// running, so the second assertion below fails unless the signal goes to the
/// process group.
#[cfg(unix)]
#[tokio::test]
async fn kill_terminates_the_os_process_group_not_just_the_shell() {
    let ctx = test_ctx();
    let dir = tempfile::tempdir().unwrap();
    let shell_pid_file = dir.path().join("shell.pid");
    let sleep_pid_file = dir.path().join("sleep.pid");

    let (id, _path) = spawn_and_register(
        &format!(
            "echo $$ > {shell}; sleep 300 & echo $! > {slp}; wait",
            shell = shell_pid_file.display(),
            slp = sleep_pid_file.display(),
        ),
        &ctx,
    )
    .await
    .unwrap();

    // Wait for the shell to publish both PIDs before killing anything.
    let mut shell_pid = None;
    let mut sleep_pid = None;
    for _ in 0..80 {
        if let (Ok(a), Ok(b)) = (
            std::fs::read_to_string(&shell_pid_file),
            std::fs::read_to_string(&sleep_pid_file),
        ) {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                shell_pid = Some(a);
                sleep_pid = Some(b);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let shell_pid = shell_pid.expect("shell never wrote its pid");
    let sleep_pid = sleep_pid.expect("shell never wrote the sleep pid");

    assert!(process_alive(shell_pid), "shell should be alive before the kill");
    assert!(process_alive(sleep_pid), "sleep should be alive before the kill");
    assert_ne!(shell_pid, sleep_pid, "sleep must be a separate process");

    let out = BashKill
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    // The reported status must be a confirmed kill, not an optimistic one.
    match out {
        ToolOutput::Structured(v) => assert_eq!(
            v["status"], "killed",
            "BashKill must report a confirmed kill; got: {v}"
        ),
        other => panic!("expected structured output, got {other:?}"),
    }

    let budget = std::time::Duration::from_secs(10);
    assert!(
        wait_until_gone(shell_pid, budget).await,
        "shell pid {shell_pid} still alive after BashKill reported success"
    );
    assert!(
        wait_until_gone(sleep_pid, budget).await,
        "grandchild pid {sleep_pid} survived BashKill — the signal reached the \
         shell but not the process group, so the real work was orphaned"
    );
}

// ── double-kill is an error ──────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn kill_already_killed_returns_error() {
    let ctx = test_ctx();
    let (id, _) = spawn_and_register("sleep 30", &ctx).await.unwrap();

    let tool = BashKill;
    // First kill — succeeds.
    tool.invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    // Second kill — should be an error, not a panic.
    let out = tool
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "second kill should return an error"
    );
}

// ── killing an already-exited command is an error ────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn kill_exited_command_returns_error() {
    let ctx = test_ctx();
    let (id, _) = spawn_and_register("echo done", &ctx).await.unwrap();

    // Wait for natural exit.
    for _ in 0..40 {
        let handle = ctx.background_commands.get(&id).await.unwrap();
        let st = handle.status.lock().unwrap().clone();
        if matches!(st, BackgroundCommandStatus::Exited { .. }) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    let tool = BashKill;
    let out = tool
        .invoke(json!({"process_id": id.to_string()}), &ctx)
        .await
        .unwrap();

    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "killing an exited command should return an error"
    );
}
